//! Logique métier des 5 opérations CRUD génériques, indépendante d'axum — utilisée par les
//! handlers REST (`rest/mod.rs`) et par les tools MCP (feature 6), pour ne jamais dupliquer les
//! règles RBAC/pagination/injection de propriétaire entre les deux surfaces d'API.

use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, IntoActiveModel, Iterable, ModelTrait,
    PaginatorTrait, PrimaryKeyToColumn, QueryFilter, QueryOrder,
};

use crate::auth::AuthPrincipal;
use crate::query::{PagedResult, Pagination};
use crate::rbac::{ListAccess, can_create, can_read, can_write, list_access, static_verdict};
use crate::rest::RestEntity;
use crate::rest::error::RestError;
use crate::users::resolve_user;

// rest/core.sdd `Must` : `E::PrimaryKey::iter()` ne peut être vide sous la bound PK `i32` — `expect` inatteignable compilable.
#[allow(clippy::expect_used)]
pub(crate) fn primary_key_column<E: RestEntity>() -> E::Column {
    E::PrimaryKey::iter()
        .next()
        .expect("RestEntity assumes exactly one primary key column")
        .into_column()
}

/// `Model::into_active_model()` (généré par `DeriveEntityModel`) marque tous les champs
/// `Unchanged`, pas `Set` — pertinent pour un modèle relu depuis la base, pas pour un corps de
/// requête PUT/POST qu'on veut écrire tel quel. `ActiveModelTrait::update()` n'inclut que les
/// champs `Set` dans la clause `SET` ; sans ce passage, un `PUT` n'écrirait aucune colonne
/// (`insert()` fonctionne quand même car il traite `Unchanged` comme une valeur à insérer, mais
/// `update()` ne le fait pas).
pub(crate) fn mark_all_set<E: RestEntity>(mut active: E::ActiveModel) -> E::ActiveModel {
    for col in E::Column::iter() {
        if let Some(value) = active.get(col).into_value() {
            active.set(col, value);
        }
    }
    active
}

pub(crate) async fn list<E: RestEntity>(
    db: &DatabaseConnection,
    principal: &AuthPrincipal,
    page: Option<u64>,
    per_page: Option<u64>,
    filter: Option<&str>,
) -> Result<PagedResult<E::Model>, RestError> {
    let user = resolve_user(db, &principal.subject, principal.email.as_deref()).await?;

    let condition = match list_access::<E>(db, &user).await? {
        ListAccess::Unrestricted => Condition::all(),
        ListAccess::FilterByOwner(condition) => condition,
        ListAccess::Forbidden => return Err(RestError::Forbidden),
    };
    let condition = match (E::filter_column(), filter) {
        (Some(col), Some(value)) => condition.add(col.eq(value)),
        _ => condition,
    };

    let pagination = Pagination::from_raw(page, per_page);
    // `rest/core.sdd` `Must` (arbitré 2026-09-29) : toute liste est ordonnée par la clé primaire
    // croissante — pagination déterministe, aucune ligne sautée ni dupliquée entre deux pages.
    let paginator = E::find()
        .filter(condition)
        .order_by_asc(primary_key_column::<E>())
        .paginate(db, pagination.per_page);
    let totals = paginator.num_items_and_pages().await?;
    // `query.sdd` borne `page >= 1` : `saturating_sub(1)` ne sature jamais, l'index rendu est
    // exactement `page - 1` (purge `arithmetic_side_effects` de `tooling.sdd`).
    let items = paginator.fetch_page(pagination.page.saturating_sub(1)).await?;

    Ok(PagedResult {
        items,
        page: pagination.page,
        per_page: pagination.per_page,
        total_items: totals.number_of_items,
        total_pages: totals.number_of_pages,
    })
}

pub(crate) async fn get<E: RestEntity>(
    db: &DatabaseConnection,
    principal: &AuthPrincipal,
    id: i32,
) -> Result<E::Model, RestError> {
    let user = resolve_user(db, &principal.subject, principal.email.as_deref()).await?;
    // Oracle d'existence (arbitrage `rest/core.sdd` 2026-09-27) : `AdminOnly` hors admin et
    // `Group` sans appartenance refusent avant toute requête sur la table de l'entité — un
    // étranger n'apprend jamais si la ligne existe. `Some(true)` (Public, admin, membre) est un
    // verdict déjà rendu : la ligne relue est rendue sans second appel rbac. `None`
    // (`OwnerOnly`, indécidable sans enregistrement) retombe sur `can_read` — résidu assumé.
    let verdict = static_verdict::<E>(db, E::read_policy(), &user).await?;
    if verdict == Some(false) {
        return Err(RestError::Forbidden);
    }
    let record = E::find_by_id(id).one(db).await?.ok_or(RestError::NotFound)?;
    if verdict.is_none() && !can_read::<E>(db, &user, &record).await? {
        return Err(RestError::Forbidden);
    }
    Ok(record)
}

pub(crate) async fn create<E: RestEntity>(
    db: &DatabaseConnection,
    principal: &AuthPrincipal,
    body: E::Model,
) -> Result<E::Model, RestError> {
    let user = resolve_user(db, &principal.subject, principal.email.as_deref()).await?;

    if !can_create::<E>(db, &user).await? {
        return Err(RestError::Forbidden);
    }

    let mut active = mark_all_set::<E>(body.into_active_model());
    // Le hook métier s'exécute avant le PK-stripping/l'injection du propriétaire ci-dessous, pour
    // que ces deux invariants de sécurité restent les derniers mots — un hook buggé ne peut pas
    // les contourner en mutant l'ActiveModel.
    active = E::before_create(active, principal).map_err(RestError::Application)?;
    // La BD attribue l'id — jamais une PK choisie par le client.
    active.not_set(primary_key_column::<E>());
    // Un utilisateur ne peut jamais créer une ressource au nom de quelqu'un d'autre, même en le
    // demandant explicitement dans le corps de la requête. Depuis l'arbitrage `rest/core.sdd`
    // 2026-09-27, l'injection vaut pour toute entité déclarant `owner_column`, quelle que soit la
    // `write_policy` : une colonne déclarée signifie « reflète qui a créé la ligne », pas une
    // colonne éditable par les appelants autorisés hors `OwnerOnly`.
    if let Some(owner_col) = E::owner_column() {
        active.set(owner_col, sea_orm::Value::from(user.id));
    }

    Ok(active.insert(db).await?)
}

