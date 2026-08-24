//! Endpoint self-service pour gérer ses propres tokens API (issue #5) — page "mon compte", pas
//! admin : chaque utilisateur ne voit et ne révoque que ses propres tokens. `issue_token`/
//! `revoke_token`/`validate_token` (`auth::token`) existaient déjà comme fonctions Rust mais
//! n'étaient montées derrière aucune route HTTP.

use axum::extract::{FromRef, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use sea_orm::entity::prelude::*;
use sea_orm::{ColumnTrait, PaginatorTrait, QueryFilter};
use serde::{Deserialize, Serialize};

use crate::auth::token::{ApiToken, Column};
use crate::auth::{AuthError, AuthPrincipal, MiryadAuthState, issue_token, revoke_token};
use crate::query::{PagedResult, Pagination};
use crate::rest::error::RestError;

/// `issue_token`/`revoke_token` ne produisent en pratique que `AuthError::Database` — les autres
/// variantes appartiennent au flow OIDC/session, jamais atteintes ici. Conversion explicite
/// plutôt qu'un `From<AuthError>` générique qui laisserait croire à une correspondance 1:1.
fn to_rest_error(err: AuthError) -> RestError {
    match err {
        AuthError::Database(db_err) => RestError::Database(db_err),
        other => RestError::Internal(other.to_string()),
    }
}

/// Jamais la valeur en clair — seul `token::IssuedToken` (retourné une fois, à l'émission) la
/// porte.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TokenSummary {
    pub id: i32,
    pub name: String,
    pub created_at: DateTimeUtc,
    pub expires_at: Option<DateTimeUtc>,
    pub last_used_at: Option<DateTimeUtc>,
}

impl From<crate::auth::token::Model> for TokenSummary {
    fn from(model: crate::auth::token::Model) -> Self {
        Self {
            id: model.id,
            name: model.name,
            created_at: model.created_at,
            expires_at: model.expires_at,
            last_used_at: model.last_used_at,
        }
    }
}

#[derive(Deserialize)]
struct ListParams {
    page: Option<u64>,
    per_page: Option<u64>,
}

#[derive(Deserialize)]
struct CreateTokenBody {
    name: String,
    expires_at: Option<DateTimeUtc>,
}

#[derive(Serialize)]
struct CreatedToken {
    id: i32,
    token: String,
}

/// Monte `GET/POST /api/v1/tokens` et `DELETE /api/v1/tokens/{id}` — n'importe quel principal
/// authentifié (dual-auth), toujours restreint au `subject` courant, jamais les tokens d'un autre
/// utilisateur. Réutilise `MiryadAuthState` comme les autres routeurs. Préfixe `/api/v1` figé,
/// cohérent avec `resource_router` (feature 6).
pub fn tokens_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>,
{
    Router::new().nest(
        "/api/v1",
        Router::new()
            .route("/tokens", get(list_tokens_handler).post(create_token_handler))
            .route("/tokens/{id}", axum::routing::delete(delete_token_handler)),
    )
}

async fn list_tokens_handler(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Query(params): Query<ListParams>,
) -> Result<Json<PagedResult<TokenSummary>>, RestError> {
    let pagination = Pagination::from_raw(params.page, params.per_page);
    let paginator = ApiToken::find()
        .filter(Column::Subject.eq(&principal.subject))
        .paginate(&auth.db, pagination.per_page);
    let totals = paginator.num_items_and_pages().await?;
    let items = paginator
        .fetch_page(pagination.page - 1)
        .await?
        .into_iter()
        .map(TokenSummary::from)
        .collect();

    Ok(Json(PagedResult {
        items,
        page: pagination.page,
        per_page: pagination.per_page,
        total_items: totals.number_of_items,
        total_pages: totals.number_of_pages,
    }))
}

async fn create_token_handler(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Json(body): Json<CreateTokenBody>,
) -> Result<Json<CreatedToken>, RestError> {
    let issued = issue_token(&auth.db, &principal.subject, &body.name, body.expires_at)
        .await
        .map_err(to_rest_error)?;
    Ok(Json(CreatedToken {
        id: issued.id,
        token: issued.token,
    }))
}

