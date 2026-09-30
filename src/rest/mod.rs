//! API REST générique — routeur CRUD axum construit depuis [`crate::resource::MiryadResource`].
//!
//! `resource_router::<E, S>()` monte `GET/POST /api/v1/{resource}` et
//! `GET/PUT/DELETE /api/v1/{resource}/{id}` avec RBAC et pagination automatiques.

pub mod admin;
pub(crate) mod core;
pub mod error;
pub mod me;
pub mod openapi;
pub mod tokens;

use axum::extract::{FromRef, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use sea_orm::sea_query::ColumnType;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, PrimaryKeyToColumn, PrimaryKeyTrait,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::auth::{AuthPrincipal, MiryadAuthState};
use crate::query::PagedResult;
use crate::resource::{AccessPolicy, MiryadResource};
use error::RestError;

/// Entités éligibles au routeur CRUD générique — en plus de `MiryadResource`, il faut pouvoir
/// (dé)sérialiser `Model` et convertir un `Model` reçu en `ActiveModel` sans code par entité
/// (`DeriveEntityModel` fournit `IntoActiveModel` automatiquement). Contrainte assumée : une
/// seule colonne de clé primaire, de type `i32` — vrai pour toutes les entités du crate à ce
/// jour, documentée comme limite dans `docs/architecture.md`.
///
/// Une entité à clé primaire composée (ou, à défaut, un `Model` ne dérivant pas `Deserialize`)
/// qui implémente pourtant `MiryadResource` reste exclue du routeur à la compilation :
/// `sea-orm 2` donne aux clés composées un `PrimaryKeyTrait::ValueType` tuple, jamais `i32`.
/// La vérification vit dans le doctest `compile_fail` ci-dessous — le doctest réussit quand
/// la compilation échoue.
///
/// ```compile_fail
/// use miryad_core::auth::MiryadAuthState;
/// use miryad_core::resource::{AccessPolicy, MiryadResource};
/// use miryad_core::rest::resource_router;
/// use sea_orm::entity::prelude::*;
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
/// #[sea_orm(table_name = "composite_keys")]
/// pub struct Model {
///     #[sea_orm(primary_key, auto_increment = false)]
///     pub first: i32,
///     #[sea_orm(primary_key, auto_increment = false)]
///     pub second: i32,
/// }
///
/// #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
/// pub enum Relation {}
///
/// impl ActiveModelBehavior for ActiveModel {}
///
/// impl MiryadResource for Entity {
///     fn resource_name() -> &'static str {
///         "composites"
///     }
///     fn read_policy() -> AccessPolicy {
///         AccessPolicy::Public
///     }
///     fn write_policy() -> AccessPolicy {
///         AccessPolicy::Public
///     }
///     fn owner_column() -> Option<Column> {
///         None
///     }
/// }
///
/// let _router = resource_router::<Entity, MiryadAuthState>();
/// ```
pub trait RestEntity:
    MiryadResource<
        Model: Serialize + DeserializeOwned + IntoActiveModel<<Self as EntityTrait>::ActiveModel> + Sync,
        ActiveModel: ActiveModelTrait<Entity = Self> + Send,
        PrimaryKey: PrimaryKeyTrait<ValueType = i32>
                        + PrimaryKeyToColumn<Column = <Self as EntityTrait>::Column>,
    >
{
}

impl<E> RestEntity for E where
    E: MiryadResource<
            Model: Serialize + DeserializeOwned + IntoActiveModel<<E as EntityTrait>::ActiveModel> + Sync,
            ActiveModel: ActiveModelTrait<Entity = E> + Send,
            PrimaryKey: PrimaryKeyTrait<ValueType = i32>
                            + PrimaryKeyToColumn<Column = <E as EntityTrait>::Column>,
        >
{
}

#[derive(serde::Deserialize)]
struct ListParams {
    page: Option<u64>,
    per_page: Option<u64>,
    filter: Option<String>,
}

/// Monte `GET/POST /api/v1/{resource_name}` et `GET/PUT/DELETE /api/v1/{resource_name}/{id}`.
/// Réutilise `MiryadAuthState` (feature 2b) — même état que l'auth, rien de nouveau à composer
/// côté app. Préfixe `/api/v1` figé dans le crate (feature 6) — élimine par construction la
/// collision avec une route SPA du frontend dont le nom correspondrait à un `resource_name`.
///
/// # Panics
///
/// Refuse au montage trois déclarations invalides détectables sans requête (arbitré
/// 2026-09-27 pour les deux premières, 2026-09-29 pour la troisième) : `AccessPolicy::OwnerOnly`
/// (lecture ou écriture) avec `owner_column` à `None`, `filter_column` désignant une colonne non
/// textuelle, ou `owner_column` désignant une colonne de type autre que `ColumnType::Integer`.
/// Le message cite l'entité et la règle violée ; une entité mal déclarée ne monte jamais et ne
/// répond jamais à une requête.
pub fn resource_router<E, S>() -> Router<S>
where
    E: RestEntity,
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>,
{
    // mod.sdd « Refuser au montage, par panic » (arbitré 2026-09-27) : garde exécutée avant
    // toute construction de chemin, OwnerOnly sans owner_column. `assert!` : message explicite
    // identique, sans le macro `panic!` (purge de l'exemption de lint, tâche « Revue
    // 2026-09-29 »).
    assert!(
        !(matches!(E::read_policy(), AccessPolicy::OwnerOnly)
            || matches!(E::write_policy(), AccessPolicy::OwnerOnly))
            || E::owner_column().is_some(),
        "`{}` declares `AccessPolicy::OwnerOnly` with `owner_column` None — invalid MiryadResource declaration, refusing to mount its router",
        E::resource_name()
    );

    if let Some(filter_column) = E::filter_column() {
        let def = filter_column.def();
        // Colonne textuelle au sens sea-query : `String` ou `Text` — `filter` y reste réservé
        // (./core.sdd). `assert!` : même panic explicite que le garde ci-dessus, sans le macro
        // `panic!` (mod.sdd « Refuser au montage, par panic » — arbitrage 2026-09-27).
        assert!(
            matches!(def.get_column_type(), ColumnType::String(_) | ColumnType::Text),
            "`{}` declares `filter_column` on a non-textual column — `filter` is reserved for text columns, refusing to mount its router",
            E::resource_name()
        );
    }

    if let Some(owner_column) = E::owner_column() {
        let def = owner_column.def();
        // Colonne `i32` au sens sea-query : `ColumnType::Integer`, nullable accepté — la
        // valeur `None` relève de ../rbac.sdd, pas du montage. `rbac::evaluate` compare des
        // `sea_orm::Value` strictement : hors `Integer`, il ne matcherait jamais et refuserait
        // éternellement hors admin (mod.sdd « Refuser au montage, par panic » — arbitrage
        // 2026-09-29).
        assert!(
            matches!(def.get_column_type(), ColumnType::Integer),
            "`{}` declares `owner_column` on a non-`i32` column — `owner_column` must be `i32`, refusing to mount its router",
            E::resource_name()
        );
    }

    let collection_path = format!("/{}", E::resource_name());
    let item_path = format!("/{}/{{id}}", E::resource_name());

    Router::new().nest(
        "/api/v1",
        Router::new()
            .route(&collection_path, get(list_handler::<E>).post(create_handler::<E>))
            .route(
                &item_path,
                get(get_handler::<E>)
                    .put(update_handler::<E>)
                    .delete(delete_handler::<E>),
            ),
    )
}

async fn list_handler<E: RestEntity>(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Query(params): Query<ListParams>,
) -> Result<Json<PagedResult<E::Model>>, RestError> {
    let page = core::list::<E>(
        &auth.db,
        &principal,
        params.page,
        params.per_page,
        params.filter.as_deref(),
    )
    .await?;
    Ok(Json(page))
}

async fn get_handler<E: RestEntity>(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Path(id): Path<i32>,
) -> Result<Json<E::Model>, RestError> {
    Ok(Json(core::get::<E>(&auth.db, &principal, id).await?))
}

async fn create_handler<E: RestEntity>(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Json(body): Json<E::Model>,
) -> Result<(StatusCode, Json<E::Model>), RestError> {
    Ok((
        StatusCode::CREATED,
        Json(core::create::<E>(&auth.db, &principal, body).await?),
    ))
}

async fn update_handler<E: RestEntity>(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Path(id): Path<i32>,
    Json(body): Json<E::Model>,
) -> Result<Json<E::Model>, RestError> {
    Ok(Json(core::update::<E>(&auth.db, &principal, id, body).await?))
}

async fn delete_handler<E: RestEntity>(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Path(id): Path<i32>,
) -> Result<StatusCode, RestError> {
    core::delete::<E>(&auth.db, &principal, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    // Le `Scenario` « Surface disponible sans aucune feature » de `./mod.sdd` n'a pas de test
    // unitaire ici, conformément à la tâche « Convertir » et au précédent `../users/mod.rs` : sa
    // preuve est la combinaison `--no-default-features` de la batterie de `/tooling.sdd` — ce
    // fichier ne porte aucun `#[cfg(feature = ...)]`, la compilation et les tests inline de la
    // surface CRUD sur cette combinaison (et sur `--all-features`) sont la preuve. Les autres
    // `Scenario` ont chacun un test nommé distinct ci-dessous, ou sont prouvés par un test
    // existant cartographié dans `./mod.sdd` (`owner_only_without_column_panics_at_mount`,
    // `filter_column_on_non_text_column_panics_at_mount`) ou par le doctest `compile_fail` de la
    // doc de `RestEntity` (exclusion de clé composée).

    use super::*;
    use crate::auth::cookie::build_set_cookie;
    use crate::auth::issue_token;
    use crate::auth::oidc::{MockOidcClient, OidcIdentity};
    use crate::migration::Migrator;
    use crate::users::resolve_user as auth_resolve_user;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::patch;
    use sea_orm::ActiveValue::Set;
    use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Schema};
    use sea_orm_migration::MigratorTrait;
    use tower::ServiceExt;

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

    mod widget {
        use crate::resource::{AccessPolicy, HookError, MiryadResource};
        use sea_orm::ActiveValue::Set;
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "widgets")]
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
                "widgets"
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

            // Hook de test : rejette un label vide, sinon le passe en majuscules — de quoi
            // vérifier à la fois le blocage (feature 7b) et la mutation de l'ActiveModel.
            fn before_create(
                active: ActiveModel,
                _principal: &crate::auth::AuthPrincipal,
            ) -> Result<ActiveModel, HookError> {
                let label = match &active.label {
                    sea_orm::ActiveValue::Set(v) | sea_orm::ActiveValue::Unchanged(v) => v.clone(),
                    sea_orm::ActiveValue::NotSet => String::new(),
                };
                if label.is_empty() {
                    return Err(HookError::with_code("WIDGET-001", "label must not be empty"));
                }
                let mut active = active;
                active.label = Set(label.to_uppercase());
                Ok(active)
            }
        }
    }

    // Fixture des hooks `before_update` et `before_delete` (amendement 2026-09-23 de
    // `../rest/core.sdd` et `../resource.sdd`) : écriture `OwnerOnly`, lecture `Public`. Le hook
    // de mise à jour rejette un label vide (code `WIDGET-002`) sinon le passe en majuscules —
    // miroir côté update de ce que fait `widget` à la création ; le hook de suppression ne rejette
    // que les lignes au `status` `"locked"` (code `WIDGET-LOCKED`), pour prouver que le rejet est
    // conditionnel et la suppression possible.
    mod doodad {
        use crate::resource::{AccessPolicy, HookError, MiryadResource};
        use sea_orm::ActiveValue::Set;
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "doodads")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub owner_id: i32,
            pub label: String,
            pub status: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "doodads"
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

            fn before_update(
                active: ActiveModel,
                _existing: &Self::Model,
                _principal: &crate::auth::AuthPrincipal,
            ) -> Result<ActiveModel, HookError> {
                let label = match &active.label {
                    sea_orm::ActiveValue::Set(v) | sea_orm::ActiveValue::Unchanged(v) => v.clone(),
                    sea_orm::ActiveValue::NotSet => String::new(),
                };
                if label.is_empty() {
                    return Err(HookError::with_code(
                        "WIDGET-002",
                        "label must not be updated to empty",
                    ));
                }
                let mut active = active;
                active.label = Set(label.to_uppercase());
                Ok(active)
            }

            fn before_delete(
                existing: &Self::Model,
                _principal: &crate::auth::AuthPrincipal,
            ) -> Result<(), HookError> {
                if existing.status == "locked" {
                    return Err(HookError::with_code(
                        "WIDGET-LOCKED",
                        "locked widgets must not be deleted",
                    ));
                }
                Ok(())
            }
        }
    }

    // Fixture hostile côté update (le pendant update de la fixture `hostile` que la spec
    // `../rest/core.sdd` prévoit pour la création) : le hook re-forge la PK (à 4242, ligne
    // expressément seedingée par le test) et l'owner (utilisateur inconnu 999_999) ; les deux
    // invariants postérieurs de `core::update` doivent annuler la forgedure, et sa mutation de
    // `label` atteste que le hook a bien tourné.
    mod hostile {
        use crate::resource::{AccessPolicy, HookError, MiryadResource};
        use sea_orm::ActiveValue::Set;
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "hostiles")]
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
                "hostiles"
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

            fn before_update(
                active: ActiveModel,
                _existing: &Self::Model,
                _principal: &crate::auth::AuthPrincipal,
            ) -> Result<ActiveModel, HookError> {
                let mut active = active;
                active.id = Set(4242);
                active.owner_id = Set(999_999);
                active.label = Set("FORGED".to_string());
                Ok(active)
            }
        }
    }

    // Fixture de l'arbitrage 2026-09-27 (`Must` « Refuser au montage, par panic ») : entité
    // `OwnerOnly` en écriture déclarant `owner_column` à `None`.
    mod ownerless {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "ownerless")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub label: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        // Déclaration invalide (../resource.sdd) : `OwnerOnly` en écriture sans colonne
        // propriétaire. Jamais montée ni requêtée — `resource_router` doit refuser le montage.
        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "ownerless"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    // Fixture de l'arbitrage 2026-09-27 (`Must` « Refuser au montage, par panic ») : entité dont
    // `filter_column` désigne une colonne `i32` — `filter` reste réservé aux colonnes texte
    // (./core.sdd), le montage doit refuser avant toute route construite.
    mod numberfilter {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "numberfilters")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub count: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "numberfilters"
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
            fn filter_column() -> Option<Column> {
                Some(Column::Count)
            }
        }
    }

    // Fixture du troisième garde (arbitrage 2026-09-29, `Must` « Refuser au montage, par panic ») :
    // entité `OwnerOnly` en écriture dont `owner_column` désigne une colonne `i64` —
    // `rbac::evaluate` compare des `sea_orm::Value` strictement et ne matcherait jamais un `i32`
    // utilisateur : refus du montage avant toute route construite.
    mod bigowner {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "bigowners")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub owner_id: i64,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        // Déclaration invalide (../resource.sdd, arbitrage 2026-09-29) : colonne propriétaire
        // `i64`. Jamais montée ni requêtée — `resource_router` doit refuser le montage.
        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "bigowner"
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

    // Contre-épreuve du troisième garde (même arbitrage) : une colonne propriétaire nullable
    // `Option<i32>` reste du `ColumnType::Integer` — le montage est accepté, le cas « valeur
    // `None` » relève de ../rbac.sdd, pas du montage.
    mod optowner {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "optowners")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub owner_id: Option<i32>,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "optowners"
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

    async fn test_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");

        // Tables de test, créées à la volée (pas de migration permanente pour des entités qui
        // n'existent que dans ces tests) via l'utilitaire `Schema` de SeaORM.
        let backend = db.get_database_backend();
        let schema = Schema::new(backend);
        db.execute(&schema.create_table_from_entity(recipe::Entity))
            .await
            .expect("recipes table creates");
        db.execute(&schema.create_table_from_entity(ingredient::Entity))
            .await
            .expect("ingredients table creates");
        db.execute(&schema.create_table_from_entity(widget::Entity))
            .await
            .expect("widgets table creates");
        db.execute(&schema.create_table_from_entity(doodad::Entity))
            .await
            .expect("doodads table creates");
        db.execute(&schema.create_table_from_entity(hostile::Entity))
            .await
            .expect("hostiles table creates");
        db
    }

    fn test_state(db: DatabaseConnection) -> MiryadAuthState {
        MiryadAuthState {
            oidc_client: std::sync::Arc::new(MockOidcClient::default()),
            cookie_key: ::cookie::Key::from(&[0u8; 64]),
            post_login_redirect: "/".to_string(),
            post_logout_redirect: "/".to_string(),
            db,
            secure_cookies: false,
            token_pepper: "test-pepper".to_string(),
        }
    }

    fn app(state: MiryadAuthState) -> Router {
        Router::new()
            .merge(resource_router::<recipe::Entity, MiryadAuthState>())
            .merge(resource_router::<ingredient::Entity, MiryadAuthState>())
            .merge(resource_router::<widget::Entity, MiryadAuthState>())
            .merge(resource_router::<doodad::Entity, MiryadAuthState>())
            .merge(resource_router::<hostile::Entity, MiryadAuthState>())
            .with_state(state)
    }

    async fn bearer_for(db: &DatabaseConnection, subject: &str) -> String {
        issue_token(db, subject, "test", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token
    }

    fn json_request(method: &str, uri: &str, token: &str, body: Option<serde_json::Value>) -> Request<Body> {
        let builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json");
        match body {
            Some(value) => builder
                .body(Body::from(value.to_string()))
                .expect("valid request"),
            None => builder.body(Body::empty()).expect("valid request"),
        }
    }

    async fn json_body(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        serde_json::from_slice(&bytes).expect("valid JSON body")
    }

    /// Corps texte brut d'une réponse — les rejets `400`/`401`/`404` d'axum et de `AuthError`
    /// ne sont pas du JSON, `json_body` ne s'applique qu'aux corps de la crate.
    async fn text_body(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// JWT tri-segment dont la claim `exp` vaut `exp` (copie du pattern de `../auth/dual.rs`) —
    /// `extract_session` relit cette claim côté serveur.
    fn make_jwt(exp: u64) -> String {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("header.{payload}.sig")
    }

    fn future_exp() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_secs()
            + 3600
    }

    /// Paire `nom=valeur` isolée d'un en-tête `Set-Cookie` (premier segment avant `;`).
    fn cookie_pair(set_cookie: &str) -> String {
        set_cookie
            .split(';')
            .next()
            .expect("cookie pair present")
            .to_string()
    }

    /// Cookie `miryad_session` valide signé de la clé de l'état, pour le `subject` donné
    /// (pattern de construction de `../auth/dual.rs`, Scenario « Le cookie de session traverse
    /// la surface CRUD »).
    fn session_cookie_for(state: &MiryadAuthState, subject: &str) -> String {
        let identity = OidcIdentity {
            id_token: make_jwt(future_exp()),
            subject: subject.to_string(),
            email: None,
            preferred_username: None,
        };
        cookie_pair(&build_set_cookie(
            &identity,
            &state.cookie_key,
            state.secure_cookies,
        ))
    }

    #[tokio::test]
    async fn create_ignores_client_supplied_owner() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let alice = auth_resolve_user(&db, "alice", None).await.expect("resolve");
        let state = test_state(db);
        let app = app(state);

        let body = serde_json::json!({
            "id": 0,
            "title": "Tarte",
            "owner_id": 999_999,
            "category": "dessert",
        });
        let resp = app
            .oneshot(json_request("POST", "/api/v1/recipes", &token, Some(body)))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::CREATED);
        let created = json_body(resp).await;
        assert_eq!(created["owner_id"], alice.id);
    }

    #[tokio::test]
    async fn list_filters_by_owner_for_non_admin_but_not_for_admin() {
        let db = test_db().await;
        let alice_token = bearer_for(&db, "alice").await;
        let admin_token = bearer_for(&db, "admin-user").await;
        let admin = auth_resolve_user(&db, "admin-user", None).await.expect("resolve");
        crate::users::sync_group_memberships(&db, admin.id, &["admin".to_string()])
            .await
            .expect("sync");

        let state = test_state(db);
        let app_ref = app(state);

        for (title, owner_token) in [("Tarte", &alice_token), ("Soupe", &admin_token)] {
            let body = serde_json::json!({
                "id": 0, "title": title, "owner_id": 0, "category": "plat",
            });
            let resp = app_ref
                .clone()
                .oneshot(json_request("POST", "/api/v1/recipes", owner_token, Some(body)))
                .await
                .expect("create succeeds");
            assert_eq!(resp.status(), StatusCode::CREATED);
        }

        let alice_list = app_ref
            .clone()
            .oneshot(json_request("GET", "/api/v1/recipes", &alice_token, None))
            .await
            .expect("list succeeds");
        let alice_body = json_body(alice_list).await;
        assert_eq!(alice_body["total_items"], 1);

        let admin_list = app_ref
            .oneshot(json_request("GET", "/api/v1/recipes", &admin_token, None))
            .await
            .expect("list succeeds");
        let admin_body = json_body(admin_list).await;
        assert_eq!(admin_body["total_items"], 2);
    }

    #[tokio::test]
    async fn list_pagination_returns_requested_page() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app_ref = app(state);

        for title in ["Un", "Deux", "Trois"] {
            let body = serde_json::json!({
                "id": 0, "title": title, "owner_id": 0, "category": "plat",
            });
            app_ref
                .clone()
                .oneshot(json_request("POST", "/api/v1/recipes", &token, Some(body)))
                .await
                .expect("create succeeds");
        }

        let resp = app_ref
            .oneshot(json_request(
                "GET",
                "/api/v1/recipes?page=2&per_page=1",
                &token,
                None,
            ))
            .await
            .expect("list succeeds");
        let page = json_body(resp).await;
        assert_eq!(page["page"], 2);
        assert_eq!(page["per_page"], 1);
        assert_eq!(page["total_items"], 3);
        assert_eq!(page["total_pages"], 3);
        assert_eq!(page["items"].as_array().expect("array").len(), 1);
        assert_eq!(page["items"][0]["title"], "Deux");
    }

    #[tokio::test]
    async fn list_filter_combines_with_owner_restriction() {
        let db = test_db().await;
        let alice_token = bearer_for(&db, "alice").await;
        let bob_token = bearer_for(&db, "bob").await;
        let state = test_state(db);
        let app_ref = app(state);

        for (title, category, token) in [
            ("Tarte", "dessert", &alice_token),
            ("Soupe", "plat", &alice_token),
            ("Gateau", "dessert", &bob_token),
        ] {
            let body = serde_json::json!({
                "id": 0, "title": title, "owner_id": 0, "category": category,
            });
            app_ref
                .clone()
                .oneshot(json_request("POST", "/api/v1/recipes", token, Some(body)))
                .await
                .expect("create succeeds");
        }

        let resp = app_ref
            .oneshot(json_request(
                "GET",
                "/api/v1/recipes?filter=dessert",
                &alice_token,
                None,
            ))
            .await
            .expect("list succeeds");
        let page = json_body(resp).await;
        // Alice n'a qu'une recette "dessert" (la sienne) — celle de Bob, bien que "dessert" aussi,
        // reste hors de portée grâce au filtre RBAC combiné au filtre de catégorie.
        assert_eq!(page["total_items"], 1);
        assert_eq!(page["items"][0]["title"], "Tarte");
    }

    #[tokio::test]
    async fn list_ingredients_forbidden_without_group_membership() {
        let db = test_db().await;
        let token = bearer_for(&db, "stranger").await;
        let state = test_state(db);
        let app_ref = app(state);

        let resp = app_ref
            .oneshot(json_request("GET", "/api/v1/ingredients", &token, None))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn update_forbidden_for_non_owner_allowed_for_owner() {
        let db = test_db().await;
        let alice_token = bearer_for(&db, "alice").await;
        let bob_token = bearer_for(&db, "bob").await;
        let state = test_state(db);
        let app_ref = app(state);

        let create_body = serde_json::json!({
            "id": 0, "title": "Tarte", "owner_id": 0, "category": "dessert",
        });
        let created = app_ref
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/recipes",
                &alice_token,
                Some(create_body),
            ))
            .await
            .expect("create succeeds");
        let created = json_body(created).await;
        let id = created["id"].as_i64().expect("id present");

        let update_body = serde_json::json!({
            "id": id, "title": "Tarte modifiee", "owner_id": 0, "category": "dessert",
        });
        let forbidden = app_ref
            .clone()
            .oneshot(json_request(
                "PUT",
                &format!("/api/v1/recipes/{id}"),
                &bob_token,
                Some(update_body.clone()),
            ))
            .await
            .expect("router does not fail");
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

        let allowed = app_ref
            .oneshot(json_request(
                "PUT",
                &format!("/api/v1/recipes/{id}"),
                &alice_token,
                Some(update_body),
            ))
            .await
            .expect("router does not fail");
        assert_eq!(allowed.status(), StatusCode::OK);
        let updated = json_body(allowed).await;
        assert_eq!(updated["title"], "Tarte modifiee");
    }

    #[tokio::test]
    async fn update_ignores_client_supplied_owner() {
        let db = test_db().await;
        let alice_token = bearer_for(&db, "alice").await;
        let alice = auth_resolve_user(&db, "alice", None).await.expect("resolve");
        let bob = auth_resolve_user(&db, "bob", None).await.expect("resolve");
        let state = test_state(db);
        let app_ref = app(state);

        let create_body = serde_json::json!({
            "id": 0, "title": "Tarte", "owner_id": 0, "category": "dessert",
        });
        let created = app_ref
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/recipes",
                &alice_token,
                Some(create_body),
            ))
            .await
            .expect("create succeeds");
        let created = json_body(created).await;
        let id = created["id"].as_i64().expect("id present");
        assert_eq!(created["owner_id"], alice.id);

        // Tente de "donner" la ressource à bob en changeant owner_id dans le corps — doit rester
        // sans effet, comme create() protège déjà owner_id contre une valeur cliente.
        let update_body = serde_json::json!({
            "id": id, "title": "Tarte modifiee", "owner_id": bob.id, "category": "dessert",
        });
        let resp = app_ref
            .oneshot(json_request(
                "PUT",
                &format!("/api/v1/recipes/{id}"),
                &alice_token,
                Some(update_body),
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        let updated = json_body(resp).await;
        assert_eq!(updated["title"], "Tarte modifiee");
        assert_eq!(updated["owner_id"], alice.id);
    }

    #[tokio::test]
    async fn update_with_invalid_owner_id_in_body_does_not_fail() {
        let db = test_db().await;
        let alice_token = bearer_for(&db, "alice").await;
        let alice = auth_resolve_user(&db, "alice", None).await.expect("resolve");
        let state = test_state(db);
        let app_ref = app(state);

        let create_body = serde_json::json!({
            "id": 0, "title": "Tarte", "owner_id": 0, "category": "dessert",
        });
        let created = app_ref
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/recipes",
                &alice_token,
                Some(create_body),
            ))
            .await
            .expect("create succeeds");
        let created = json_body(created).await;
        let id = created["id"].as_i64().expect("id present");

        // owner_id ne correspondant à aucun utilisateur — appliqué tel quel casserait sur la
        // contrainte FK (500 brut) si update() ne l'ignorait pas comme create().
        let update_body = serde_json::json!({
            "id": id, "title": "Tarte modifiee", "owner_id": 999_999, "category": "dessert",
        });
        let resp = app_ref
            .oneshot(json_request(
                "PUT",
                &format!("/api/v1/recipes/{id}"),
                &alice_token,
                Some(update_body),
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        let updated = json_body(resp).await;
        assert_eq!(updated["owner_id"], alice.id);
    }

    #[tokio::test]
    async fn get_and_delete_nonexistent_recipe_return_404() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app_ref = app(state);

        let get_resp = app_ref
            .clone()
            .oneshot(json_request("GET", "/api/v1/recipes/999999", &token, None))
            .await
            .expect("router does not fail");
        assert_eq!(get_resp.status(), StatusCode::NOT_FOUND);

        let delete_resp = app_ref
            .clone()
            .oneshot(json_request("DELETE", "/api/v1/recipes/999999", &token, None))
            .await
            .expect("router does not fail");
        assert_eq!(delete_resp.status(), StatusCode::NOT_FOUND);

        // Élargissement rattaché à `../rest/core.sdd` (oracle d'existence, arbitrage 2026-09-27) :
        // la recette (`OwnerOnly`) est le résidu assumé qui répond `404` ; sous `Group` (lecture)
        // et `AdminOnly` (écriture), un id inconnu répond `403` — `static_verdict` refuse avant
        // toute relecture de la table, l'existence de la ligne n'est jamais trahie.
        let ingredient_get = app_ref
            .clone()
            .oneshot(json_request("GET", "/api/v1/ingredients/999999", &token, None))
            .await
            .expect("router does not fail");
        assert_eq!(
            ingredient_get.status(),
            StatusCode::FORBIDDEN,
            "l'oracle d'existence est fermé pour la lecture `Group`"
        );

        let ingredient_delete = app_ref
            .oneshot(json_request("DELETE", "/api/v1/ingredients/999999", &token, None))
            .await
            .expect("router does not fail");
        assert_eq!(
            ingredient_delete.status(),
            StatusCode::FORBIDDEN,
            "l'oracle d'existence est fermé pour l'écriture `AdminOnly`"
        );
    }

    #[tokio::test]
    async fn before_create_hook_mutation_is_applied_at_insert() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app = app(state);

        let body = serde_json::json!({"id": 0, "owner_id": 0, "label": "gadget"});
        let resp = app
            .oneshot(json_request("POST", "/api/v1/widgets", &token, Some(body)))
            .await
            .expect("router does not fail");

        assert_eq!(resp.status(), StatusCode::CREATED);
        let created = json_body(resp).await;
        assert_eq!(created["label"], "GADGET");
    }

    #[tokio::test]
    async fn before_create_hook_error_blocks_insert_without_mrd_code() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app = app(state);

        let body = serde_json::json!({"id": 0, "owner_id": 0, "label": ""});
        let resp = app
            .oneshot(json_request("POST", "/api/v1/widgets", &token, Some(body)))
            .await
            .expect("router does not fail");

        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let error = json_body(resp).await;
        assert_eq!(error["code"], "WIDGET-001");
        assert_eq!(error["message"], "label must not be empty");
    }

    // Scenario « update : la mutation du hook before_update est écrite, mais les invariants
    // restent après lui » (amendement 2026-09-23, `../rest/core.sdd`) — la ligne écrite porte
    // l'id du chemin, l'owner d'`existing` et la mutation du hook ; PK divergente et owner
    // étranger du corps sont ignorés.
    #[tokio::test]
    async fn before_update_hook_mutation_is_applied_at_update() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let alice = auth_resolve_user(&db, "alice", None).await.expect("resolve");
        let bob = auth_resolve_user(&db, "bob", None).await.expect("resolve");
        let state = test_state(db);
        let app_ref = app(state);

        let create_body = serde_json::json!({
            "id": 0, "owner_id": 0, "label": "gadget", "status": "open",
        });
        let created = app_ref
            .clone()
            .oneshot(json_request("POST", "/api/v1/doodads", &token, Some(create_body)))
            .await
            .expect("create succeeds");
        assert_eq!(created.status(), StatusCode::CREATED);
        let created = json_body(created).await;
        let id = created["id"].as_i64().expect("id present");

        // PK divergente de l'id du chemin et owner étranger — les deux invariants postérieurs au
        // hook doivent les annuler, la mutation du hook doit survivre.
        let update_body = serde_json::json!({
            "id": id + 1000, "owner_id": bob.id, "label": "mutant", "status": "open",
        });
        let resp = app_ref
            .oneshot(json_request(
                "PUT",
                &format!("/api/v1/doodads/{id}"),
                &token,
                Some(update_body),
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        let updated = json_body(resp).await;
        assert_eq!(
            updated["id"], id,
            "la cible est la ligne du chemin, jamais celle du corps"
        );
        assert_eq!(
            updated["owner_id"], alice.id,
            "le propriétaire reste celui d'existing"
        );
        assert_eq!(
            updated["label"], "MUTANT",
            "la mutation du hook est écrite sur la ligne"
        );
    }

    // Scenario « update réjecté par before_update : RestError::Application sans code MRD-* »
    // (amendement 2026-09-23) — le HookError `WIDGET-002` traverse intact et interrompt avant
    // `ActiveModelTrait::update` : la ligne en base reste inchangée.
    #[tokio::test]
    async fn before_update_hook_error_blocks_update_without_mrd_code() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app_ref = app(state);

        let create_body = serde_json::json!({
            "id": 0, "owner_id": 0, "label": "draft", "status": "open",
        });
        let created = app_ref
            .clone()
            .oneshot(json_request("POST", "/api/v1/doodads", &token, Some(create_body)))
            .await
            .expect("create succeeds");
        let created = json_body(created).await;
        let id = created["id"].as_i64().expect("id present");

        let update_body = serde_json::json!({
            "id": id, "owner_id": 0, "label": "", "status": "open",
        });
        let resp = app_ref
            .clone()
            .oneshot(json_request(
                "PUT",
                &format!("/api/v1/doodads/{id}"),
                &token,
                Some(update_body),
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let error = json_body(resp).await;
        assert_eq!(error["code"], "WIDGET-002");
        assert_eq!(error["message"], "label must not be updated to empty");
        let rendered = error.to_string();
        assert!(
            !rendered.contains("MRD-"),
            "le HookError ne porte jamais de code de la crate : {rendered}"
        );

        // Aucune écriture : `ActiveModelTrait::update` n'a pas reçu l'ActiveModel rejeté.
        let after = app_ref
            .oneshot(json_request(
                "GET",
                &format!("/api/v1/doodads/{id}"),
                &token,
                None,
            ))
            .await
            .expect("router does not fail");
        assert_eq!(after.status(), StatusCode::OK);
        let after = json_body(after).await;
        assert_eq!(
            after["label"], "draft",
            "l'update rejetée laisse la ligne inchangée"
        );
        assert_eq!(after["status"], "open");
    }

    // Scenario « update hook hostile : PK et owner qu'il re-forge sont annulés par les
    // invariants » (amendement 2026-09-23, miroir côté update du hook de création hostile) —
    // la fixture `hostile` re-pose la PK à 4242 (ligne seedingée pour l'occasion) et l'owner sur
    // un utilisateur inconnu : la cible reste la ligne du chemin, le propriétaire reste celui
    // d'`existing`, et la troisième ligne n'a rien subi.
    #[tokio::test]
    async fn update_hook_hostile_ne_contourne_les_invariants() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let alice = auth_resolve_user(&db, "alice", None).await.expect("resolve");
        let bob = auth_resolve_user(&db, "bob", None).await.expect("resolve");

        // Troisième ligne portant la PK que le hook hostile re-forge (4242) : la mise à jour ne
        // doit jamais atterrir dessus.
        hostile::Entity::insert(hostile::ActiveModel {
            id: Set(4242),
            owner_id: Set(bob.id),
            label: Set("target".to_string()),
        })
        .exec(&db)
        .await
        .expect("third row seeds");
        let probe = db.clone();

        let state = test_state(db);
        let app_ref = app(state);

        let create_body = serde_json::json!({"id": 0, "owner_id": 0, "label": "alpha"});
        let created = app_ref
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/hostiles",
                &token,
                Some(create_body),
            ))
            .await
            .expect("create succeeds");
        let created = json_body(created).await;
        let id = created["id"].as_i64().expect("id present");

        let update_body = serde_json::json!({"id": id, "owner_id": alice.id, "label": "alpha2"});
        let resp = app_ref
            .oneshot(json_request(
                "PUT",
                &format!("/api/v1/hostiles/{id}"),
                &token,
                Some(update_body),
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        let updated = json_body(resp).await;
        assert_eq!(
            updated["id"], id,
            "la PK forcée par le chemin l'emporte sur le hook"
        );
        assert_eq!(
            updated["owner_id"], alice.id,
            "l'owner reconduit depuis existing l'emporte sur le hook"
        );
        assert_eq!(
            updated["label"], "FORGED",
            "la mutation licite du hook passe malgré tout"
        );

        let other = hostile::Entity::find_by_id(4242)
            .one(&probe)
            .await
            .expect("third row still readable")
            .expect("aucune mise à jour n'a atterri sur la PK forgée par le hook");
        assert_eq!(other.owner_id, bob.id);
        assert_eq!(other.label, "target", "la ligne tierce est restée intacte");
    }

    // Scenario « delete réjecté par before_delete : RestError::Application, aucun DELETE émis »
    // (amendement 2026-09-23) — le HookError `WIDGET-LOCKED` traverse intact, `delete_by_id`
    // n'est jamais atteint et la ligne `"locked"` survit ; un second appel sur une ligne libre
    // supprime normalement, prouvant que le rejet est conditionnel au hook.
    #[tokio::test]
    async fn before_delete_hook_error_blocks_delete_without_mrd_code() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app_ref = app(state);

        let mut ids = Vec::new();
        for (label, status) in [("garde", "locked"), ("libre", "open")] {
            let body = serde_json::json!({
                "id": 0, "owner_id": 0, "label": label, "status": status,
            });
            let created = app_ref
                .clone()
                .oneshot(json_request("POST", "/api/v1/doodads", &token, Some(body)))
                .await
                .expect("create succeeds");
            assert_eq!(created.status(), StatusCode::CREATED);
            let created = json_body(created).await;
            ids.push(created["id"].as_i64().expect("id present"));
        }
        let locked = ids.first().copied().expect(" deux lignes créées");
        let other = ids.get(1).copied().expect("deux lignes créées");

        let resp = app_ref
            .clone()
            .oneshot(json_request(
                "DELETE",
                &format!("/api/v1/doodads/{locked}"),
                &token,
                None,
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let error = json_body(resp).await;
        assert_eq!(error["code"], "WIDGET-LOCKED");
        assert_eq!(error["message"], "locked widgets must not be deleted");
        let rendered = error.to_string();
        assert!(
            !rendered.contains("MRD-"),
            "le HookError ne porte jamais de code de la crate : {rendered}"
        );

        // `delete_by_id` n'a jamais été atteint : la ligne verrouillée est toujours en base.
        let still_there = app_ref
            .clone()
            .oneshot(json_request(
                "GET",
                &format!("/api/v1/doodads/{locked}"),
                &token,
                None,
            ))
            .await
            .expect("router does not fail");
        assert_eq!(still_there.status(), StatusCode::OK);

        // Une ligne non verrouillée se supprime normalement — le rejet est conditionnel au hook,
        // pas systématique.
        let deleted = app_ref
            .clone()
            .oneshot(json_request(
                "DELETE",
                &format!("/api/v1/doodads/{other}"),
                &token,
                None,
            ))
            .await
            .expect("router does not fail");
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        let gone = app_ref
            .oneshot(json_request(
                "GET",
                &format!("/api/v1/doodads/{other}"),
                &token,
                None,
            ))
            .await
            .expect("router does not fail");
        assert_eq!(gone.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn entity_without_hook_override_is_unaffected() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app = app(state);

        let body = serde_json::json!({
            "id": 0, "title": "Tarte", "owner_id": 0, "category": "dessert",
        });
        let resp = app
            .oneshot(json_request("POST", "/api/v1/recipes", &token, Some(body)))
            .await
            .expect("router does not fail");

        assert_eq!(resp.status(), StatusCode::CREATED);
        let created = json_body(resp).await;
        assert_eq!(created["title"], "Tarte");
    }

    // ——— Conversion des `Scenario` de `./mod.sdd` (tâche « Convertir ») : un test nommé
    // distinct par Scenario non prouvé par un test existant. Chaque test est un verrou du
    // comportement réel de la surface assemblée (délégation à ./core.rs, rejets d'axum). ———

    /// `Scenario` : « La route collection expose GET et POST ».
    #[tokio::test]
    async fn collection_route_serves_get_and_post() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app_ref = app(state);

        let resp = app_ref
            .clone()
            .oneshot(json_request("GET", "/api/v1/recipes", &token, None))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        let page = json_body(resp).await;
        for key in ["items", "page", "per_page", "total_items", "total_pages"] {
            assert!(
                page.get(key).is_some(),
                "les cinq clés wire de PagedResult doivent être là, manque `{key}` : {page}"
            );
        }
        assert_eq!(page["items"], serde_json::json!([]), "base vierge : items vide");

        let body = serde_json::json!({
            "id": 0, "title": "Tarte", "owner_id": 0, "category": "dessert",
        });
        let resp = app_ref
            .clone()
            .oneshot(json_request("POST", "/api/v1/recipes", &token, Some(body)))
            .await
            .expect("router does not fail");
        assert_eq!(
            resp.status(),
            StatusCode::CREATED,
            "201 à la création (arbitré 2026-09-27)"
        );
        let created = json_body(resp).await;
        assert_eq!(created["title"], "Tarte");

        let page = json_body(
            app_ref
                .oneshot(json_request("GET", "/api/v1/recipes", &token, None))
                .await
                .expect("router does not fail"),
        )
        .await;
        assert_eq!(page["total_items"], 1);
        assert_eq!(
            page["items"][0]["id"], created["id"],
            "le GET suivant contient le créé"
        );
    }

    /// `Scenario` : « Verbe hors liste répond 405 avec Allow ».
    #[tokio::test]
    async fn unlisted_verb_returns_405_with_allow() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app_ref = app(state);

        let resp = app_ref
            .oneshot(json_request("PATCH", "/api/v1/recipes", &token, None))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

        let allow = resp
            .headers()
            .get("allow")
            .expect("en-tête Allow présent sur un 405 d'axum")
            .to_str()
            .expect("Allow en ASCII")
            .to_string();
        let methods: Vec<&str> = allow.split(',').map(str::trim).collect();
        for expected in ["GET", "HEAD", "POST"] {
            assert!(
                methods.contains(&expected),
                "l'en-tête Allow `{allow}` doit contenir {expected}"
            );
        }

        let body = text_body(resp).await;
        assert!(
            !body.contains("MRD-REST-"),
            "le 405 vient d'axum, aucune erreur de la crate ne doit filtrer : {body}"
        );
    }

    /// `Scenario` : « HEAD passe par le handler GET de la collection ».
    #[tokio::test]
    async fn head_is_served_by_collection_get() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app_ref = app(state);

        let resp = app_ref
            .oneshot(json_request("HEAD", "/api/v1/recipes", &token, None))
            .await
            .expect("router does not fail");
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "HEAD servi par list_handler avec le corps retiré (axum 0.8.9)"
        );
        assert!(text_body(resp).await.is_empty(), "aucun corps sur HEAD");
    }

    /// `Scenario` : « La route item répond GET, PUT et DELETE ».
    #[tokio::test]
    async fn item_route_serves_get_put_delete() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app_ref = app(state);

        let create_body = serde_json::json!({
            "id": 0, "title": "Tarte", "owner_id": 0, "category": "dessert",
        });
        let created = json_body(
            app_ref
                .clone()
                .oneshot(json_request("POST", "/api/v1/recipes", &token, Some(create_body)))
                .await
                .expect("create succeeds"),
        )
        .await;
        let id = created["id"].as_i64().expect("id present");

        let resp = app_ref
            .clone()
            .oneshot(json_request(
                "GET",
                &format!("/api/v1/recipes/{id}"),
                &token,
                None,
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_body(resp).await["title"], "Tarte");

        let update_body = serde_json::json!({
            "id": id, "title": "Tarte modifiee", "owner_id": 0, "category": "dessert",
        });
        let resp = app_ref
            .clone()
            .oneshot(json_request(
                "PUT",
                &format!("/api/v1/recipes/{id}"),
                &token,
                Some(update_body),
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_body(resp).await["title"], "Tarte modifiee");

        let resp = app_ref
            .clone()
            .oneshot(json_request(
                "DELETE",
                &format!("/api/v1/recipes/{id}"),
                &token,
                None,
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(text_body(resp).await.is_empty(), "204 sans corps");

        let resp = app_ref
            .oneshot(json_request(
                "GET",
                &format!("/api/v1/recipes/{id}"),
                &token,
                None,
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(text_body(resp).await, "MRD-REST-001: resource not found");
    }

    /// `Scenario` : « id non numérique répond 400 avant tout handler ».
    #[tokio::test]
    async fn non_numeric_id_returns_400() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app_ref = app(state);

        let resp = app_ref
            .oneshot(json_request("GET", "/api/v1/recipes/abc", &token, None))
            .await
            .expect("router does not fail");
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "rejet FailedToDeserializePathParams d'axum, pas 404"
        );
        let body = text_body(resp).await;
        assert!(
            !body.contains("MRD-REST-"),
            "get_handler jamais appelé, Path<i32> échoue avant : {body}"
        );
    }

    /// `Scenario` : « id hors portée i32 répond 400 ».
    #[tokio::test]
    async fn id_out_of_i32_range_returns_400() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app_ref = app(state);

        // 3 000 000 000 > i32::MAX : même rejet Path que l'id non numérique, la borne i32 de
        // la signature de get_handler est observable sur le fil.
        let resp = app_ref
            .oneshot(json_request("GET", "/api/v1/recipes/3000000000", &token, None))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = text_body(resp).await;
        assert!(
            !body.contains("MRD-REST-"),
            "rejet d'axum, pas de la crate : {body}"
        );
    }

    /// `Scenario` : « Chemin inconnu sous le préfixe monté répond 404 ».
    #[tokio::test]
    async fn unknown_path_under_mount_returns_404() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app_ref = app(state);

        let resp = app_ref
            .oneshot(json_request("GET", "/api/v1/ghosts", &token, None))
            .await
            .expect("router does not fail");
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "404 du fallback par défaut d'axum — ./mod.rs ne pose aucun fallback"
        );
        let body = text_body(resp).await;
        assert!(
            !body.contains("MRD-REST-"),
            "MRD-REST-001 viendrait de ./core.rs, jamais atteint sur un chemin inexistant : {body}"
        );
    }

    /// `Scenario` : « Refus d'authentification précède tout parsing de corps et de query ».
    #[tokio::test]
    async fn auth_rejection_short_circuits_body_and_query() {
        let db = test_db().await;
        let state = test_state(db);
        let app_ref = app(state);

        // POST sans aucun credential, corps JSON syntaxiquement invalide : 401, pas 400 —
        // l'extraction AuthPrincipal précède Json dans l'ordre de signature.
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/recipes")
            .header("Content-Type", "application/json")
            .body(Body::from("{ ceci n'est pas du json"))
            .expect("valid request");
        let resp = app_ref.clone().oneshot(req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            text_body(resp).await,
            "MRD-AUTH-001: not authenticated (no session cookie)"
        );

        // GET sans credential, query mal typée : 401 aussi, Query n'est pas atteint.
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/recipes?page=abc")
            .body(Body::empty())
            .expect("valid request");
        let resp = app_ref.oneshot(req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// `Scenario` : « Paramètre query mal typé répond 400 avec credentials valides ».
    #[tokio::test]
    async fn malformed_query_returns_400_when_authenticated() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app_ref = app(state);

        let resp = app_ref
            .clone()
            .oneshot(json_request("GET", "/api/v1/recipes?page=abc", &token, None))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "QueryRejection d'axum");
        let body = text_body(resp).await;
        assert!(
            !body.contains("MRD-REST-"),
            "rejet d'axum, pas de la crate : {body}"
        );

        // Paramètre inconnu ignoré (ListParams sans deny_unknown_fields) : 200, page rendue
        // telle que normalisée.
        let resp = app_ref
            .oneshot(json_request(
                "GET",
                "/api/v1/recipes?page=2&inconnu=1",
                &token,
                None,
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_body(resp).await["page"], 2);
    }

    /// `Scenario` : « page et `per_page` à zéro sont renvoyés normalisés ».
    #[tokio::test]
    async fn wire_observes_normalized_pagination() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app_ref = app(state);

        for title in ["Un", "Deux", "Trois"] {
            let body = serde_json::json!({
                "id": 0, "title": title, "owner_id": 0, "category": "plat",
            });
            app_ref
                .clone()
                .oneshot(json_request("POST", "/api/v1/recipes", &token, Some(body)))
                .await
                .expect("create succeeds");
        }

        let resp = app_ref
            .oneshot(json_request(
                "GET",
                "/api/v1/recipes?page=0&per_page=0",
                &token,
                None,
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        let page = json_body(resp).await;
        // La normalisation vue est celle de Pagination::from_raw via core::list (../query.sdd) :
        // ./mod.rs ne fait que passer les Option bruts.
        assert_eq!(page["page"], 1, "page 0 rendue normalisée à 1");
        assert_eq!(page["per_page"], 1, "per_page 0 rendu normalisé à 1");
    }

    /// `Scenario` : « Corps JSON invalide est refusé avant le hook ».
    #[tokio::test]
    async fn invalid_json_body_refuses_before_hook() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app_ref = app(state);

        // Corps syntaxiquement invalide : 400 (JsonSyntaxError d'axum), before_create jamais
        // appelé — aucun code WIDGET-001 ne doit apparaître.
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/widgets")
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(Body::from("{ ce ci n'est pas du json"))
            .expect("valid request");
        let resp = app_ref.clone().oneshot(req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = text_body(resp).await;
        assert!(
            !body.contains("WIDGET-001"),
            "le hook n'est jamais appelé : {body}"
        );

        // Aucune ligne insérée.
        let resp = app_ref
            .clone()
            .oneshot(json_request("GET", "/api/v1/widgets", &token, None))
            .await
            .expect("router does not fail");
        assert_eq!(json_body(resp).await["total_items"], 0);

        // Même POST sans en-tête Content-Type application/json : 415.
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/widgets")
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::from(r#"{"id": 0, "owner_id": 0, "label": "gadget"}"#))
            .expect("valid request");
        let resp = app_ref.clone().oneshot(req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        // Corps JSON valide de mauvaise forme : 422 JsonDataError, statut partagé avec le 422
        // HookError (arbitrage `Tasks`).
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/widgets")
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"nope": true}"#))
            .expect("valid request");
        let resp = app_ref.clone().oneshot(req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // Toujours aucune ligne insérée : sur aucune de ces branches le hook n'a tourné.
        let resp = app_ref
            .oneshot(json_request("GET", "/api/v1/widgets", &token, None))
            .await
            .expect("router does not fail");
        assert_eq!(json_body(resp).await["total_items"], 0);
    }

    /// `Scenario` : « Trois entités coexistent sous le préfixe figé ».
    #[tokio::test]
    async fn three_entities_coexist_under_fixed_prefix() {
        let db = test_db().await;
        let alice_token = bearer_for(&db, "alice").await;
        let stranger_token = bearer_for(&db, "stranger").await;
        let state = test_state(db);
        let app_ref = app(state);

        // Une recette et un widget créés par alice — le with_state unique scelle l'état des
        // cinq nest assemblés par `app` sans conflit de montage.
        let recipe_body = serde_json::json!({
            "id": 0, "title": "Tarte", "owner_id": 0, "category": "dessert",
        });
        app_ref
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/recipes",
                &alice_token,
                Some(recipe_body),
            ))
            .await
            .expect("create succeeds");
        let widget_body = serde_json::json!({"id": 0, "owner_id": 0, "label": "gadget"});
        app_ref
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/widgets",
                &alice_token,
                Some(widget_body),
            ))
            .await
            .expect("create succeeds");

        // GET /recipes : la page des recettes de alice.
        let recipes = json_body(
            app_ref
                .clone()
                .oneshot(json_request("GET", "/api/v1/recipes", &alice_token, None))
                .await
                .expect("router does not fail"),
        )
        .await;
        assert_eq!(recipes["total_items"], 1);
        assert_eq!(recipes["items"][0]["title"], "Tarte");

        // GET /widgets : la page des widgets — lecture Public, visible du stranger.
        let widgets = json_body(
            app_ref
                .clone()
                .oneshot(json_request("GET", "/api/v1/widgets", &stranger_token, None))
                .await
                .expect("router does not fail"),
        )
        .await;
        assert_eq!(widgets["total_items"], 1);
        assert_eq!(widgets["items"][0]["label"], "GADGET");

        // GET /ingredients : 403 pour le stranger (politique Group "editors") — les routeurs
        // ne se marchent pas dessus.
        let resp = app_ref
            .oneshot(json_request("GET", "/api/v1/ingredients", &stranger_token, None))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(text_body(resp).await, "MRD-REST-002: access denied");
    }

    /// Handler du routeur hôte pour le Scenario suivant — verbe complémentaire maison.
    async fn host_patch_handler() -> (StatusCode, &'static str) {
        (StatusCode::OK, "hote")
    }

    /// `Scenario` : « Le routeur hôte qui ajoute un verbe complémentaire coexiste avec les
    /// routes CRUD ».
    #[tokio::test]
    async fn host_complementary_verb_coexists() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app_ref = Router::new()
            .route("/api/v1/recipes", patch(host_patch_handler))
            .merge(resource_router::<recipe::Entity, MiryadAuthState>())
            .with_state(state);

        let resp = app_ref
            .clone()
            .oneshot(json_request("PATCH", "/api/v1/recipes", &token, None))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(text_body(resp).await, "hote", "c'est le handler hôte qui répond");

        // La fusion des MethodRouters à verbes disjoints n'a rien écrasé : le GET est
        // toujours servi par list_handler (corps PagedResult).
        let resp = app_ref
            .oneshot(json_request("GET", "/api/v1/recipes", &token, None))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            json_body(resp).await.get("total_items").is_some(),
            "le GET passe toujours par list_handler"
        );
    }

    /// `Scenario` : « Deux routers en chevauchement sur un même verbe paniquent au montage,
    /// pas à la requête ».
    #[test]
    #[should_panic(expected = "Overlapping method route")]
    fn overlapping_verb_merge_panics_at_assembly() {
        // Le routeur hôte enregistre lui-même GET /api/v1/recipes (chemin applati identique,
        // verbe get en commun) puis merge le routeur de la même entité : Router::merge panic
        // au montage (axum 0.8.9, track_caller), avant toute requête. ./mod.rs ne déduplique
        // rien — l'unicité du resource_name est une charge de l'app.
        let _merged = Router::new()
            .route("/api/v1/recipes", get(|| async { "" }))
            .merge(resource_router::<recipe::Entity, MiryadAuthState>());
    }

    /// `Scenario` : « Le cookie de session traverse la surface CRUD ».
    #[tokio::test]
    async fn session_cookie_round_trips_crud() {
        let db = test_db().await;
        let alice_token = bearer_for(&db, "alice").await;
        let bob_token = bearer_for(&db, "bob").await;
        let state = test_state(db);
        let cookie = session_cookie_for(&state, "alice");
        let app_ref = app(state);

        // Une recette par propriétaire, via Bearer.
        for token in [&alice_token, &bob_token] {
            let body = serde_json::json!({
                "id": 0, "title": "Tarte", "owner_id": 0, "category": "dessert",
            });
            let resp = app_ref
                .clone()
                .oneshot(json_request("POST", "/api/v1/recipes", token, Some(body)))
                .await
                .expect("create succeeds");
            assert_eq!(resp.status(), StatusCode::CREATED);
        }

        // GET au seul cookie de session chiffré valide, sans en-tête Authorization : 200 —
        // la branche cookie de l'impl FromRequestParts (../auth/dual.sdd) alimente le même
        // AuthPrincipal que les tokens API. La restriction RBAC appliquée est celle de
        // l'utilisatrice du cookie : alice ne voit que sa ligne.
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/recipes")
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request");
        let resp = app_ref.oneshot(req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            json_body(resp).await["total_items"],
            1,
            "OwnerOnly restreint la page au sujet du cookie, la ligne de bob reste hors de portée"
        );
    }

    // Scenario « Entité `OwnerOnly` sans colonne propriétaire panique au montage » (arbitrage
    // 2026-09-27, `Must` « Refuser au montage, par panic ») : `resource_router` panic avant de
    // construire le moindre chemin — l'entité mal déclarée ne répondra jamais `403`/`500` à la
    // première requête. Le message cite l'entité (`ownerless`) et la règle violée.
    #[test]
    #[should_panic(expected = "`ownerless` declares `AccessPolicy::OwnerOnly`")]
    fn owner_only_without_column_panics_at_mount() {
        let _router = resource_router::<ownerless::Entity, MiryadAuthState>();
    }

    // Scenario « Entité à colonne de filtre non textuelle panique au montage » (arbitrage
    // 2026-09-27, même mécanique) : `filter` reste réservé aux colonnes texte, une
    // `filter_column` sur colonne `i32` refuse le montage, message citant l'entité
    // (`numberfilters`) et la règle violée.
    #[test]
    #[should_panic(expected = "`numberfilters` declares `filter_column` on a non-textual column")]
    fn filter_column_on_non_text_column_panics_at_mount() {
        let _router = resource_router::<numberfilter::Entity, MiryadAuthState>();
    }

    // Scenario « Entité à colonne propriétaire non `i32` panique au montage » (arbitrage
    // 2026-09-29, `Must` « Refuser au montage, par panic ») : `owner_column` sur colonne `i64`
    // refuse le montage avant toute route construite — `rbac::evaluate` ne matcherait jamais la
    // colonne et refuserait éternellement hors admin. Message citant l'entité (`bigowner`) et la
    // règle.
    #[test]
    #[should_panic(expected = "`bigowner` declares `owner_column` on a non-`i32` column")]
    fn owner_column_on_non_i32_column_panics_at_mount() {
        let _router = resource_router::<bigowner::Entity, MiryadAuthState>();
    }

    // Contre-épreuve du même Scenario (`But`) : colonne `Option<i32>` nullable, même politique
    // `OwnerOnly` — le type est `ColumnType::Integer`, le montage passe. Verrou du réel attendu
    // vert à sa rédaction (il garde ouverte l'acceptation des colonnes nullables, cas qui
    // relève de ../rbac.sdd).
    #[test]
    fn owner_column_as_nullable_i32_mounts() {
        let _router = resource_router::<optowner::Entity, MiryadAuthState>();
    }
}
