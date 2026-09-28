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
/// Refuse au montage deux déclarations invalides détectables sans requête (arbitré
/// 2026-09-27) : `AccessPolicy::OwnerOnly` (lecture ou écriture) avec `owner_column` à `None`,
/// ou `filter_column` désignant une colonne non textuelle. Le message cite l'entité et la règle
/// violée ; une entité mal déclarée ne monte jamais et ne répond jamais à une requête.
pub fn resource_router<E, S>() -> Router<S>
where
    E: RestEntity,
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>,
{
    // Les deux gardes s'exécutent avant toute construction de chemin (mod.sdd `Must` « Refuser
    // au montage, par panic », arbitrage 2026-09-27) — même mécanique que la collision de
    // `resource_name` remontée en panic par `Router::merge`.

    #[allow(clippy::panic)]
    // mod.sdd « Refuser au montage, par panic » — arbitrage 2026-09-27 (OwnerOnly sans owner_column)
    if (matches!(E::read_policy(), AccessPolicy::OwnerOnly)
        || matches!(E::write_policy(), AccessPolicy::OwnerOnly))
        && E::owner_column().is_none()
    {
        panic!(
            "`{}` declares `AccessPolicy::OwnerOnly` with `owner_column` None — invalid MiryadResource declaration, refusing to mount its router",
            E::resource_name()
        );
    }

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
    use super::*;
    use crate::auth::issue_token;
    use crate::auth::oidc::MockOidcClient;
    use crate::migration::Migrator;
    use crate::users::resolve_user as auth_resolve_user;
    use axum::body::Body;
    use axum::http::Request;
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
}
