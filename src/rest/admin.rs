//! Endpoint admin en lecture seule pour lister les utilisateurs et leurs groupes (issue #4). Pas
//! un `MiryadResource` : `User` n'a pas la sémantique CRUD (pas d'owner, jamais de
//! write — Authentik reste la seule source de vérité pour l'appartenance aux groupes, cf.
//! `users::sync_group_memberships`). Un routeur dédié, dans l'esprit d'`auth::auth_router`.

use std::collections::HashMap;

use axum::extract::{FromRef, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
use serde::{Deserialize, Serialize};

use crate::auth::{AuthPrincipal, MiryadAuthState};
use crate::query::{PagedResult, Pagination};
use crate::rest::error::RestError;
use crate::users::{group, is_admin, membership, resolve_user, user};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UserSummary {
    pub id: i32,
    pub subject: String,
    pub email: Option<String>,
    pub groups: Vec<String>,
}

#[derive(Deserialize)]
struct ListParams {
    page: Option<u64>,
    per_page: Option<u64>,
}

/// Monte `GET /api/v1/users` — liste paginée `{ id, subject, email, groups }`, réservée aux
/// membres du groupe admin (`AdminOnly`, cf. `docs/architecture.md` section RBAC). Réutilise
/// `MiryadAuthState` comme les autres routeurs — rien de nouveau à composer côté app. Préfixe
/// `/api/v1` figé, cohérent avec `resource_router` (feature 6).
pub fn users_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>,
{
    Router::new().nest("/api/v1", Router::new().route("/users", get(list_users_handler)))
}

async fn list_users_handler(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Query(params): Query<ListParams>,
) -> Result<Json<PagedResult<UserSummary>>, RestError> {
    let caller = resolve_user(&auth.db, &principal.subject, principal.email.as_deref()).await?;
    if !is_admin(&auth.db, caller.id).await? {
        return Err(RestError::Forbidden);
    }

    let pagination = Pagination::from_raw(params.page, params.per_page);
    let paginator = user::Entity::find().paginate(&auth.db, pagination.per_page);
    let totals = paginator.num_items_and_pages().await?;
    let users = paginator.fetch_page(pagination.page - 1).await?;

    let mut groups_by_user = groups_by_user(&auth.db, users.iter().map(|u| u.id)).await?;

    let items = users
        .into_iter()
        .map(|u| UserSummary {
            groups: groups_by_user.remove(&u.id).unwrap_or_default(),
            id: u.id,
            subject: u.subject,
            email: u.email,
        })
        .collect();

    Ok(Json(PagedResult {
        items,
        page: pagination.page,
        per_page: pagination.per_page,
        total_items: totals.number_of_items,
        total_pages: totals.number_of_pages,
    }))
}

/// Deux requêtes (memberships puis groupes), jamais une par utilisateur — évite le N+1 sur une
/// page de résultats. `is_in` sur une liste vide est explicitement court-circuité (cf.
/// `graphql::principal::load_principal`, même précaution) plutôt que délégué au driver.
/// `pub(crate)` : réutilisée par `rest::me` (issue #24), même besoin "groupes d'un utilisateur".
pub(crate) async fn groups_by_user(
    db: &sea_orm::DatabaseConnection,
    user_ids: impl Iterator<Item = i32>,
) -> Result<HashMap<i32, Vec<String>>, sea_orm::DbErr> {
    let user_ids: Vec<i32> = user_ids.collect();
    if user_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let memberships = membership::Entity::find()
        .filter(membership::Column::UserId.is_in(user_ids))
        .all(db)
        .await?;
    if memberships.is_empty() {
        return Ok(HashMap::new());
    }

    let group_ids: Vec<i32> = memberships.iter().map(|m| m.group_id).collect();
    let group_names: HashMap<i32, String> = group::Entity::find()
        .filter(group::Column::Id.is_in(group_ids))
        .all(db)
        .await?
        .into_iter()
        .map(|g| (g.id, g.name))
        .collect();

    let mut result: HashMap<i32, Vec<String>> = HashMap::new();
    for m in memberships {
        if let Some(name) = group_names.get(&m.group_id) {
            result.entry(m.user_id).or_default().push(name.clone());
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::issue_token;
    use crate::auth::oidc::MockOidcClient;
    use crate::migration::Migrator;
    use crate::users::sync_group_memberships;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use sea_orm::{Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;
    use tower::ServiceExt;

    async fn test_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
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
            .merge(users_router::<MiryadAuthState>())
            .with_state(state)
    }

    async fn json_body(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        serde_json::from_slice(&bytes).expect("valid JSON body")
    }

    fn get_request(uri: &str, token: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(uri)
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .expect("valid request")
    }

    #[tokio::test]
    async fn admin_sees_paginated_users_with_their_groups() {
        let db = test_db().await;
        let admin = resolve_user(&db, "admin-sub", None)
            .await
            .expect("resolve succeeds");
        sync_group_memberships(&db, admin.id, &["admin".to_string()])
            .await
            .expect("sync succeeds");
        let alice = resolve_user(&db, "alice-sub", Some("alice@example.com"))
            .await
            .expect("resolve succeeds");
        sync_group_memberships(&db, alice.id, &["editors".to_string(), "viewers".to_string()])
            .await
            .expect("sync succeeds");
        // bob n'a jamais rejoint de groupe — doit apparaître avec groups: [].
        resolve_user(&db, "bob-sub", None)
            .await
            .expect("resolve succeeds");

        let token = issue_token(&db, "admin-sub", "test", None)
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        let resp = app
            .oneshot(get_request("/api/v1/users", &token))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);

        let body = json_body(resp).await;
        assert_eq!(body["total_items"], 3);
        let items = body["items"].as_array().expect("items array");
        assert_eq!(items.len(), 3);

        let alice_entry = items
            .iter()
            .find(|u| u["subject"] == "alice-sub")
            .expect("alice present");
        assert_eq!(alice_entry["email"], "alice@example.com");
        let mut groups: Vec<&str> = alice_entry["groups"]
            .as_array()
            .expect("groups array")
            .iter()
            .map(|g| g.as_str().expect("group is a string"))
            .collect();
        groups.sort_unstable();
        assert_eq!(groups, vec!["editors", "viewers"]);

        let bob_entry = items
            .iter()
            .find(|u| u["subject"] == "bob-sub")
            .expect("bob present");
        assert_eq!(bob_entry["groups"].as_array().expect("groups array").len(), 0);
    }

    #[tokio::test]
    async fn non_admin_is_forbidden() {
        let db = test_db().await;
        resolve_user(&db, "alice-sub", None)
            .await
            .expect("resolve succeeds");
        let token = issue_token(&db, "alice-sub", "test", None)
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        let resp = app
            .oneshot(get_request("/api/v1/users", &token))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn pagination_params_are_respected() {
        let db = test_db().await;
        let admin = resolve_user(&db, "admin-sub", None)
            .await
            .expect("resolve succeeds");
        sync_group_memberships(&db, admin.id, &["admin".to_string()])
            .await
            .expect("sync succeeds");
        for n in 0..3 {
            resolve_user(&db, &format!("user-{n}"), None)
                .await
                .expect("resolve succeeds");
        }
        // 4 utilisateurs au total (admin + 3) — page 2 à per_page=3 ne renvoie que le dernier.

        let token = issue_token(&db, "admin-sub", "test", None)
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        let resp = app
            .oneshot(get_request("/api/v1/users?page=2&per_page=3", &token))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);

        let body = json_body(resp).await;
        assert_eq!(body["page"], 2);
        assert_eq!(body["per_page"], 3);
        assert_eq!(body["total_items"], 4);
        assert_eq!(body["total_pages"], 2);
        assert_eq!(body["items"].as_array().expect("items array").len(), 1);
    }
}
