//! RBAC applicatif row-level — évaluation par enregistrement ([`can_read`](crate::rbac::can_read)/[`can_write`](crate::rbac::can_write)),
//! création ([`can_create`](crate::rbac::can_create)) et filtrage de liste ([`ListAccess`](crate::rbac::ListAccess)).
//!
//! Admin gagne toujours. `OwnerOnly` sans `owner_column` → refus (fail-closed).

use sea_orm::entity::prelude::*;
use sea_orm::{Condition, DatabaseConnection, ModelTrait};

use crate::resource::{AccessPolicy, MiryadResource};
use crate::users::group::{is_admin, is_member};
use crate::users::user;

/// `record` doit déjà être chargé — cette fonction ne fait pas de requête pour le récupérer,
/// elle évalue une politique contre un enregistrement en main (cf. feature 4 pour le filtrage de
/// liste, hors-scope ici).
///
/// # Errors
///
/// Propage toute [`DbErr`] de [`is_admin`] ou [`is_member`] : une panne d'infrastructure n'est
/// jamais dégradée en refus silencieux.
pub async fn can_read<E>(
    db: &DatabaseConnection,
    user: &user::Model,
    record: &E::Model,
) -> Result<bool, DbErr>
where
    E: MiryadResource,
    E::Model: ModelTrait<Entity = E>,
{
    evaluate::<E>(db, E::read_policy(), E::owner_column(), user, record).await
}

/// Évalue la politique d'écriture (`write_policy`) contre un enregistrement déjà chargé, même
/// table de décision que [`can_read`] avec la politique lue en différence.
///
/// # Errors
///
/// Propage toute [`DbErr`] de [`is_admin`] ou [`is_member`] : une panne d'infrastructure n'est
/// jamais dégradée en refus silencieux.
pub async fn can_write<E>(
    db: &DatabaseConnection,
    user: &user::Model,
    record: &E::Model,
) -> Result<bool, DbErr>
where
    E: MiryadResource,
    E::Model: ModelTrait<Entity = E>,
{
    evaluate::<E>(db, E::write_policy(), E::owner_column(), user, record).await
}

/// Autorisation de créer un nouvel enregistrement (feature 4) — il n'y en a pas encore à
/// comparer, donc pas de vérification par propriétaire : `OwnerOnly` avec `owner_column` déclarée
/// autorise la création (le créateur devient le propriétaire). Sans colonne déclarée, la création
/// est refusée (`false`) hors admin — fail-closed arbitré le 2026-09-27, cohérent avec
/// `evaluate` et [`list_access`] sur la même combinaison ; le raccourci admin reste valable sur
/// ce chemin (une seule requête [`is_admin`]). `Group`/`AdminOnly` filtrent selon l'appartenance.
///
/// # Errors
///
/// Propage toute [`DbErr`] de [`is_admin`] : une panne d'infrastructure n'est jamais dégradée
/// en refus silencieux.
pub async fn can_create<E>(db: &DatabaseConnection, user: &user::Model) -> Result<bool, DbErr>
where
    E: MiryadResource,
{
    match E::write_policy() {
        AccessPolicy::Public => Ok(true),
        AccessPolicy::OwnerOnly => {
            if E::owner_column().is_some() {
                return Ok(true);
            }
            // Colonne absente : fail-closed pour un non-admin, le raccourci admin passe.
            is_admin(db, user.id).await
        }
        AccessPolicy::AdminOnly => is_admin(db, user.id).await,
        AccessPolicy::Group(name) => {
            if is_admin(db, user.id).await? {
                return Ok(true);
            }
            is_member(db, user.id, name).await
        }
    }
}

/// Verdict décidable **sans enregistrement** (arbitré 2026-09-27, factorisation extraite de
/// [`evaluate`]) : `Public` → `Some(true)` avant toute requête ; admin → `Some(true)` ;
/// `AdminOnly` hors admin → `Some(false)` ; `Group` → `Some` du résultat de [`is_member`] ;
/// `OwnerOnly` → `None` — la décision requiert un enregistrement, y compris quand
/// [`MiryadResource::owner_column`] est `None` (la branche fail-closed reste dans [`evaluate`],
/// qui a l'enregistrement). Consommée par [`evaluate`] et, avant toute lecture de ligne, par
/// `rest::core` (fermeture de l'oracle d'existence, `./rest/core.sdd`).
///
/// # Errors
///
/// Propage toute [`DbErr`] de [`is_admin`] ou [`is_member`] : une panne d'infrastructure n'est
/// jamais dégradée en refus silencieux.
// `E` n'est utilisé qu'à l'appel (turbofish) pour épingler l'entité évaluée : signature imposée par `rbac.sdd` `Exposes` (arbitré 2026-09-27).
#[allow(clippy::extra_unused_type_parameters)]
pub(crate) async fn static_verdict<E>(
    db: &DatabaseConnection,
    policy: AccessPolicy,
    user: &user::Model,
) -> Result<Option<bool>, DbErr>
where
    E: MiryadResource,
{
    if policy == AccessPolicy::Public {
        return Ok(Some(true));
    }
    // L'admin l'emporte toujours sur les autres politiques (cf. feature 1, doc du trait
    // MiryadResource : "+ les membres du groupe admin" sur OwnerOnly et Group(name)).
    if is_admin(db, user.id).await? {
        return Ok(Some(true));
    }
    match policy {
        // Déjà rendu par le retour anticipé au-dessus, avant toute requête — le répéter rend un
        // verdict explicite plutôt qu'une panic (`unreachable` de la dette stricte purgée, cf.
        // `tooling.sdd`).
        AccessPolicy::Public => Ok(Some(true)),
        AccessPolicy::AdminOnly => Ok(Some(false)),
        AccessPolicy::Group(name) => Ok(Some(is_member(db, user.id, name).await?)),
        // Indécidable sans enregistrement : comparaison (et fail-closed sans colonne) restent
        // dans `evaluate`.
        AccessPolicy::OwnerOnly => Ok(None),
    }
}