pub(crate) async fn update<E: RestEntity>(
    db: &DatabaseConnection,
    principal: &AuthPrincipal,
    id: i32,
    body: E::Model,
) -> Result<E::Model, RestError> {
    let user = resolve_user(db, &principal.subject, principal.email.as_deref()).await?;
    // Même préfixe que `get` (arbitrage `rest/core.sdd` 2026-09-27) : verdict statique avant
    // relecture — `AdminOnly`/`Group` refusent sans jamais toucher la table de l'entité ;
    // `Some(true)` dispense de second appel rbac ; `None` (`OwnerOnly`) retombe sur `can_write`.
    let verdict = static_verdict::<E>(db, E::write_policy(), &user).await?;
    if verdict == Some(false) {
        return Err(RestError::Forbidden);
    }
    let existing = E::find_by_id(id).one(db).await?.ok_or(RestError::NotFound)?;

    if verdict.is_none() && !can_write::<E>(db, &user, &existing).await? {
        return Err(RestError::Forbidden);
    }

    let mut active = mark_all_set::<E>(body.into_active_model());
    // Le hook métier de la mise à jour s'exécute après `can_write` (déjà statué sur `existing`) et
    // avant les deux invariants ci-dessous, à la position symétrique de `before_create` dans
    // create() : ces deux invariants restent les derniers mots — un hook buggé ou hostile ne peut
    // ni déplacer la cible ni transférer la propriété (amendement `rest/core.sdd` 2026-09-23).
    // `existing` n'est prêté qu'en lecture seule, `Err` traverse en Application sans code `MRD-*`.
    active = E::before_update(active, &existing, principal).map_err(RestError::Application)?;
    // Force la PK depuis le chemin — ignore toute divergence dans le corps de la requête, y compris
    // celle que poserait le hook.
    active.set(primary_key_column::<E>(), sea_orm::Value::from(id));
    // Même invariant qu'à la création (cf. create()) : owner_column() n'est jamais éditable ni
    // par le client, même en le demandant explicitement dans le corps, ni par le hook — sinon un
    // owner_id divergent change silencieusement le propriétaire (incohérent avec la protection de
    // create()), ou un owner_id invalide fait échouer la requête sur une contrainte FK brute plutôt
    // qu'un 4xx propre. Depuis l'arbitrage 2026-09-27, la reconduction depuis `existing` vaut pour
    // toute entité déclarant `owner_column`, quelle que soit la `write_policy`.
    if let Some(owner_col) = E::owner_column() {
        active.set(owner_col, existing.get(owner_col));
    }

    active.update(db).await.map_err(|err| match err {
        // La ligne relue a disparu entre la relecture et l'`UPDATE` (course tolérée, fenêtre sans
        // verrou ni transaction — arbitrage `rest/core.sdd` 2026-09-27) : c'est un `404` légitime,
        // plus la panne `500` qu'un `DbErr` ordinaire traduisait jusqu'ici. Sur backend sans
        // `RETURNING` (type MySQL) `SeaORM` rend `RecordNotFound` après relecture vide là où
        // Postgres rend `RecordNotUpdated` — même disparition, même 404 (tâche « Revue 2026-09-29 »
        // de `rest/core.sdd`, aligné sur `auth/token.rs`).
        sea_orm::DbErr::RecordNotUpdated | sea_orm::DbErr::RecordNotFound(_) => RestError::NotFound,
        other => RestError::Database(other),
    })
}

