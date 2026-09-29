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
    use crate::users::{resolve_user, sync_group_memberships};
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

    async fn test_db() -> DatabaseConnection {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
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
}