async fn evaluate<E>(
    db: &DatabaseConnection,
    policy: AccessPolicy,
    owner_column: Option<E::Column>,
    user: &user::Model,
    record: &E::Model,
) -> Result<bool, DbErr>
where
    E: MiryadResource,
    E::Model: ModelTrait<Entity = E>,
{
    // Partie statique (Public, raccourci admin, AdminOnly, Group) partagée avec `rest::core`
    // via `static_verdict` ; seul `OwnerOnly` rend `None` et atteint la comparaison ci-dessous
    // (`rbac.sdd` `Must` : `evaluate` = `static_verdict` puis la seule comparaison de colonne
    // propriétaire — la purge `unreachable` de `tooling.sdd` retire le match et sa panic).
    if let Some(verdict) = static_verdict::<E>(db, policy, user).await? {
        return Ok(verdict);
    }

    // Contrat feature 1 : `owner_column() == None` avec `OwnerOnly` est un comportement
    // non défini au niveau du trait — on choisit de refuser plutôt que de risquer un
    // accès non voulu (fail-closed).
    let Some(col) = owner_column else {
        return Ok(false);
    };
    let owner_value = record.get(col);
    Ok(owner_value == sea_orm::Value::from(user.id))
}

/// Résultat de l'évaluation RBAC pour une opération de liste (feature 4) — contrairement à
/// `can_read`/`can_write`, il n'y a pas encore d'enregistrement précis à comparer, donc pas de
/// simple booléen : soit on ne filtre pas, soit on filtre par propriétaire, soit c'est refusé
/// avant même de construire une requête.
#[derive(Debug)]
pub enum ListAccess {
    /// Politique publique, ou appelant admin — aucune restriction à appliquer.
    Unrestricted,
    /// Politique `OwnerOnly` pour un appelant non-admin — condition à ajouter à la requête de
    /// liste (`WHERE owner_column = user.id`).
    FilterByOwner(Condition),
    /// Politique `Group`/`AdminOnly` sans l'appartenance requise — pas de requête à exécuter.
    Forbidden,
}