async fn delete_token_handler(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Path(id): Path<i32>,
) -> Result<StatusCode, RestError> {
    let record = ApiToken::find_by_id(id)
        .one(&auth.db)
        .await?
        .ok_or(RestError::NotFound)?;
    if record.subject != principal.subject {
        return Err(RestError::Forbidden);
    }
    revoke_token(&auth.db, id).await.map_err(to_rest_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::oidc::MockOidcClient;
    use crate::migration::Migrator;
    use axum::body::Body;
    use axum::http::Request;
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
            .merge(tokens_router::<MiryadAuthState>())
            .with_state(state)
    }

    async fn json_body(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        serde_json::from_slice(&bytes).expect("valid JSON body")
    }

    fn request(method: &str, uri: &str, token: &str, body: Option<serde_json::Value>) -> Request<Body> {
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

    #[tokio::test]
    async fn create_then_list_returns_the_token_without_the_cleartext_value() {
        let db = test_db().await;
        let bootstrap = issue_token(&db, "alice", "bootstrap", None)
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        let create_body = serde_json::json!({ "name": "cli laptop" });
        let created = app
            .clone()
            .oneshot(request("POST", "/api/v1/tokens", &bootstrap, Some(create_body)))
            .await
            .expect("router does not fail");
        assert_eq!(created.status(), StatusCode::OK);
        let created_body = json_body(created).await;
        let cleartext = created_body["token"].as_str().expect("token present").to_string();
        assert!(cleartext.starts_with("mrd_"));

        let listed = app
            .oneshot(request("GET", "/api/v1/tokens", &bootstrap, None))
            .await
            .expect("router does not fail");
        assert_eq!(listed.status(), StatusCode::OK);
        let listed_body = json_body(listed).await;
        assert_eq!(listed_body["total_items"], 2);
        let items = listed_body["items"].as_array().expect("items array");
        let new_entry = items
            .iter()
            .find(|t| t["name"] == "cli laptop")
            .expect("new token present in the list");
        assert!(
            new_entry.get("token").is_none(),
            "cleartext value must never be listed"
        );
        assert_eq!(new_entry["id"], created_body["id"]);
    }

    #[tokio::test]
    async fn list_only_returns_the_caller_own_tokens() {
        let db = test_db().await;
        let alice_token = issue_token(&db, "alice", "alice's token", None)
            .await
            .expect("issuing succeeds")
            .token;
        issue_token(&db, "bob", "bob's token", None)
            .await
            .expect("issuing succeeds");
        let app = app(test_state(db));

        let resp = app
            .oneshot(request("GET", "/api/v1/tokens", &alice_token, None))
            .await
            .expect("router does not fail");
        let body = json_body(resp).await;
        assert_eq!(body["total_items"], 1);
        assert_eq!(body["items"][0]["name"], "alice's token");
    }

    #[tokio::test]
    async fn owner_can_revoke_their_own_token() {
        let db = test_db().await;
        let alice_token = issue_token(&db, "alice", "alice's token", None)
            .await
            .expect("issuing succeeds")
            .token;
        let to_revoke = issue_token(&db, "alice", "to revoke", None)
            .await
            .expect("issuing succeeds");
        let app = app(test_state(db));

        let resp = app
            .clone()
            .oneshot(request(
                "DELETE",
                &format!("/api/v1/tokens/{}", to_revoke.id),
                &alice_token,
                None,
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let listed = app
            .oneshot(request("GET", "/api/v1/tokens", &alice_token, None))
            .await
            .expect("router does not fail");
        let body = json_body(listed).await;
        assert_eq!(body["total_items"], 1);
    }

    #[tokio::test]
    async fn cannot_revoke_someone_else_token() {
        let db = test_db().await;
        let alice_token = issue_token(&db, "alice", "alice's token", None)
            .await
            .expect("issuing succeeds")
            .token;
        let bobs_token = issue_token(&db, "bob", "bob's token", None)
            .await
            .expect("issuing succeeds");
        let db_check = db.clone();
        let app = app(test_state(db));

        let resp = app
            .oneshot(request(
                "DELETE",
                &format!("/api/v1/tokens/{}", bobs_token.id),
                &alice_token,
                None,
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // toujours vivant, jamais révoqué par l'appelante non-propriétaire.
        let principal = crate::auth::validate_token(&db_check, &bobs_token.token)
            .await
            .expect("bob's token remains valid");
        assert_eq!(principal.subject, "bob");
    }

    #[tokio::test]
    async fn deleting_an_unknown_token_returns_not_found() {
        let db = test_db().await;
        let alice_token = issue_token(&db, "alice", "alice's token", None)
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        let resp = app
            .oneshot(request("DELETE", "/api/v1/tokens/999999", &alice_token, None))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