pub(crate) async fn delete<E: RestEntity>(
    db: &DatabaseConnection,
    principal: &AuthPrincipal,
    id: i32,
) -> Result<(), RestError> {
    let user = resolve_user(db, &principal.subject, principal.email.as_deref()).await?;
    // Oracle d'existence sur la `write_policy` applicable — même préfixe que `get` (arbitrage
    // `rest/core.sdd` 2026-09-27) : `Some(false)` refuse avant toute requête sur la table de
    // l'entité, `Some(true)` dispense du second appel rbac après relecture, `None` (`OwnerOnly`,
    // indécidable sans enregistrement) retombe sur `can_write`.
    let verdict = static_verdict::<E>(db, E::write_policy(), &user).await?;
    if verdict == Some(false) {
        return Err(RestError::Forbidden);
    }
    let existing = E::find_by_id(id).one(db).await?.ok_or(RestError::NotFound)?;

    if verdict.is_none() && !can_write::<E>(db, &user, &existing).await? {
        return Err(RestError::Forbidden);
    }

    // Le hook métier de la suppression s'exécute après `can_write` (statué sur `existing`) et avant
    // toute émission de `DELETE` : un `Err` interrompt là, `delete_by_id` n'est jamais atteint
    // (amendement `rest/core.sdd` 2026-09-23). Aucun ActiveModel n'existe ici, `existing` est prêté
    // en lecture seule ; `Err` traverse en Application sans code `MRD-*`.
    E::before_delete(&existing, principal).map_err(RestError::Application)?;

    // Depuis l'arbitrage `rest/core.sdd` 2026-09-27, le résultat SQL est lu : la ligne relue a
    // disparu entre la relecture et le `DELETE` (course tolérée, pas de verrou) quand
    // `rows_affected` vaut `0` — c'est un `404`, plus jamais un `204` muet sur rien de supprimé.
    let result = E::delete_by_id(id).exec(db).await?;
    if result.rows_affected == 0 {
        return Err(RestError::NotFound);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Conversion des `Scenario` de `./core.sdd` dont la preuve est attendue inline ici (mapping
    //! `Tasks`). Les fixtures `recipe`/`ingredient` sont copiées de `./mod.rs` ; les autres portent
    //! chaque politique ou chaque hook requis. Les preuves rattachées existantes (seize tests de
    //! `./mod.rs`, deux de `../mcp/registry.rs`) ne sont pas dupliquées ici.

    use super::*;
    use crate::auth::PrincipalSource;
    use crate::migration::Migrator;
    use crate::users::{ADMIN_GROUP_NAME, sync_group_memberships, user};
    use sea_orm::ActiveValue::{NotSet, Set, Unchanged};
    use sea_orm::{ConnectionTrait, Database, EntityTrait, Schema};
    use sea_orm_migration::MigratorTrait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Lecture et écriture `OwnerOnly` avec colonne propriétaire et colonne de filtre déclarées
    /// (copie de la fixture `recipe` de `./mod.rs`).
    mod recipe {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "recipes")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
            pub owner_id: i32,
            pub category: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "recipes"
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
            fn filter_column() -> Option<Column> {
                Some(Column::Category)
            }
        }
    }

    /// Lecture `Group("editors")`, écriture `AdminOnly`, sans colonne propriétaire (copie de la
    /// fixture `ingredient` de `./mod.rs`) — porte les oracle fermés `Group` (lecture) et
    /// `AdminOnly` (écriture).
    mod ingredient {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
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

    /// Lecture et écriture `AdminOnly` — pendant lecture de l'oracle fermé `AdminOnly`.
    mod secret {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "secrets")]
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
                "secrets"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    /// Colonne propriétaire déclarée sous `write_policy Public` — fixture de l'arbitrage
    /// 2026-09-27 (injection indépendante de la politique d'écriture) et du listage `Public`.
    mod public_owned {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "public_owneds")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub owner_id: i32,
            pub label: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "public_owneds"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn owner_column() -> Option<Column> {
                Some(Column::OwnerId)
            }
        }
    }

    /// Colonne propriétaire déclarée sous `write_policy Group("editors")` — reconduction hors
    /// `OwnerOnly` (arbitrage 2026-09-27) et oracle fermé `Group` en écriture.
    mod group_owned {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "group_owned")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub owner_id: i32,
            pub label: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "group_owned"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Group("editors")
            }
            fn owner_column() -> Option<Column> {
                Some(Column::OwnerId)
            }
        }
    }

    /// Écriture `AdminOnly` avec `before_create` espion — scenario « create refusé : ni hook, ni
    /// écriture ». Compteur propre à ce seul test (pas de collision entre tests parallèles).
    static WATCHED_CREATE_CALLS: AtomicUsize = AtomicUsize::new(0);

    mod watched {
        use super::WATCHED_CREATE_CALLS;
        use crate::resource::{AccessPolicy, HookError, MiryadResource};
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};
        use std::sync::atomic::Ordering;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "watched")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub label: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "watched"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn owner_column() -> Option<Column> {
                None
            }

            fn before_create(
                active: ActiveModel,
                _principal: &crate::auth::AuthPrincipal,
            ) -> Result<ActiveModel, HookError> {
                WATCHED_CREATE_CALLS.fetch_add(1, Ordering::SeqCst);
                Ok(active)
            }
        }
    }

    /// Écriture `OwnerOnly` avec `before_create` hostile : re-pose la PK à `77` et l'owner sur un
    /// utilisateur inconnu, et majuscule le label (mutation licite témoin). Fixture de la spec
    /// (`Tasks`) pour le scenario « hook hostile » de la création.
    mod forged {
        use crate::resource::{AccessPolicy, HookError, MiryadResource};
        use sea_orm::ActiveValue::Set;
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "forged")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub owner_id: i32,
            pub label: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "forged"
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

            fn before_create(
                active: ActiveModel,
                _principal: &crate::auth::AuthPrincipal,
            ) -> Result<ActiveModel, HookError> {
                let mut active = active;
                active.id = Set(77);
                active.owner_id = Set(999_999);
                active.label = Set("FORGE".to_string());
                Ok(active)
            }
        }
    }

    /// Colonne `name` sous contrainte `UNIQUE` (index posé à la main, la fixture de test n'a pas
    /// de migration) — scenario « create en contrainte de colonne ».
    mod unique_thing {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "unique_things")]
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
                "unique_things"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    /// `before_create` espion attestant que `update` n'appelle jamais le hook de création.
    /// Compteur propre à ce seul test.
    static NEVER_CREATE_CALLS: AtomicUsize = AtomicUsize::new(0);

    mod never_create {
        use super::NEVER_CREATE_CALLS;
        use crate::resource::{AccessPolicy, HookError, MiryadResource};
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};
        use std::sync::atomic::Ordering;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "never_creates")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub label: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "never_creates"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn owner_column() -> Option<Column> {
                None
            }

            fn before_create(
                active: ActiveModel,
                _principal: &crate::auth::AuthPrincipal,
            ) -> Result<ActiveModel, HookError> {
                NEVER_CREATE_CALLS.fetch_add(1, Ordering::SeqCst);
                Ok(active)
            }
        }
    }

    /// Fixture « course à l'update » pilotée (`Tasks` : « ou fixture pilotée ») : son
    /// `before_save` (exécuté par `SeaORM` après le hook `before_update` de `core::update`, avant
    /// l'émission de l'`UPDATE`) supprime la table — la ligne relue par `core::update` a donc
    /// disparu entre la relecture et l'écriture, sans fenêtre de timing.
    mod vanishing {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "vanishings")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub label: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        #[async_trait::async_trait]
        impl ActiveModelBehavior for ActiveModel {
            async fn before_save<C>(self, db: &C, insert: bool) -> Result<Self, DbErr>
            where
                C: ConnectionTrait,
            {
                if !insert {
                    db.execute_unprepared("DELETE FROM vanishings").await?;
                }
                Ok(self)
            }
        }

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "vanishings"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    /// Fixture « course à la suppression » pilotée : le hook `before_delete` (seul point de
    /// passage entre la relecture et l'émission du `DELETE` dans `core::delete`) déclenche le
    /// balai du test (tâche live sur le même runtime, saveur multi-thread, même pool) et attend
    /// sa confirmation — le `delete_by_id` de `core::delete` touche alors `0` ligne.
    static VANISH_REQ: std::sync::OnceLock<std::sync::mpsc::Sender<()>> = std::sync::OnceLock::new();
    static VANISH_DONE: std::sync::OnceLock<std::sync::Mutex<std::sync::mpsc::Receiver<()>>> =
        std::sync::OnceLock::new();

    mod vanish_del {
        use super::{VANISH_DONE, VANISH_REQ};
        use crate::resource::{AccessPolicy, HookError, MiryadResource};
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "vanish_dels")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub label: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "vanish_dels"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn owner_column() -> Option<Column> {
                None
            }

            fn before_delete(
                _existing: &Self::Model,
                _principal: &crate::auth::AuthPrincipal,
            ) -> Result<(), HookError> {
                VANISH_REQ
                    .get()
                    .expect("le test pose le déclencheur avant l'appel")
                    .send(())
                    .map_err(|_| HookError::new("balai mort"))?;
                VANISH_DONE
                    .get()
                    .expect("le test pose la voie d'acquittement avant l'appel")
                    .lock()
                    .expect("acquittement non empoisonné")
                    .recv()
                    .map_err(|_| HookError::new("balai sans acquittement"))?;
                Ok(())
            }
        }
    }

    /// Rejette un label vide — porte le rejet de hook traduit côté MCP (scenario « parité MCP »).
    #[cfg(feature = "mcp")]
    mod parity_hooked {
        use crate::resource::{AccessPolicy, HookError, MiryadResource};
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "parity_hookeds")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub label: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "parity_hookeds"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn owner_column() -> Option<Column> {
                None
            }

            fn before_create(
                active: ActiveModel,
                _principal: &crate::auth::AuthPrincipal,
            ) -> Result<ActiveModel, HookError> {
                let label = match &active.label {
                    sea_orm::ActiveValue::Set(v) | sea_orm::ActiveValue::Unchanged(v) => v.clone(),
                    sea_orm::ActiveValue::NotSet => String::new(),
                };
                if label.is_empty() {
                    return Err(HookError::with_code("PARITY-001", "label must not be empty"));
                }
                Ok(active)
            }
        }
    }

    /// Base migrée des seules tables `crate::migration` (aucune table d'entité).
    async fn migrated_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    /// Crée à la volée la table d'une fixture (même méthode que `./mod.rs`).
    async fn create_entity_table<E: EntityTrait>(db: &DatabaseConnection, entity: E) {
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(entity))
            .await
            .expect("fixture table creates");
    }

    fn principal(subject: &str) -> AuthPrincipal {
        AuthPrincipal {
            subject: subject.to_string(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::ApiToken { token_id: 0 },
        }
    }

    async fn member_of(db: &DatabaseConnection, subject: &str, group: &str) -> AuthPrincipal {
        let user = resolve_user(db, subject, None).await.expect("resolve");
        sync_group_memberships(db, user.id, &[group.to_string()])
            .await
            .expect("sync");
        principal(subject)
    }

    // ---------------------------------------------------------------- listage

    /// Scenario « Provisionnement du principal avant toute décision et avant tout refus » —
    /// `list` sur une table d'entité absente refuse `403` (politique `Group`) mais a déjà posé la
    /// ligne `miryad_users` du `subject` : `resolve_user` est premier, refusé ou pas.
    #[tokio::test]
    async fn list_provisionne_puis_refuse_sur_table_absente() {
        let db = migrated_db().await;
        // La table `ingredients` n'est pas créée : seul un refus décidé avant toute requête
        // (et non une erreur de table) peut sortir de cet appel.
        let result = list::<ingredient::Entity>(&db, &principal("stranger"), None, None, None).await;
        assert!(
            matches!(result, Err(RestError::Forbidden)),
            "le refus se décide avant la table, pas après : {result:?}"
        );
        let provisioned = user::Entity::find()
            .filter(user::Column::Subject.eq("stranger"))
            .one(&db)
            .await
            .expect("miryad_users is migrated");
        assert!(
            provisioned.is_some(),
            "resolve_user a tourné avant la décision : la ligne du subject existe malgré le 403"
        );
    }

    /// Scenario « Tables internes absentes : `RestError::Database` avant toute politique » — les
    /// cinq fonctions de `core.rs` échouent dès `resolve_user`, aucune politique consultable.
    #[tokio::test]
    async fn toutes_operations_sans_tables_utilisateurs_sont_database() {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        let alice = principal("alice");

        assert!(matches!(
            list::<recipe::Entity>(&db, &alice, None, None, None).await,
            Err(RestError::Database(_))
        ));
        assert!(matches!(
            get::<recipe::Entity>(&db, &alice, 1).await,
            Err(RestError::Database(_))
        ));
        let body = recipe::Model {
            id: 0,
            title: "Tarte".to_string(),
            owner_id: 0,
            category: "dessert".to_string(),
        };
        assert!(matches!(
            create::<recipe::Entity>(&db, &alice, body.clone()).await,
            Err(RestError::Database(_))
        ));
        assert!(matches!(
            update::<recipe::Entity>(&db, &alice, 1, body).await,
            Err(RestError::Database(_))
        ));
        assert!(matches!(
            delete::<recipe::Entity>(&db, &alice, 1).await,
            Err(RestError::Database(_))
        ));
    }

    /// Scenario « Une table d'entité absente refuse d'abord l'appelant refusé, ne trahit rien
    /// d'autre » — les deux branches sont distinctement observables sur la même connexion
    /// non-migrée pour la table de l'entité.
    #[tokio::test]
    async fn list_refuse_ne_touche_pas_la_table_absente() {
        let db = migrated_db().await;
        let editor = member_of(&db, "editor", "editors").await;

        // Appelant refusé : Forbidden sans que la table absente soit jamais interrogée.
        assert!(matches!(
            list::<ingredient::Entity>(&db, &principal("stranger"), None, None, None).await,
            Err(RestError::Forbidden)
        ));
        // Appelant autorisé sur la même connexion : la table absente devient une DbErr ordinaire.
        let allowed = list::<ingredient::Entity>(&db, &editor, None, None, None).await;
        assert!(
            matches!(allowed, Err(RestError::Database(_))),
            "l'appelant autorisé bute sur la table absente : {allowed:?}"
        );
    }

    /// Scenario « Listage `OwnerOnly` : le non-admin ne voit que ses lignes, l'admin voit tout ».
    #[tokio::test]
    async fn list_owner_only_non_admin_vs_admin() {
        let db = migrated_db().await;
        create_entity_table(&db, recipe::Entity).await;
        let alice = principal("alice");
        let bob = principal("bob");
        let admin = member_of(&db, "admin-user", ADMIN_GROUP_NAME).await;

        for (token, title) in [(&alice, "Tarte"), (&bob, "Soupe")] {
            create::<recipe::Entity>(
                &db,
                token,
                recipe::Model {
                    id: 0,
                    title: title.to_string(),
                    owner_id: 0,
                    category: "plat".to_string(),
                },
            )
            .await
            .expect("create succeeds");
        }

        let mine = list::<recipe::Entity>(&db, &alice, None, None, None)
            .await
            .expect("list succeeds");
        assert_eq!(
            mine.total_items, 1,
            "FilterByOwner ne rend que la ligne du demandeur"
        );
        assert!(
            mine.items.iter().all(|r| r.owner_id == mine.items[0].owner_id),
            "aucune ligne d'autrui ne traverse la condition owner"
        );

        let every = list::<recipe::Entity>(&db, &admin, None, None, None)
            .await
            .expect("list succeeds");
        assert_eq!(every.total_items, 2, "l'admin rend Unrestricted sans filtre SQL");
    }

    /// Scenario « Listage `Public` : aucune condition propriétaire n'est ajoutée » — Unrestricted
    /// (même branche que `list_pagination_returns_requested_page` de `./mod.rs`).
    #[tokio::test]
    async fn list_public_voit_toutes_les_lignes() {
        let db = migrated_db().await;
        create_entity_table(&db, public_owned::Entity).await;
        let alice = principal("alice");
        let bob = principal("bob");

        for (token, label) in [(&alice, "une"), (&bob, "deux")] {
            create::<public_owned::Entity>(
                &db,
                token,
                public_owned::Model {
                    id: 0,
                    owner_id: 0,
                    label: label.to_string(),
                },
            )
            .await
            .expect("create succeeds");
        }

        let page = list::<public_owned::Entity>(&db, &bob, None, None, None)
            .await
            .expect("list succeeds");
        assert_eq!(
            page.total_items, 2,
            "la ligne d'autrui compte dans le listage Public"
        );
    }

    /// Scenario « Filtre transmis à une entité sans colonne de filtre : ignoré silencieusement »
    /// (complément inline d'avec `list_filter_combines_with_owner_restriction` de `./mod.rs`).
    #[tokio::test]
    async fn list_filtre_ignore_sans_colonne_declaree() {
        let db = migrated_db().await;
        create_entity_table(&db, ingredient::Entity).await;
        ingredient::Entity::insert_many([
            ingredient::ActiveModel {
                id: NotSet,
                name: Set("Sel".to_string()),
            },
            ingredient::ActiveModel {
                id: NotSet,
                name: Set("Poivre".to_string()),
            },
        ])
        .exec(&db)
        .await
        .expect("rows seed");
        let editor = member_of(&db, "editor", "editors").await;

        let page = list::<ingredient::Entity>(&db, &editor, None, None, Some("valeur-sans-colonne"))
            .await
            .expect("le filtre sans colonne déclarée ne provoque aucune erreur");
        assert_eq!(
            page.total_items, 2,
            "le paramètre est ignoré, la réponse est complète"
        );
    }

    /// Scenario « Pagination normalisée : `0`, absence et excès rendus bornés » (complément
    /// inline d'avec `list_pagination_returns_requested_page` de `./mod.rs`, qui couvre l'écho
    /// des valeurs demandées).
    #[tokio::test]
    async fn list_pagination_bornee() {
        let db = migrated_db().await;
        create_entity_table(&db, recipe::Entity).await;
        let alice = principal("alice");
        for title in ["Un", "Deux", "Trois"] {
            create::<recipe::Entity>(
                &db,
                &alice,
                recipe::Model {
                    id: 0,
                    title: title.to_string(),
                    owner_id: 0,
                    category: "plat".to_string(),
                },
            )
            .await
            .expect("create succeeds");
        }

        let zeroed = list::<recipe::Entity>(&db, &alice, Some(0), Some(0), None)
            .await
            .expect("list succeeds");
        assert_eq!(zeroed.page, 1);
        assert_eq!(zeroed.per_page, 1);

        let huge = list::<recipe::Entity>(&db, &alice, None, Some(50_000), None)
            .await
            .expect("list succeeds");
        assert_eq!(huge.per_page, crate::query::MAX_PER_PAGE);
        assert!(huge.items.len() <= usize::try_from(crate::query::MAX_PER_PAGE).expect("fits"));
    }

    /// Scenario « Page au-delà de la dernière : `items` vide, totaux réels, page hors limite
    /// conservée ».
    #[tokio::test]
    async fn list_page_hors_limites_vide_avec_totaux() {
        let db = migrated_db().await;
        create_entity_table(&db, recipe::Entity).await;
        let alice = principal("alice");
        for title in ["Un", "Deux", "Trois"] {
            create::<recipe::Entity>(
                &db,
                &alice,
                recipe::Model {
                    id: 0,
                    title: title.to_string(),
                    owner_id: 0,
                    category: "plat".to_string(),
                },
            )
            .await
            .expect("create succeeds");
        }

        let page = list::<recipe::Entity>(&db, &alice, Some(99), Some(1), None)
            .await
            .expect("aucune erreur hors limite");
        assert_eq!(page.page, 99, "la page demandée est conservée, pas ramenée");
        assert_eq!(page.items, [] as [recipe::Model; 0]);
        assert_eq!(page.total_items, 3);
    }

    /// Scenario « Table vide : totaux `0`, `items` vide, page conservée ».
    #[tokio::test]
    async fn list_table_vide_totaux_zero() {
        let db = migrated_db().await;
        create_entity_table(&db, recipe::Entity).await;

        let page = list::<recipe::Entity>(&db, &principal("alice"), None, None, None)
            .await
            .expect("table vide n'est pas une erreur");
        assert_eq!(page.total_items, 0);
        assert_eq!(page.total_pages, 0, "division majorante : table vide fait 0 page");
        assert_eq!(page.items, [] as [recipe::Model; 0]);
    }

    // -------------------------------------------------------------------- get

    /// Scenario « get d'un inconnu sous `OwnerOnly` : `404`, résidu assumé de l'oracle » —
    /// `static_verdict` rend `None`, la relecture rend `None`, `can_read` n'est jamais consulté.
    #[tokio::test]
    async fn get_owner_only_introuvable_est_not_found_sans_can_read() {
        let db = migrated_db().await;
        create_entity_table(&db, recipe::Entity).await;

        assert!(matches!(
            get::<recipe::Entity>(&db, &principal("alice"), 999_999).await,
            Err(RestError::NotFound)
        ));
    }

    /// Scenario « get d'un inconnu sous `AdminOnly`/`Group` : `403` sans jamais toucher la table »
    /// — preuve par table d'entité absente : sans oracle, l'appel rendrait `Database`.
    #[tokio::test]
    async fn get_admin_only_et_group_introuvable_refuse_avant_toute_lecture() {
        let db = migrated_db().await;
        // Tables `secrets` et `ingredients` volontairement absentes : tout accès à la table de
        // l'entité se traduirait en RestError::Database, jamais observable si l'oracle ferme.
        assert!(matches!(
            get::<secret::Entity>(&db, &principal("alice"), 1).await,
            Err(RestError::Forbidden)
        ));
        assert!(matches!(
            get::<ingredient::Entity>(&db, &principal("alice"), 1).await,
            Err(RestError::Forbidden)
        ));
    }

    /// Scenario « get existant : `Ok`, `RestError::Forbidden` au non-autorisé » — owner, admin et
    /// tiers sur la même ligne `OwnerOnly`.
    #[tokio::test]
    async fn get_owner_passe_etranger_refuse() {
        let db = migrated_db().await;
        create_entity_table(&db, recipe::Entity).await;
        let alice = principal("alice");
        let created = create::<recipe::Entity>(
            &db,
            &alice,
            recipe::Model {
                id: 0,
                title: "Tarte".to_string(),
                owner_id: 0,
                category: "dessert".to_string(),
            },
        )
        .await
        .expect("create succeeds");
        let admin = member_of(&db, "admin-user", ADMIN_GROUP_NAME).await;

        assert_eq!(
            get::<recipe::Entity>(&db, &alice, created.id)
                .await
                .expect("owner lit sa ligne")
                .title,
            "Tarte"
        );
        assert!(
            get::<recipe::Entity>(&db, &admin, created.id)
                .await
                .is_ok_and(|r| r.id == created.id),
            "l'admin lit aussi la ligne"
        );
        assert!(matches!(
            get::<recipe::Entity>(&db, &principal("bob"), created.id).await,
            Err(RestError::Forbidden)
        ));
    }

    // ----------------------------------------------------------------- create

    /// Scenario « create refusé : ni hook, ni écriture, `MRD-REST-002` » — l'espion ne compte
    /// rien, la table reste vide.
    #[tokio::test]
    async fn create_refuse_appelle_ni_hook_ni_insert() {
        let db = migrated_db().await;
        create_entity_table(&db, watched::Entity).await;

        assert!(matches!(
            create::<watched::Entity>(
                &db,
                &principal("alice"),
                watched::Model {
                    id: 0,
                    label: "x".to_string()
                }
            )
            .await,
            Err(RestError::Forbidden)
        ));
        assert_eq!(
            WATCHED_CREATE_CALLS.load(Ordering::SeqCst),
            0,
            "le hook n'est jamais appelé quand can_create refuse"
        );
        assert_eq!(
            watched::Entity::find()
                .all(&db)
                .await
                .expect("table readable")
                .len(),
            0,
            "l'insertion n'a jamais été tentée"
        );
    }

    /// Scenarios « hook hostile : PK et owner qu'il re-forge sont annulés par les invariants » et
    /// « create : la mutation du hook est écrite, mais les invariants restent après lui » (partie
    /// inline : PK attribuée, owner de l'appelant, mutation `FORGE` visible).
    #[tokio::test]
    async fn create_hook_hostile_ne_contourne_les_invariants() {
        let db = migrated_db().await;
        create_entity_table(&db, forged::Entity).await;
        let bob = resolve_user(&db, "bob", None).await.expect("resolve");
        forged::Entity::insert(forged::ActiveModel {
            id: Set(77),
            owner_id: Set(bob.id),
            label: Set("gardee".to_string()),
        })
        .exec(&db)
        .await
        .expect("seed row 77");

        let created = create::<forged::Entity>(
            &db,
            &principal("alice"),
            forged::Model {
                id: 0,
                owner_id: 0,
                label: "alpha".to_string(),
            },
        )
        .await
        .expect("create succeeds");

        assert_ne!(
            created.id, 77,
            "la PK re-posée par le hook est annulée par not_set"
        );
        let alice = user::Entity::find()
            .filter(user::Column::Subject.eq("alice"))
            .one(&db)
            .await
            .expect("readable")
            .expect("alice provisioned");
        assert_eq!(
            created.owner_id, alice.id,
            "l'owner foré par le hook (999_999) est annulé par l'injection postérieure"
        );
        assert_eq!(created.label, "FORGE", "la mutation licite du hook survit");
        let untouched = forged::Entity::find_by_id(77)
            .one(&db)
            .await
            .expect("readable")
            .expect("la ligne tierce existe");
        assert_eq!(
            untouched.label, "gardee",
            "aucune écriture n'a atterri sur la PK forgée"
        );
    }

    /// Scenario « create : `OwnerOnly` force l'attribut propriétaire, corps muet ou menteur » et
    /// la revendication PK de « create : la mutation du hook est écrite » : PK cliente ignorée,
    /// corps omettant l'owner (`0`) comme corps menteur (`999_999`) portent l'id de l'appelant.
    #[tokio::test]
    async fn create_ignore_la_pk_du_corps() {
        let db = migrated_db().await;
        create_entity_table(&db, recipe::Entity).await;
        let bob = resolve_user(&db, "bob", None).await.expect("resolve");
        recipe::Entity::insert(recipe::ActiveModel {
            id: Set(4242),
            title: Set("gardee".to_string()),
            owner_id: Set(bob.id),
            category: Set("plat".to_string()),
        })
        .exec(&db)
        .await
        .expect("seed row 4242");
        let alice = principal("alice");
        let alice_id = resolve_user(&db, "alice", None).await.expect("resolve").id;

        let liar = create::<recipe::Entity>(
            &db,
            &alice,
            recipe::Model {
                id: 4242,
                title: "menteur".to_string(),
                owner_id: 999_999,
                category: "dessert".to_string(),
            },
        )
        .await
        .expect("create succeeds");
        assert_ne!(liar.id, 4242, "la PK du corps est retirée, la base attribue");
        assert_eq!(liar.owner_id, alice_id, "le owner menteur du corps est ignoré");

        let silent = create::<recipe::Entity>(
            &db,
            &alice,
            recipe::Model {
                id: 0,
                title: "muet".to_string(),
                owner_id: 0,
                category: "dessert".to_string(),
            },
        )
        .await
        .expect("create succeeds");
        assert_eq!(
            silent.owner_id, alice_id,
            "le corps omettant l'owner reçoit celui de l'appelant"
        );

        let untouched = recipe::Entity::find_by_id(4242)
            .one(&db)
            .await
            .expect("readable")
            .expect("la ligne tierce existe");
        assert_eq!(untouched.title, "gardee");
    }

    /// Scenario « create hors `OwnerOnly` avec colonne propriétaire déclarée : l'injection
    /// s'applique quand même » (arbitré 2026-09-27) — TEST ROUGE avant implémentation.
    #[tokio::test]
    async fn create_injecte_owner_meme_hors_owner_only_si_colonne_declaree() {
        let db = migrated_db().await;
        create_entity_table(&db, public_owned::Entity).await;
        let alice_id = resolve_user(&db, "alice", None).await.expect("resolve").id;

        let created = create::<public_owned::Entity>(
            &db,
            &principal("alice"),
            public_owned::Model {
                id: 0,
                owner_id: 999_999,
                label: "x".to_string(),
            },
        )
        .await
        .expect("create succeeds");
        assert_eq!(
            created.owner_id, alice_id,
            "write_policy Public avec owner_column déclarée : l'injection s'applique quand même"
        );
    }

    /// Scenario « create en contrainte de colonne : `DbErr` devient `MRD-REST-003` ».
    #[tokio::test]
    async fn create_insertion_en_contrainte_est_database() {
        let db = migrated_db().await;
        create_entity_table(&db, unique_thing::Entity).await;
        db.execute_unprepared("CREATE UNIQUE INDEX unique_things_name_key ON unique_things (name)")
            .await
            .expect("unique index creates");

        create::<unique_thing::Entity>(
            &db,
            &principal("alice"),
            unique_thing::Model {
                id: 0,
                name: "sel".to_string(),
            },
        )
        .await
        .expect("first insert succeeds");
        let conflicting = create::<unique_thing::Entity>(
            &db,
            &principal("alice"),
            unique_thing::Model {
                id: 0,
                name: "sel".to_string(),
            },
        )
        .await;
        assert!(
            matches!(conflicting, Err(RestError::Database(_))),
            "la contrainte brute devient Database (MRD-REST-003), jamais un 400 fabriqué ici : {conflicting:?}"
        );
    }

    // ------------------------------------------- ORDER BY clé primaire (arbitré 2026-09-29)

    /// `Must` « Toute liste est ordonnée par la clé primaire croissante » (arbitré par Sébastien
    /// le 2026-09-29) — preuve d'émission par `sea_orm::MockDatabase` : le `SELECT` paginé doit
    /// porter `ORDER BY` sur la colonne de clé primaire avant `paginate`. TEST ROUGE avant
    /// implémentation : sous `SQLite` toute table `rowid` est balayée en ordre de `rowid` — le
    /// contenu est identique avec ou sans clause, seule la clause émise est observable.
    #[tokio::test]
    async fn list_applique_order_by_sur_la_cle_primaire() {
        use sea_orm::{DbBackend, MockDatabase};
        use std::collections::BTreeMap;

        let alice = user::Model {
            id: 7,
            subject: "alice".to_string(),
            email: None,
            display_name: None,
            created_at: chrono::Utc::now(),
        };
        let count_row = BTreeMap::from([("num_items".to_string(), sea_orm::Value::BigInt(Some(5)))]);
        let db = MockDatabase::new(DbBackend::Sqlite)
            .append_query_results([[alice]])
            .append_query_results([[count_row]])
            .append_query_results::<public_owned::Model, _, _>([[]])
            .into_connection();

        let page = list::<public_owned::Entity>(&db, &principal("alice"), Some(1), Some(2), None)
            .await
            .expect("le mock scripte les trois requêtes du listage");
        assert_eq!(page.total_items, 5, "le `COUNT` scripté vaut 5");

        let page_sql = db
            .into_transaction_log()
            .iter()
            .flat_map(sea_orm::Transaction::statements)
            .map(ToString::to_string)
            // Le SELECT de la page : pas le COUNT englobant, qui ne porte jamais de LIMIT.
            .find(|sql| {
                sql.starts_with("SELECT") && sql.contains(r#"FROM "public_owneds""#) && !sql.contains("COUNT")
            })
            .expect("le SELECT de la page paginée a été émis");
        assert!(
            page_sql.contains(r#"ORDER BY "public_owneds"."id" ASC"#),
            "`ORDER BY` sur la clé primaire est appliqué avant @paginate : {page_sql}"
        );
    }

    /// `Must` « Toute liste est ordonnée par la clé primaire croissante » — verrou de pagination
    /// déterministe sur `SQLite` réel : trois pages consécutives à `per_page` 2 couvrent les cinq
    /// lignes sans saut ni doublon, en ordre croissant de clé primaire. Consigné au rapport :
    /// sous `SQLite` ce test est vert même sans clause (balayage `rowid`) — la preuve rouge est
    /// le test de statement ci-dessus ; celui-ci verrouille le contrat rendu.
    #[tokio::test]
    async fn list_pagination_deux_pages_sans_saut_ni_doublon() {
        let db = migrated_db().await;
        create_entity_table(&db, recipe::Entity).await;
        let alice = principal("alice");
        let mut seeded = Vec::new();
        for title in ["Un", "Deux", "Trois", "Quatre", "Cinq"] {
            seeded.push(
                create::<recipe::Entity>(
                    &db,
                    &alice,
                    recipe::Model {
                        id: 0,
                        title: title.to_string(),
                        owner_id: 0,
                        category: "plat".to_string(),
                    },
                )
                .await
                .expect("create succeeds")
                .id,
            );
        }

        let mut seen: Vec<i32> = Vec::new();
        for page_index in 1..=3 {
            let page = list::<recipe::Entity>(&db, &alice, Some(page_index), Some(2), None)
                .await
                .expect("list succeeds");
            assert_eq!(page.total_items, 5, "les totaux décrivent les cinq lignes");
            for item in &page.items {
                assert!(
                    !seen.contains(&item.id),
                    "la ligne {} est dupliquée entre deux pages",
                    item.id
                );
                seen.push(item.id);
            }
        }
        seeded.sort_unstable();
        assert_eq!(seen.len(), 5, "aucune ligne n'est sautée sur les trois pages");
        assert_eq!(
            seen, seeded,
            "le parcours des pages suit la clé primaire croissante"
        );
    }

    // ----------------------------------------------------------------- update

    /// Scenario « update `OwnerOnly` d'un inconnu : `NotFound`, résidu assumé de l'oracle ».
    #[tokio::test]
    async fn update_owner_only_introuvable_est_not_found_sans_can_write() {
        let db = migrated_db().await;
        create_entity_table(&db, recipe::Entity).await;

        assert!(matches!(
            update::<recipe::Entity>(
                &db,
                &principal("alice"),
                999_999,
                recipe::Model {
                    id: 999_999,
                    title: "fantôme".to_string(),
                    owner_id: 0,
                    category: "plat".to_string(),
                }
            )
            .await,
            Err(RestError::NotFound)
        ));
    }

    /// Scenario « update `AdminOnly`/`Group` d'un inconnu : `403` sans jamais toucher la table »
    /// — TEST ROUGE avant implémentation (preuve par table absente).
    #[tokio::test]
    async fn update_admin_only_et_group_introuvable_refuse_avant_toute_lecture() {
        let db = migrated_db().await;
        // Tables des entités volontairement absentes : sans oracle, find_by_id rendrait Database.
        assert!(matches!(
            update::<ingredient::Entity>(
                &db,
                &principal("alice"),
                1,
                ingredient::Model {
                    id: 1,
                    name: "Sel".to_string()
                }
            )
            .await,
            Err(RestError::Forbidden)
        ));
        assert!(matches!(
            update::<group_owned::Entity>(
                &db,
                &principal("alice"),
                1,
                group_owned::Model {
                    id: 1,
                    owner_id: 0,
                    label: "x".to_string()
                }
            )
            .await,
            Err(RestError::Forbidden)
        ));
    }

    /// Scenario « update d'une ligne existante refusée : le corps n'est jamais examiné » — la
    /// décision est prise sur `existing` : un corps dont la PK est inconnue ne déplace pas la
    /// réponse vers `NotFound`, l'appelant refusé reçoit `Forbidden`.
    #[tokio::test]
    async fn update_ligne_existante_refusee_corps_jamais_examine() {
        let db = migrated_db().await;
        create_entity_table(&db, recipe::Entity).await;
        let created = create::<recipe::Entity>(
            &db,
            &principal("alice"),
            recipe::Model {
                id: 0,
                title: "Tarte".to_string(),
                owner_id: 0,
                category: "dessert".to_string(),
            },
        )
        .await
        .expect("create succeeds");

        assert!(matches!(
            update::<recipe::Entity>(
                &db,
                &principal("bob"),
                created.id,
                recipe::Model {
                    id: 999_998,
                    title: "détournée".to_string(),
                    owner_id: 0,
                    category: "dessert".to_string(),
                }
            )
            .await,
            Err(RestError::Forbidden)
        ));
    }

    /// Scenario « update : les colonnes du corps sont écrites, PK du chemin seule cible »
    /// (volet cible : la ligne tierce portant la PK du corps reste intacte).
    #[tokio::test]
    async fn update_force_l_id_du_chemin() {
        let db = migrated_db().await;
        create_entity_table(&db, recipe::Entity).await;
        let alice = principal("alice");
        let a = create::<recipe::Entity>(
            &db,
            &alice,
            recipe::Model {
                id: 0,
                title: "A".to_string(),
                owner_id: 0,
                category: "plat".to_string(),
            },
        )
        .await
        .expect("create A");
        let b = create::<recipe::Entity>(
            &db,
            &principal("bob"),
            recipe::Model {
                id: 0,
                title: "B".to_string(),
                owner_id: 0,
                category: "plat".to_string(),
            },
        )
        .await
        .expect("create B");

        let updated = update::<recipe::Entity>(
            &db,
            &alice,
            a.id,
            recipe::Model {
                id: b.id,
                title: "A modifiée".to_string(),
                owner_id: 999_999,
                category: "dessert".to_string(),
            },
        )
        .await
        .expect("update succeeds");
        assert_eq!(
            updated.id, a.id,
            "la cible est l'id du chemin, jamais la PK du corps"
        );
        assert_eq!(updated.title, "A modifiée");

        let other = recipe::Entity::find_by_id(b.id)
            .one(&db)
            .await
            .expect("readable")
            .expect("la ligne tierce existe");
        assert_eq!(other.title, "B", "la ligne portant la PK du corps est intacte");
    }

    /// Volet « toutes les colonnes du corps sont écrites » du même scenario — `mark_all_set`
    /// remplit la `SET` que `SeaORM` seul ne prendrait pas dans les `Unchanged`.
    #[tokio::test]
    async fn update_ecrit_toutes_les_colonnes_du_corps() {
        let db = migrated_db().await;
        create_entity_table(&db, recipe::Entity).await;
        let created = create::<recipe::Entity>(
            &db,
            &principal("alice"),
            recipe::Model {
                id: 0,
                title: "Tarte".to_string(),
                owner_id: 0,
                category: "dessert".to_string(),
            },
        )
        .await
        .expect("create succeeds");

        let updated = update::<recipe::Entity>(
            &db,
            &principal("alice"),
            created.id,
            recipe::Model {
                id: created.id,
                title: "Tarte modifiée".to_string(),
                owner_id: 0,
                category: "salé".to_string(),
            },
        )
        .await
        .expect("update succeeds");
        assert_eq!(updated.title, "Tarte modifiée");
        assert_eq!(
            updated.category, "salé",
            "toute colonne du corps passe par la SET"
        );
        assert_eq!(updated.owner_id, created.owner_id);
    }

    /// Scenario « update hors `OwnerOnly` avec colonne propriétaire déclarée : la propriété
    /// survit quand même » (arbitré 2026-09-27) — TEST ROUGE avant implémentation.
    #[tokio::test]
    async fn update_reconduit_owner_meme_hors_owner_only_si_colonne_declaree() {
        let db = migrated_db().await;
        create_entity_table(&db, group_owned::Entity).await;
        let alice = resolve_user(&db, "alice", None).await.expect("resolve");
        let inserted = group_owned::Entity::insert(group_owned::ActiveModel {
            id: NotSet,
            owner_id: Set(alice.id),
            label: Set("initial".to_string()),
        })
        .exec(&db)
        .await
        .expect("seed row");

        // Membre du groupe autorisé à écrire tente de se transférer la ligne via le corps.
        let bob = member_of(&db, "bob", "editors").await;
        update::<group_owned::Entity>(
            &db,
            &bob,
            inserted.last_insert_id,
            group_owned::Model {
                id: inserted.last_insert_id,
                owner_id: bob_subject_id(&db).await,
                label: "détourné".to_string(),
            },
        )
        .await
        .expect("le membre écrit");
        let row = group_owned::Entity::find_by_id(inserted.last_insert_id)
            .one(&db)
            .await
            .expect("readable")
            .expect("la ligne existe");
        assert_eq!(
            row.owner_id, alice.id,
            "la reconduction depuis existing s'applique hors OwnerOnly dès que owner_column est déclarée"
        );
        assert_eq!(row.label, "détourné", "les autres colonnes du corps sont écrites");
    }

    async fn bob_subject_id(db: &DatabaseConnection) -> i32 {
        user::Entity::find()
            .filter(user::Column::Subject.eq("bob"))
            .one(db)
            .await
            .expect("readable")
            .expect("bob provisioned")
            .id
    }

    /// Volet « un espion de hook ne bouge pas » du scenario « update : les colonnes du corps
    /// sont écrites » : `before_create` n'est jamais consulté par `update`, y compris quand la
    /// mutation réussit.
    #[tokio::test]
    async fn update_n_appelle_jamais_before_create() {
        let db = migrated_db().await;
        create_entity_table(&db, never_create::Entity).await;
        let inserted = never_create::Entity::insert(never_create::ActiveModel {
            id: NotSet,
            label: Set("initial".to_string()),
        })
        .exec(&db)
        .await
        .expect("seed row");

        update::<never_create::Entity>(
            &db,
            &principal("alice"),
            inserted.last_insert_id,
            never_create::Model {
                id: inserted.last_insert_id,
                label: "modifié".to_string(),
            },
        )
        .await
        .expect("update succeeds");

        assert_eq!(
            NEVER_CREATE_CALLS.load(Ordering::SeqCst),
            0,
            "update n'appelle jamais before_create"
        );
    }

    /// Scenario « update de la ligne supprimée entre-temps : `NotFound`, plus jamais `Database` »
    /// (arbitré 2026-09-27) — TEST ROUGE avant implémentation. Course pilotée par le `before_save`
    /// de la fixture `vanishing` (voie `SeaORM` postérieure au hook de `core::update`).
    #[tokio::test]
    async fn update_course_supprimee_est_not_found_jamais_database() {
        let db = migrated_db().await;
        create_entity_table(&db, vanishing::Entity).await;
        let inserted = vanishing::Entity::insert(vanishing::ActiveModel {
            id: NotSet,
            label: Set("vivante".to_string()),
        })
        .exec(&db)
        .await
        .expect("seed row");

        let result = update::<vanishing::Entity>(
            &db,
            &principal("alice"),
            inserted.last_insert_id,
            vanishing::Model {
                id: inserted.last_insert_id,
                label: "modifiée".to_string(),
            },
        )
        .await;
        assert!(
            matches!(result, Err(RestError::NotFound)),
            "la ligne disparue entre la relecture et l'UPDATE est un 404, plus jamais un 500 : {result:?}"
        );
    }

    /// `Tasks` « Revue 2026-09-29 » — backend sans `RETURNING` (type `MySQL`) : `SeaORM` émet
    /// l'`UPDATE`, passe `rows_affected`, puis relit la ligne ; la relecture vide rend
    /// `DbErr::RecordNotFound` (et non `RecordNotUpdated`). `update` doit le traduire en
    /// `NotFound`, aligné sur `auth/token.rs`. Simulation par `MockDatabase` : `UPDATE` à une
    /// ligne affectée, relecture sans ligne. TEST ROUGE avant implémentation (sans le bras
    /// supplémentaire, la traduction rend `Database`).
    #[tokio::test]
    async fn update_relecture_vide_apres_update_est_not_found_jamais_database() {
        use sea_orm::{DbBackend, MockDatabase, MockExecResult};

        let alice = user::Model {
            id: 7,
            subject: "alice".to_string(),
            email: None,
            display_name: None,
            created_at: chrono::Utc::now(),
        };
        let row = never_create::Model {
            id: 3,
            label: "vivante".to_string(),
        };
        // `DbBackend::MySql` (et non `Sqlite`) : la feature `sqlite-use-returning-for-3_35` est
        // active dans ce build, `support_returning()` y vaut `true` et `SeaORM` rend alors
        // `RecordNotUpdated` via la voie `UPDATE ... RETURNING` — le même bras déjà couvert par la
        // course `vanishing`. Seul un backend sans `RETURNING` (type `MySQL`) engage la relecture
        // après `UPDATE` qui produit le `RecordNotFound` visé par cette tâche.
        let db = MockDatabase::new(DbBackend::MySql)
            .append_query_results([[alice]])
            .append_query_results([[row]])
            .append_exec_results([MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .append_query_results::<never_create::Model, _, _>([[]])
            .into_connection();

        let result = update::<never_create::Entity>(
            &db,
            &principal("alice"),
            3,
            never_create::Model {
                id: 3,
                label: "modifiée".to_string(),
            },
        )
        .await;

        assert!(
            matches!(result, Err(RestError::NotFound)),
            "`DbErr::RecordNotFound` de la relecture après `UPDATE` (backend sans `RETURNING`) \
             est un 404, plus jamais un 500 : {result:?}"
        );
        let log = db.into_transaction_log();
        assert!(
            log.iter()
                .flat_map(sea_orm::Transaction::statements)
                .any(|stmt| stmt.to_string().starts_with("UPDATE")),
            "l'`UPDATE` a bien été émis — le chemin passe par la relecture, pas par un refus amont"
        );
    }

    // ----------------------------------------------------------------- delete

    /// Scenario « update/delete `OwnerOnly` d'un inconnu : même préfixe » — volet delete du résidu
    /// d'oracle : relecture `None` rend `NotFound`, `can_write` jamais consulté.
    #[tokio::test]
    async fn delete_owner_only_introuvable_est_not_found() {
        let db = migrated_db().await;
        create_entity_table(&db, recipe::Entity).await;

        assert!(matches!(
            delete::<recipe::Entity>(&db, &principal("alice"), 999_999).await,
            Err(RestError::NotFound)
        ));
    }

    /// Volet delete du scenario « AdminOnly/Group d'un inconnu : `403` sans jamais toucher la
    /// table » — TEST ROUGE avant implémentation (preuve par table absente).
    #[tokio::test]
    async fn delete_admin_only_et_group_introuvable_refuse_avant_toute_lecture() {
        let db = migrated_db().await;
        assert!(matches!(
            delete::<ingredient::Entity>(&db, &principal("alice"), 1).await,
            Err(RestError::Forbidden)
        ));
        assert!(matches!(
            delete::<group_owned::Entity>(&db, &principal("alice"), 1).await,
            Err(RestError::Forbidden)
        ));
    }

    /// Scenario « delete : ... `0` ligne devient `NotFound` » (arbitré 2026-09-27) — volet ligne
    /// supprimée entre la relecture et le `DELETE`, piloté par le hook `before_delete` de la
    /// fixture (seule voie de passage entre les deux) — TEST ROUGE avant implémentation.
    #[tokio::test(flavor = "multi_thread")]
    async fn delete_zero_ligne_affectee_est_not_found() {
        let db = migrated_db().await;
        create_entity_table(&db, vanish_del::Entity).await;
        let inserted = vanish_del::Entity::insert(vanish_del::ActiveModel {
            id: NotSet,
            label: Set("a supprimer sous le nez".to_string()),
        })
        .exec(&db)
        .await
        .expect("seed row");

        // Balai : tâche live sur le runtime du test, déclenchée par le hook entre la relecture
        // et l'émission du `DELETE`, acquittée avant que le hook ne rende la main.
        let (req_tx, req_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        VANISH_REQ.set(req_tx).expect("déclencheur posé une seule fois");
        VANISH_DONE
            .set(std::sync::Mutex::new(done_rx))
            .expect("acquittement posé une seule fois");
        let balai_db = db.clone();
        let balai = tokio::spawn(async move {
            tokio::task::block_in_place(move || req_rx.recv()).expect("demande de suppression du hook");
            balai_db
                .execute_unprepared("DELETE FROM vanish_dels")
                .await
                .expect("la ligne disparaît avant le DELETE de core::delete");
            done_tx.send(()).expect("le hook attend l'acquittement");
        });

        let result = delete::<vanish_del::Entity>(&db, &principal("alice"), inserted.last_insert_id).await;
        balai.await.expect("balai rendu");

        assert!(
            matches!(result, Err(RestError::NotFound)),
            "rows_affected == 0 est un NotFound, plus jamais un 204 muet : {result:?}"
        );
    }

    // ----------------------------------------------------------- mark_all_set

    /// Scenario « `mark_all_set` ne touche pas ce qui n'est pas posé » — unitaire, sans base.
    #[test]
    fn mark_all_set_epargne_les_not_set() {
        let active = recipe::ActiveModel {
            id: Set(1),
            title: Set("posée".to_string()),
            owner_id: Unchanged(7),
            category: NotSet,
        };

        let marked = mark_all_set::<recipe::Entity>(active);

        assert!(matches!(marked.title, Set(_)), "une colonne déjà Set reste Set");
        assert!(
            matches!(marked.owner_id, Set(v) if v == 7),
            "Unchanged porteur de valeur est promu Set"
        );
        assert!(
            matches!(marked.category, NotSet),
            "NotSet sans valeur n'est pas promu"
        );
    }

    // ------------------------------------------------------------ parité MCP

    /// Scenario « Parité MCP : la surface MCP rend les mêmes décisions, traduites sans décision
    /// propre » — `--features mcp` : les erreurs rendues par ces fonctions (seul chemin que
    /// `../mcp/registry.rs` emprunte) se traduisent par `From<RestError>` en `-320xx`/`-32603`,
    /// sans réévaluation ; l'invariant d'injection vaut aussi pour `McpOp::Create` (délégation).
    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn parite_mcp_refus_et_invariants() {
        use crate::mcp::McpError;

        let db = migrated_db().await;
        create_entity_table(&db, ingredient::Entity).await;
        create_entity_table(&db, recipe::Entity).await;

        // list_access refuse → RestError::Forbidden → McpError::Forbidden (-32001).
        let refused = list::<ingredient::Entity>(&db, &principal("stranger"), None, None, None)
            .await
            .expect_err("Group sans appartenance refuse");
        let refused: McpError = refused.into();
        assert!(matches!(refused, McpError::Forbidden));
        assert_eq!(
            refused.rpc_code(),
            -32001,
            "traduction sans réévaluation de la politique"
        );

        // NotFound du core → McpError::NotFound (-32002).
        let missing = get::<recipe::Entity>(&db, &principal("alice"), 999_999)
            .await
            .expect_err("ligne absente");
        let missing: McpError = missing.into();
        assert!(matches!(missing, McpError::NotFound));
        assert_eq!(missing.rpc_code(), -32002);

        // Rejet de hook → McpError::Application sans code MRD-*, data.code = HookError::code.
        create_entity_table(&db, parity_hooked::Entity).await;
        let rejected = create::<parity_hooked::Entity>(
            &db,
            &principal("alice"),
            parity_hooked::Model {
                id: 0,
                label: String::new(),
            },
        )
        .await
        .expect_err("le hook rejette le label vide");
        let rejected: McpError = rejected.into();
        assert!(matches!(rejected, McpError::Application(_)));
        assert_eq!(rejected.rpc_code(), -32000);
        assert_eq!(
            rejected.data(),
            Some(serde_json::json!({ "code": "PARITY-001" })),
            "le code du HookError traverse intact"
        );
        assert!(
            !rejected.to_string().contains("MRD-"),
            "HookError ne porte jamais de code de la crate : {rejected}"
        );

        // Invariant d'injection : le create emprunté par McpOp::Create écrase l'owner du corps
        // (même fonction, parité structurelle vérifiée dans ../mcp/registry.rs).
        create_entity_table(&db, public_owned::Entity).await;
        let alice_id = resolve_user(&db, "alice", None).await.expect("resolve").id;
        let created = create::<public_owned::Entity>(
            &db,
            &principal("alice"),
            public_owned::Model {
                id: 0,
                owner_id: 999_999,
                label: "x".to_string(),
            },
        )
        .await
        .expect("create succeeds");
        assert_eq!(created.owner_id, alice_id);
    }
}