/// Restriction pour un listage, décidée sur `read_policy` seule : jamais un booléen, jamais
/// l'exécution de la requête — `Unrestricted`, le filtre par propriétaire ou le refus avant
/// toute construction de requête.
///
/// # Errors
///
/// Propage toute [`DbErr`] de [`is_admin`] ou [`is_member`] : une panne d'infrastructure n'est
/// jamais dégradée en refus silencieux.
pub async fn list_access<E>(db: &DatabaseConnection, user: &user::Model) -> Result<ListAccess, DbErr>
where
    E: MiryadResource,
{
    let policy = E::read_policy();

    if policy == AccessPolicy::Public {
        return Ok(ListAccess::Unrestricted);
    }
    if is_admin(db, user.id).await? {
        return Ok(ListAccess::Unrestricted);
    }

    match policy {
        // Déjà rendu par le retour anticipé au-dessus, avant toute requête — le répéter rend un
        // verdict explicite plutôt qu'une panic (`unreachable` de la dette stricte purgée, cf.
        // `tooling.sdd`, même patron que `static_verdict`).
        AccessPolicy::Public => Ok(ListAccess::Unrestricted),
        AccessPolicy::AdminOnly => Ok(ListAccess::Forbidden),
        AccessPolicy::Group(name) => {
            if is_member(db, user.id, name).await? {
                Ok(ListAccess::Unrestricted)
            } else {
                Ok(ListAccess::Forbidden)
            }
        }
        AccessPolicy::OwnerOnly => match E::owner_column() {
            Some(col) => Ok(ListAccess::FilterByOwner(Condition::all().add(col.eq(user.id)))),
            // Même contrat fail-closed que `evaluate` : pas de colonne, pas d'accès.
            None => Ok(ListAccess::Forbidden),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use crate::users::group::{Column as GroupColumn, Entity as GroupEntity};
    use crate::users::{ensure_service_account, resolve_user, sync_group_memberships};
    use sea_orm::{Schema, Set};
    use sea_orm_migration::MigratorTrait;

    mod recipe {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "recipes")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
            pub owner_id: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "recipes"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn owner_column() -> Option<Column> {
                Some(Column::OwnerId)
            }
        }
    }

    mod ingredient {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "ingredients")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub name: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "ingredients"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Group("editors")
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    /// `OwnerOnly` en lecture *et* écriture — `recipe`/`ingredient` ne couvrent pas ce cas
    /// (`recipe` est public en lecture), nécessaire pour tester `ListAccess::FilterByOwner`.
    mod note {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "notes")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub body: String,
            pub owner_id: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "notes"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn owner_column() -> Option<Column> {
                Some(Column::OwnerId)
            }
        }
    }

    /// `OwnerOnly` en lecture *et* écriture avec `owner_column() -> None` — la déclaration
    /// incohérente arbitrée fail-closed le 2026-09-27, fixture du refus de `can_create` sur
    /// cette combinaison (`memos` de `./rbac.sdd`).
    mod memos {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "memos")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub body: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "memos"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    /// `drafts` — lecture `Group("ghosts")`, écriture `Group("maintainers")` : fixture du refus
    /// sur groupe jamais vu (`Scenario` « groupe inconnu : refus sans créer quoi que ce soit »)
    /// et de la création par groupe nommé (`Scenario` « Création `Group` »). Écart de spec
    /// consigné : la ligne `Tasks` de `rbac.sdd` annonce `read Group("drafts")` pour cette
    /// fixture alors que le `Scenario` « groupe inconnu » exige explicitement
    /// `read Group("ghosts")` ; le `Scenario` (matière première des tests, `bootstrap.project.md`)
    /// est verrouillé ici, l'écart est remonté pour arbitrage `[?]`.
    mod drafts {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "drafts")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub body: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "drafts"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Group("ghosts")
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Group("maintainers")
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    /// `bulletins` — lecture `AdminOnly`, écriture `Public` : dissocie les deux politiques sur une
    /// même entité, fixture du listage `AdminOnly` (`Scenario` « Listage `AdminOnly` ») et du
    /// `Ok(true)` de `can_create` `Public` sans aucune requête (`Scenario` « `can_create` répond
    /// `Public` et `OwnerOnly` sans toucher la base »).
    mod bulletins {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "bulletins")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "bulletins"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    /// `ledgers` — `OwnerOnly` en lecture *et* écriture avec colonne propriétaire typée
    /// `Option<i32>` : fixture de la valeur `None` portée par une colonne déclarée (`Scenario`
    /// « enregistrement dont la colonne propriétaire vaut `None` » — arbitré 2026-09-29, contrat
    /// `Handles` de `rbac.sdd`).
    mod ledgers {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "ledgers")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub label: String,
            pub owner_id: Option<i32>,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "ledgers"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn owner_column() -> Option<Column> {
                Some(Column::OwnerId)
            }
        }
    }

    async fn test_db() -> DatabaseConnection {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    /// `sqlite::memory:` jamais migrée (aucune table `crate::migration`) — fixture des `Scenario`
    /// « sans toucher la base » (les `Ok` y sont la preuve qu'aucune requête n'est partie) et de
    /// la propagation `DbErr` (les non-`Public` butent sur les tables d'appartenance absentes).
    async fn unmigrated_db() -> DatabaseConnection {
        sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects (base non migrée)")
    }

    /// Utilisateur construit en mémoire, jamais via `resolve_user` — les `Scenario` sur connexion
    /// non migrée où la table `miryad_users` elle-même n'existe pas.
    fn in_memory_user(id: i32, subject: &str) -> user::Model {
        user::Model {
            id,
            subject: subject.to_string(),
            email: None,
            display_name: None,
            created_at: chrono::Utc::now(),
        }
    }

    /// Crée à la volée la table d'une fixture (patron de `rest/core.rs`) — les tables d'entités de
    /// test ne sont pas posées par `crate::migration`.
    async fn create_entity_table<E: EntityTrait>(db: &DatabaseConnection, entity: E) {
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(entity))
            .await
            .expect("fixture table creates");
    }

    #[tokio::test]
    async fn owner_only_allows_owner_denies_others_allows_admin() {
        let db = test_db().await;
        let owner = resolve_user(&db, "owner", None).await.expect("resolve");
        let other = resolve_user(&db, "other", None).await.expect("resolve");
        let admin = resolve_user(&db, "admin-user", None).await.expect("resolve");
        sync_group_memberships(&db, admin.id, &["admin".to_string()])
            .await
            .expect("sync");

        let record = recipe::Model {
            id: 1,
            title: "Tarte".to_string(),
            owner_id: owner.id,
        };

        assert!(can_write::<recipe::Entity>(&db, &owner, &record).await.unwrap());
        assert!(!can_write::<recipe::Entity>(&db, &other, &record).await.unwrap());
        assert!(can_write::<recipe::Entity>(&db, &admin, &record).await.unwrap());
    }

    #[tokio::test]
    async fn public_policy_always_allows() {
        let db = test_db().await;
        let anyone = resolve_user(&db, "anyone", None).await.expect("resolve");
        let record = recipe::Model {
            id: 1,
            title: "Tarte".to_string(),
            owner_id: 999,
        };
        assert!(can_read::<recipe::Entity>(&db, &anyone, &record).await.unwrap());
    }

    #[tokio::test]
    async fn group_policy_allows_member_and_admin_denies_others() {
        let db = test_db().await;
        let member = resolve_user(&db, "member", None).await.expect("resolve");
        sync_group_memberships(&db, member.id, &["editors".to_string()])
            .await
            .expect("sync");
        let admin = resolve_user(&db, "admin-user", None).await.expect("resolve");
        sync_group_memberships(&db, admin.id, &["admin".to_string()])
            .await
            .expect("sync");
        let stranger = resolve_user(&db, "stranger", None).await.expect("resolve");

        let record = ingredient::Model {
            id: 1,
            name: "Sel".to_string(),
        };

        assert!(
            can_read::<ingredient::Entity>(&db, &member, &record)
                .await
                .unwrap()
        );
        assert!(
            can_read::<ingredient::Entity>(&db, &admin, &record)
                .await
                .unwrap()
        );
        assert!(
            !can_read::<ingredient::Entity>(&db, &stranger, &record)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn admin_only_denies_non_admin() {
        let db = test_db().await;
        let stranger = resolve_user(&db, "stranger", None).await.expect("resolve");
        let record = ingredient::Model {
            id: 1,
            name: "Sel".to_string(),
        };
        assert!(
            !can_write::<ingredient::Entity>(&db, &stranger, &record)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn owner_only_without_owner_column_is_fail_closed() {
        // `ingredient` déclare `owner_column() -> None`. Si son `write_policy` était `OwnerOnly`
        // (contrat non respecté d'après feature 1), l'évaluateur doit refuser, pas paniquer ni
        // autoriser par défaut.
        let db = test_db().await;
        let stranger = resolve_user(&db, "stranger", None).await.expect("resolve");
        let record = ingredient::Model {
            id: 1,
            name: "Sel".to_string(),
        };
        let result = evaluate::<ingredient::Entity>(
            &db,
            AccessPolicy::OwnerOnly,
            ingredient::Entity::owner_column(),
            &stranger,
            &record,
        )
        .await
        .expect("evaluation does not error");
        assert!(!result);
    }

    #[tokio::test]
    async fn list_access_public_is_unrestricted() {
        let db = test_db().await;
        let anyone = resolve_user(&db, "anyone", None).await.expect("resolve");
        assert!(matches!(
            list_access::<recipe::Entity>(&db, &anyone).await.unwrap(),
            ListAccess::Unrestricted
        ));
    }

    #[tokio::test]
    async fn list_access_owner_only_filters_for_non_admin_but_not_admin() {
        let db = test_db().await;
        let owner = resolve_user(&db, "owner", None).await.expect("resolve");
        let admin = resolve_user(&db, "admin-user", None).await.expect("resolve");
        sync_group_memberships(&db, admin.id, &["admin".to_string()])
            .await
            .expect("sync");

        match list_access::<note::Entity>(&db, &owner).await.unwrap() {
            ListAccess::FilterByOwner(_) => (),
            other => panic!("expected FilterByOwner for a non-admin, got {other:?}"),
        }
        assert!(matches!(
            list_access::<note::Entity>(&db, &admin).await.unwrap(),
            ListAccess::Unrestricted
        ));
    }

    #[tokio::test]
    async fn list_access_group_policy_forbidden_without_membership() {
        let db = test_db().await;
        let stranger = resolve_user(&db, "stranger", None).await.expect("resolve");
        let member = resolve_user(&db, "member", None).await.expect("resolve");
        sync_group_memberships(&db, member.id, &["editors".to_string()])
            .await
            .expect("sync");

        assert!(matches!(
            list_access::<ingredient::Entity>(&db, &stranger).await.unwrap(),
            ListAccess::Forbidden
        ));
        // `ingredient.read_policy()` est `Group("editors")`.
        assert!(matches!(
            list_access::<ingredient::Entity>(&db, &member).await.unwrap(),
            ListAccess::Unrestricted
        ));
    }

    #[tokio::test]
    async fn can_create_owner_only_without_column_is_fail_closed() {
        // Scénario « Création `OwnerOnly` sans colonne : fail-closed » — arbitré 2026-09-27,
        // cohérent avec le fail-closed de `evaluate`/`list_access` sur la même combinaison.
        let db = test_db().await;
        let stranger = resolve_user(&db, "stranger", None).await.expect("resolve");
        assert!(
            !can_create::<memos::Entity>(&db, &stranger).await.unwrap(),
            "OwnerOnly sans owner_column doit être refusé à la création (fail-closed)"
        );
    }

    #[tokio::test]
    async fn can_create_owner_only_without_column_allows_admin() {
        // Scénario « raccourci admin passe la création `OwnerOnly` sans colonne » — le raccourci
        // admin s'applique à `can_create` comme à `evaluate` (arbitré 2026-09-27).
        let db = test_db().await;
        let admin = resolve_user(&db, "admin-user", None).await.expect("resolve");
        sync_group_memberships(&db, admin.id, &["admin".to_string()])
            .await
            .expect("sync");
        assert!(
            can_create::<memos::Entity>(&db, &admin).await.unwrap(),
            "le raccourci admin doit passer la création OwnerOnly sans colonne"
        );
    }

    #[tokio::test]
    async fn static_verdict_denies_admin_only_and_group_without_record() {
        // Scénario « `static_verdict` décide `AdminOnly`/`Group` sans charger d'enregistrement » —
        // `test_db` ne migre que `crate::migration` : les tables `ingredients`/`drafts` n'existent
        // pas, un `Ok(Some(false))` prouve que le refus se décide sans relecture d'enregistrement.
        let db = test_db().await;
        let stranger = resolve_user(&db, "stranger", None).await.expect("resolve");
        assert_eq!(
            static_verdict::<ingredient::Entity>(&db, AccessPolicy::AdminOnly, &stranger)
                .await
                .unwrap(),
            Some(false),
            "AdminOnly hors admin doit se décider sans enregistrement"
        );
        assert_eq!(
            static_verdict::<ingredient::Entity>(&db, AccessPolicy::Group("editors"), &stranger)
                .await
                .unwrap(),
            Some(false),
            "Group sans appartenance doit se décider sans enregistrement"
        );
    }

    #[tokio::test]
    async fn static_verdict_defers_on_owner_only() {
        // Scénario « `static_verdict` diffère sur `OwnerOnly` » — colonne déclarée (`note`) comme
        // absente (`ingredient`) : la décision requiert un enregistrement, `static_verdict` rend
        // `None` dans les deux cas, la branche fail-closed restant dans `evaluate`.
        let db = test_db().await;
        let stranger = resolve_user(&db, "stranger", None).await.expect("resolve");
        assert_eq!(
            static_verdict::<note::Entity>(&db, AccessPolicy::OwnerOnly, &stranger)
                .await
                .unwrap(),
            None,
            "OwnerOnly avec colonne déclarée doit différer (None)"
        );
        assert_eq!(
            static_verdict::<ingredient::Entity>(&db, AccessPolicy::OwnerOnly, &stranger)
                .await
                .unwrap(),
            None,
            "OwnerOnly sans colonne doit aussi différer (None) — le fail-closed est dans evaluate"
        );
    }

    /// Scenario « Écriture `AdminOnly` : admin passe de définition » — le raccourci admin précède
    /// le match : `AdminOnly` est la politique même du groupe admin.
    #[tokio::test]
    async fn admin_only_write_policy_allows_admin() {
        let db = test_db().await;
        let admin = resolve_user(&db, "admin-user", None).await.expect("resolve");
        sync_group_memberships(&db, admin.id, &["admin".to_string()])
            .await
            .expect("sync");
        let record = ingredient::Model {
            id: 1,
            name: "Sel".to_string(),
        };
        assert!(
            can_write::<ingredient::Entity>(&db, &admin, &record)
                .await
                .unwrap(),
            "le raccourci admin doit passer `AdminOnly` de définition"
        );
    }

    /// Scenario « Lecture `OwnerOnly` : propriétaire passe, non-propriétaire refusé » — la
    /// politique de lecture est lue indépendamment de celle d'écriture (`note` est `OwnerOnly` en
    /// lecture, ce que `recipe` — public en lecture — ne couvre pas).
    #[tokio::test]
    async fn owner_only_read_policy_allows_owner_and_denies_others() {
        let db = test_db().await;
        let owner = resolve_user(&db, "owner", None).await.expect("resolve");
        let other = resolve_user(&db, "other", None).await.expect("resolve");
        let record = note::Model {
            id: 1,
            body: "Brouillon".to_string(),
            owner_id: owner.id,
        };
        assert!(
            can_read::<note::Entity>(&db, &owner, &record).await.unwrap(),
            "le propriétaire doit passer la lecture `OwnerOnly`"
        );
        assert!(
            !can_read::<note::Entity>(&db, &other, &record).await.unwrap(),
            "un non-propriétaire doit être refusé hors admin"
        );
    }

    /// Scenario « raccourci admin bat le fail-closed sans colonne » — `evaluate` forcé en
    /// `OwnerOnly` sur `ingredient` (`owner_column -> None`) : l'admin passe avant le refus qui
    /// frappe le non-admin (verrou complémentaire de
    /// `owner_only_without_owner_column_is_fail_closed`).
    #[tokio::test]
    async fn admin_shortcut_wins_before_ownerless_fail_closed() {
        let db = test_db().await;
        let admin = resolve_user(&db, "admin-user", None).await.expect("resolve");
        sync_group_memberships(&db, admin.id, &["admin".to_string()])
            .await
            .expect("sync");
        let record = ingredient::Model {
            id: 1,
            name: "Sel".to_string(),
        };
        let result = evaluate::<ingredient::Entity>(
            &db,
            AccessPolicy::OwnerOnly,
            ingredient::Entity::owner_column(),
            &admin,
            &record,
        )
        .await
        .expect("evaluation does not error");
        assert!(
            result,
            "le raccourci admin doit précéder le fail-closed sans colonne"
        );
    }

    /// Scenario « la condition `FilterByOwner` ne rend que les lignes du demandeur » — conjonction
    /// `Condition::all().add` d'une seule égalité sur `user::Model::id` : ajoutée en `filter` à
    /// `note::Entity::find()`, elle ne laisse passer que la ligne de `owner`.
    #[tokio::test]
    async fn filter_by_owner_condition_returns_only_own_rows() {
        let db = test_db().await;
        create_entity_table(&db, note::Entity).await;
        let owner = resolve_user(&db, "owner", None).await.expect("resolve");
        let other = resolve_user(&db, "other", None).await.expect("resolve");
        for (id, owner_id) in [(1, owner.id), (2, other.id)] {
            note::ActiveModel {
                id: Set(id),
                body: Set(format!("note {id}")),
                owner_id: Set(owner_id),
            }
            .insert(&db)
            .await
            .expect("fixture rows insert");
        }

        let condition = match list_access::<note::Entity>(&db, &owner).await.unwrap() {
            ListAccess::FilterByOwner(condition) => condition,
            other => panic!("expected FilterByOwner for the owner, got {other:?}"),
        };

        let all = note::Entity::find().all(&db).await.expect("query succeeds");
        assert_eq!(
            all.len(),
            2,
            "GIVEN : deux lignes en base, deux propriétaires distincts"
        );
        let rows = note::Entity::find()
            .filter(condition)
            .all(&db)
            .await
            .expect("filtered query succeeds");
        assert_eq!(rows.len(), 1, "la condition ne laisse passer qu'une ligne");
        let row = rows.first().expect("one row survives the filter");
        assert_eq!(row.id, 1);
        assert_eq!(row.owner_id, owner.id, "seule la ligne du demandeur passe");
    }

    /// Scenario « Listage `OwnerOnly` sans colonne : interdit non-admin, libre admin » — le même
    /// fail-closed écrit qu'en enregistrement ; le raccourci admin gagne avant la colonne absente.
    #[tokio::test]
    async fn list_access_owner_only_without_column_forbids_non_admin_but_not_admin() {
        let db = test_db().await;
        let stranger = resolve_user(&db, "stranger", None).await.expect("resolve");
        let admin = resolve_user(&db, "admin-user", None).await.expect("resolve");
        sync_group_memberships(&db, admin.id, &["admin".to_string()])
            .await
            .expect("sync");

        assert!(
            matches!(
                list_access::<memos::Entity>(&db, &stranger).await.unwrap(),
                ListAccess::Forbidden
            ),
            "OwnerOnly sans colonne doit fermer le listage au non-admin (fail-closed)"
        );
        assert!(
            matches!(
                list_access::<memos::Entity>(&db, &admin).await.unwrap(),
                ListAccess::Unrestricted
            ),
            "le raccourci admin doit passer avant la vérification de colonne absente"
        );
    }

    /// Scenario « Listage `AdminOnly` : interdit hors groupe admin » — fixture `bulletins`
    /// (lecture `AdminOnly`, écriture `Public`) : refus hors admin, libre pour l'admin.
    #[tokio::test]
    async fn list_access_admin_only_forbids_non_admin_and_allows_admin() {
        let db = test_db().await;
        let anyone = resolve_user(&db, "anyone", None).await.expect("resolve");
        let admin = resolve_user(&db, "admin-user", None).await.expect("resolve");
        sync_group_memberships(&db, admin.id, &["admin".to_string()])
            .await
            .expect("sync");

        assert!(
            matches!(
                list_access::<bulletins::Entity>(&db, &anyone).await.unwrap(),
                ListAccess::Forbidden
            ),
            "AdminOnly doit fermer le listage hors groupe admin"
        );
        assert!(
            matches!(
                list_access::<bulletins::Entity>(&db, &admin).await.unwrap(),
                ListAccess::Unrestricted
            ),
            "l'admin liste `AdminOnly` de définition"
        );
    }

    /// Scenario « groupe inconnu : refus sans créer quoi que ce soit » — `drafts` lit
    /// `Group("ghosts")`, groupe jamais apparu dans une `sync_group_memberships` : refus simple en
    /// lecture comme en listage, aucune ligne ajoutée à `miryad_groups` (`is_member` n'est pas
    /// `ensure_group`).
    #[tokio::test]
    async fn group_unknown_denies_without_creating_group() {
        let db = test_db().await;
        let stranger = resolve_user(&db, "stranger", None).await.expect("resolve");
        let record = drafts::Model {
            id: 1,
            body: "Brouillon".to_string(),
        };
        let groups_before = GroupEntity::find()
            .all(&db)
            .await
            .expect("count query succeeds")
            .len();

        assert!(
            !can_read::<drafts::Entity>(&db, &stranger, &record)
                .await
                .expect("un groupe jamais vu est un refus `Ok(false)`, pas une erreur"),
            "le groupe `ghosts` jamais vu doit refuser comme un non-membre"
        );
        assert!(
            matches!(
                list_access::<drafts::Entity>(&db, &stranger).await.unwrap(),
                ListAccess::Forbidden
            ),
            "le listage sous groupe jamais vu doit être interdit, pas filtré"
        );

        assert_eq!(
            GroupEntity::find()
                .all(&db)
                .await
                .expect("count query succeeds")
                .len(),
            groups_before,
            "la lecture d'autorisation ne crée pas le groupe cité"
        );
        assert!(
            GroupEntity::find()
                .filter(GroupColumn::Name.eq("ghosts"))
                .one(&db)
                .await
                .expect("query succeeds")
                .is_none(),
            "aucune ligne `ghosts` n'est apparue dans `miryad_groups`"
        );
    }

    /// Scenario « la lecture de création dépend de write, la liste de read » — `ingredient` :
    /// `Group("editors")` en lecture, `AdminOnly` en écriture ; un membre des seuls `editors`
    /// liste librement mais ne crée pas : le droit de lecture ne donne aucun droit de créer.
    #[tokio::test]
    async fn list_uses_read_policy_and_create_uses_write_policy() {
        let db = test_db().await;
        let member = resolve_user(&db, "member", None).await.expect("resolve");
        sync_group_memberships(&db, member.id, &["editors".to_string()])
            .await
            .expect("sync");

        assert!(
            matches!(
                list_access::<ingredient::Entity>(&db, &member).await.unwrap(),
                ListAccess::Unrestricted
            ),
            "read_policy seule gouverne le listage"
        );
        assert!(
            !can_create::<ingredient::Entity>(&db, &member).await.unwrap(),
            "write_policy seule gouverne la création : lire ne donne aucun droit de créer"
        );
    }

    /// Scenario « `can_create` répond `Public` et `OwnerOnly` sans toucher la base » — sur une
    /// connexion jamais migrée, chaque `Ok` est la preuve qu'aucune requête `miryad_groups` n'est
    /// partie (`recipe` écrit `OwnerOnly` avec colonne déclarée, `bulletins` écrit `Public`).
    #[tokio::test]
    async fn can_create_public_or_owner_only_never_queries_db() {
        let db = unmigrated_db().await;
        let user = in_memory_user(7, "ghost-subject");

        assert!(
            can_create::<recipe::Entity>(&db, &user)
                .await
                .expect("OwnerOnly avec colonne déclarée : Ok(true) sans aucune requête"),
            "OwnerOnly avec owner_column Some autorise la création sans rien lire"
        );
        assert!(
            can_create::<bulletins::Entity>(&db, &user)
                .await
                .expect("Public : Ok(true) sans aucune requête"),
            "Public court-circuite avant is_admin"
        );
    }

    /// Scenario « Création `AdminOnly` : seul admin passe » — `ingredient` écrit en `AdminOnly` :
    /// `true` pour l'admin, `false` pour l'étranger.
    #[tokio::test]
    async fn can_create_admin_only_grants_admin_denies_others() {
        let db = test_db().await;
        let stranger = resolve_user(&db, "stranger", None).await.expect("resolve");
        let admin = resolve_user(&db, "admin-user", None).await.expect("resolve");
        sync_group_memberships(&db, admin.id, &["admin".to_string()])
            .await
            .expect("sync");

        assert!(
            can_create::<ingredient::Entity>(&db, &admin).await.unwrap(),
            "l'admin crée sous `AdminOnly` de définition"
        );
        assert!(
            !can_create::<ingredient::Entity>(&db, &stranger).await.unwrap(),
            "hors admin, `AdminOnly` refuse la création"
        );
    }

    /// Scenario « Création `Group` : membre passe, admin passe, autres refusés » — `drafts` écrit
    /// en `Group("maintainers")` : le membre du groupe nommé passe (règle du groupe nommé),
    /// l'admin passe (raccourci avant `is_member`), l'étranger est refusé.
    #[tokio::test]
    async fn can_create_group_grants_admin_and_member_only() {
        let db = test_db().await;
        let maintainer = resolve_user(&db, "maintainer", None).await.expect("resolve");
        sync_group_memberships(&db, maintainer.id, &["maintainers".to_string()])
            .await
            .expect("sync");
        let admin = resolve_user(&db, "admin-user", None).await.expect("resolve");
        sync_group_memberships(&db, admin.id, &["admin".to_string()])
            .await
            .expect("sync");
        let stranger = resolve_user(&db, "stranger", None).await.expect("resolve");

        assert!(
            can_create::<drafts::Entity>(&db, &maintainer).await.unwrap(),
            "le membre du groupe nommé doit pouvoir créer"
        );
        assert!(
            can_create::<drafts::Entity>(&db, &admin).await.unwrap(),
            "le raccourci admin doit passer avant is_member"
        );
        assert!(
            !can_create::<drafts::Entity>(&db, &stranger).await.unwrap(),
            "sans appartenance, `Group` refuse la création"
        );
    }

    /// Scenario « raccourci `Public` n'émet aucune requête » — sur connexion jamais migrée,
    /// `can_read` (`recipe`, lecture `Public`), `can_write` (`bulletins`, écriture `Public`) et
    /// `list_access` (`recipe`) rendent `Ok` : `Public` précède `is_admin`, les tables absentes ne
    /// peuvent rien casser sur ces trois chemins.
    #[tokio::test]
    async fn public_reads_create_list_never_touch_missing_tables() {
        let db = unmigrated_db().await;
        let user = in_memory_user(7, "ghost-subject");
        let recipe_record = recipe::Model {
            id: 1,
            title: "Tarte".to_string(),
            owner_id: 999,
        };
        let bulletin_record = bulletins::Model {
            id: 1,
            title: "Avis".to_string(),
        };

        assert!(
            can_read::<recipe::Entity>(&db, &user, &recipe_record)
                .await
                .expect("lecture Public : Ok avant toute table absente"),
            "la lecture Public doit accorder sans aucune requête"
        );
        assert!(
            can_write::<bulletins::Entity>(&db, &user, &bulletin_record)
                .await
                .expect("écriture Public : Ok avant toute table absente"),
            "l'écriture Public doit accorder sans aucune requête"
        );
        assert!(
            matches!(
                list_access::<recipe::Entity>(&db, &user)
                    .await
                    .expect("listage Public : Ok avant toute table absente"),
                ListAccess::Unrestricted
            ),
            "le listage Public doit être Unrestricted sans aucune requête"
        );
    }

    /// Scenario « tables d'appartenance manquantes : `Err` propagé, pas de refus silencieux » —
    /// sur connexion jamais migrée, les quatre chemins non-`Public` d'`ingredient` (`Group` en
    /// lecture, `AdminOnly` en écriture) remontent le `DbErr` de `is_admin`/`is_member` ;
    /// `recipe` (`Public` en lecture) reste `Unrestricted` sur la même connexion.
    #[tokio::test]
    async fn non_public_paths_propagate_dberr_when_tables_missing() {
        let db = unmigrated_db().await;
        let user = in_memory_user(7, "ghost-subject");
        let record = ingredient::Model {
            id: 1,
            name: "Sel".to_string(),
        };

        assert!(
            can_read::<ingredient::Entity>(&db, &user, &record).await.is_err(),
            "Group en lecture : le DbErr remonte, jamais un false silencieux"
        );
        assert!(
            can_write::<ingredient::Entity>(&db, &user, &record)
                .await
                .is_err(),
            "AdminOnly en écriture : idem"
        );
        assert!(
            can_create::<ingredient::Entity>(&db, &user).await.is_err(),
            "AdminOnly à la création : idem"
        );
        assert!(
            list_access::<ingredient::Entity>(&db, &user).await.is_err(),
            "Group en listage : idem"
        );
        assert!(
            matches!(
                list_access::<recipe::Entity>(&db, &user)
                    .await
                    .expect("recipe Public : le raccourci passe sur la même connexion"),
                ListAccess::Unrestricted
            ),
            "le raccourci Public ignore les tables absentes"
        );
    }

    /// Scenario « enregistrement dont la colonne propriétaire vaut `None` : refusé hors admin » —
    /// contrat arbitré 2026-09-29 : une ligne sans propriétaire n'appartient à personne, `None` ne
    /// matche jamais un i32, seul l'admin y accède.
    #[tokio::test]
    async fn owner_only_null_owner_value_denies_non_admin() {
        let db = test_db().await;
        let requester = resolve_user(&db, "requester", None).await.expect("resolve");
        let admin = resolve_user(&db, "admin-user", None).await.expect("resolve");
        sync_group_memberships(&db, admin.id, &["admin".to_string()])
            .await
            .expect("sync");
        let record = ledgers::Model {
            id: 1,
            label: "Grand livre".to_string(),
            owner_id: None,
        };

        assert!(
            !can_write::<ledgers::Entity>(&db, &requester, &record)
                .await
                .unwrap(),
            "une colonne propriétaire à None ne matche jamais : refus hors admin"
        );
        assert!(
            can_write::<ledgers::Entity>(&db, &admin, &record).await.unwrap(),
            "seul chemin vers une ligne sans propriétaire : le raccourci admin"
        );
    }

    /// Scenario « compte de service évalué comme n'importe quel utilisateur » — `svc-admin`
    /// (groupe `admin`) passe, `svc-plain` (aucune appartenance) refuse, sur un enregistrement
    /// appartenant à un tiers : la nature du principal est ignorée, seule l'appartenance compte.
    #[tokio::test]
    async fn service_account_follows_group_membership_as_regular_user() {
        const PEPPER: &str = "test-pepper";
        let db = test_db().await;
        ensure_service_account(
            &db,
            "svc-admin",
            "svc-admin-secret-0123456789",
            "bootstrap",
            &["admin".to_string()],
            None,
            PEPPER,
        )
        .await
        .expect("svc-admin provisioned");
        ensure_service_account(
            &db,
            "svc-plain",
            "svc-plain-secret-0123456789",
            "bootstrap",
            &[],
            None,
            PEPPER,
        )
        .await
        .expect("svc-plain provisioned");
        let svc_admin = resolve_user(&db, "svc-admin", None).await.expect("resolve");
        let svc_plain = resolve_user(&db, "svc-plain", None).await.expect("resolve");
        let owner = resolve_user(&db, "owner", None).await.expect("resolve");
        let record = note::Model {
            id: 1,
            body: "Personnel".to_string(),
            owner_id: owner.id,
        };

        assert!(
            can_write::<note::Entity>(&db, &svc_admin, &record).await.unwrap(),
            "svc-admin, membre de `admin`, passe comme un humain"
        );
        assert!(
            !can_write::<note::Entity>(&db, &svc_plain, &record).await.unwrap(),
            "svc-plain sans appartenance est refusé comme un humain non-propriétaire"
        );
    }
}
