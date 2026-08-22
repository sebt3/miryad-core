pub mod error;

use axum::extract::{FromRef, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, EntityTrait, IntoActiveModel, Iterable, PaginatorTrait,
    PrimaryKeyToColumn, PrimaryKeyTrait, QueryFilter,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::auth::{AuthPrincipal, MiryadAuthState};
use crate::query::{PagedResult, Pagination};
use crate::rbac::{ListAccess, can_create, can_read, can_write, list_access};
use crate::resource::{AccessPolicy, MiryadResource};
use crate::users::resolve_user;
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

fn primary_key_column<E: RestEntity>() -> E::Column {
    E::PrimaryKey::iter()
        .next()
        .expect("RestEntity assumes exactly one primary key column")
        .into_column()
}

/// `Model::into_active_model()` (généré par `DeriveEntityModel`) marque tous les champs
/// `Unchanged`, pas `Set` — pertinent pour un modèle relu depuis la base, pas pour un corps de
/// requête PUT/POST qu'on veut écrire tel quel. `ActiveModelTrait::update()` n'inclut que les
/// champs `Set` dans la clause `SET` ; sans ce passage, un `PUT` n'écrirait aucune colonne
/// (découvert en testant cette feature — le `create` fonctionne quand même car `insert()` traite
/// `Unchanged` comme une valeur à insérer, mais `update()` ne le fait pas).
fn mark_all_set<E: RestEntity>(mut active: E::ActiveModel) -> E::ActiveModel {
    for col in E::Column::iter() {
        if let Some(value) = active.get(col).into_value() {
            active.set(col, value);
        }
    }
    active
}

#[derive(serde::Deserialize)]
struct ListParams {
    page: Option<u64>,
    per_page: Option<u64>,
    filter: Option<String>,
}

/// Monte `GET/POST /{resource_name}` et `GET/PUT/DELETE /{resource_name}/{id}`. Réutilise
/// `MiryadAuthState` (feature 2b) — même état que l'auth, rien de nouveau à composer côté app.
pub fn resource_router<E, S>() -> Router<S>
where
    E: RestEntity,
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>,
{
    let collection_path = format!("/{}", E::resource_name());
    let item_path = format!("/{}/{{id}}", E::resource_name());

    Router::new()
        .route(&collection_path, get(list_handler::<E>).post(create_handler::<E>))
        .route(
            &item_path,
            get(get_handler::<E>)
                .put(update_handler::<E>)
                .delete(delete_handler::<E>),
        )
}

async fn list_handler<E: RestEntity>(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Query(params): Query<ListParams>,
) -> Result<Json<PagedResult<E::Model>>, RestError> {
    let user = resolve_user(&auth.db, &principal.subject, principal.email.as_deref()).await?;

    let condition = match list_access::<E>(&auth.db, &user).await? {
        ListAccess::Unrestricted => Condition::all(),
        ListAccess::FilterByOwner(condition) => condition,
        ListAccess::Forbidden => return Err(RestError::Forbidden),
    };
    let condition = match (E::filter_column(), params.filter.as_deref()) {
        (Some(col), Some(value)) => condition.add(col.eq(value)),
        _ => condition,
    };

    let pagination = Pagination::from_raw(params.page, params.per_page);
    let paginator = E::find()
        .filter(condition)
        .paginate(&auth.db, pagination.per_page);
    let totals = paginator.num_items_and_pages().await?;
    let items = paginator.fetch_page(pagination.page - 1).await?;

    Ok(Json(PagedResult {
        items,
        page: pagination.page,
        per_page: pagination.per_page,
        total_items: totals.number_of_items,
        total_pages: totals.number_of_pages,
    }))
}

async fn get_handler<E: RestEntity>(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Path(id): Path<i32>,
) -> Result<Json<E::Model>, RestError> {
    let user = resolve_user(&auth.db, &principal.subject, principal.email.as_deref()).await?;
    let record = E::find_by_id(id)
        .one(&auth.db)
        .await?
        .ok_or(RestError::NotFound)?;

    if !can_read::<E>(&auth.db, &user, &record).await? {
        return Err(RestError::Forbidden);
    }
    Ok(Json(record))
}

async fn create_handler<E: RestEntity>(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Json(body): Json<E::Model>,
) -> Result<Json<E::Model>, RestError> {
    let user = resolve_user(&auth.db, &principal.subject, principal.email.as_deref()).await?;

    if !can_create::<E>(&auth.db, &user).await? {
        return Err(RestError::Forbidden);
    }

    let mut active = mark_all_set::<E>(body.into_active_model());
    // La BD attribue l'id — jamais une PK choisie par le client.
    active.not_set(primary_key_column::<E>());
    // Un utilisateur ne peut jamais créer une ressource au nom de quelqu'un d'autre, même en le
    // demandant explicitement dans le corps de la requête.
    if E::write_policy() == AccessPolicy::OwnerOnly
        && let Some(owner_col) = E::owner_column()
    {
        active.set(owner_col, sea_orm::Value::from(user.id));
    }

    let inserted = active.insert(&auth.db).await?;
    Ok(Json(inserted))
}

async fn update_handler<E: RestEntity>(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Path(id): Path<i32>,
    Json(body): Json<E::Model>,
) -> Result<Json<E::Model>, RestError> {
    let user = resolve_user(&auth.db, &principal.subject, principal.email.as_deref()).await?;
    let existing = E::find_by_id(id)
        .one(&auth.db)
        .await?
        .ok_or(RestError::NotFound)?;

    if !can_write::<E>(&auth.db, &user, &existing).await? {
        return Err(RestError::Forbidden);
    }

    let mut active = mark_all_set::<E>(body.into_active_model());
    // Force la PK depuis le chemin — ignore toute divergence dans le corps de la requête.
    active.set(primary_key_column::<E>(), sea_orm::Value::from(id));

    let updated = active.update(&auth.db).await?;
    Ok(Json(updated))
}

async fn delete_handler<E: RestEntity>(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Path(id): Path<i32>,
) -> Result<StatusCode, RestError> {
    let user = resolve_user(&auth.db, &principal.subject, principal.email.as_deref()).await?;
    let existing = E::find_by_id(id)
        .one(&auth.db)
        .await?
        .ok_or(RestError::NotFound)?;

    if !can_write::<E>(&auth.db, &user, &existing).await? {
        return Err(RestError::Forbidden);
    }

    E::delete_by_id(id).exec(&auth.db).await?;
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
        db
    }

    fn test_state(db: DatabaseConnection) -> MiryadAuthState {
        MiryadAuthState {
            oidc_client: std::sync::Arc::new(MockOidcClient),
            cookie_key: ::cookie::Key::from(&[0u8; 64]),
            post_login_redirect: "/".to_string(),
            post_logout_redirect: "/".to_string(),
            db,
        }
    }

    fn app(state: MiryadAuthState) -> Router {
        Router::new()
            .merge(resource_router::<recipe::Entity, MiryadAuthState>())
            .merge(resource_router::<ingredient::Entity, MiryadAuthState>())
            .with_state(state)
    }

    async fn bearer_for(db: &DatabaseConnection, subject: &str) -> String {
        issue_token(db, subject, "test", None)
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
            .oneshot(json_request("POST", "/recipes", &token, Some(body)))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
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
                .oneshot(json_request("POST", "/recipes", owner_token, Some(body)))
                .await
                .expect("create succeeds");
            assert_eq!(resp.status(), StatusCode::OK);
        }

        let alice_list = app_ref
            .clone()
            .oneshot(json_request("GET", "/recipes", &alice_token, None))
            .await
            .expect("list succeeds");
        let alice_body = json_body(alice_list).await;
        assert_eq!(alice_body["total_items"], 1);

        let admin_list = app_ref
            .oneshot(json_request("GET", "/recipes", &admin_token, None))
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
                .oneshot(json_request("POST", "/recipes", &token, Some(body)))
                .await
                .expect("create succeeds");
        }

        let resp = app_ref
            .oneshot(json_request("GET", "/recipes?page=2&per_page=1", &token, None))
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
                .oneshot(json_request("POST", "/recipes", token, Some(body)))
                .await
                .expect("create succeeds");
        }

        let resp = app_ref
            .oneshot(json_request("GET", "/recipes?filter=dessert", &alice_token, None))
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
            .oneshot(json_request("GET", "/ingredients", &token, None))
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
            .oneshot(json_request("POST", "/recipes", &alice_token, Some(create_body)))
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
                &format!("/recipes/{id}"),
                &bob_token,
                Some(update_body.clone()),
            ))
            .await
            .expect("router does not fail");
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

        let allowed = app_ref
            .oneshot(json_request(
                "PUT",
                &format!("/recipes/{id}"),
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
    async fn get_and_delete_nonexistent_recipe_return_404() {
        let db = test_db().await;
        let token = bearer_for(&db, "alice").await;
        let state = test_state(db);
        let app_ref = app(state);

        let get_resp = app_ref
            .clone()
            .oneshot(json_request("GET", "/recipes/999999", &token, None))
            .await
            .expect("router does not fail");
        assert_eq!(get_resp.status(), StatusCode::NOT_FOUND);

        let delete_resp = app_ref
            .oneshot(json_request("DELETE", "/recipes/999999", &token, None))
            .await
            .expect("router does not fail");
        assert_eq!(delete_resp.status(), StatusCode::NOT_FOUND);
    }
}
