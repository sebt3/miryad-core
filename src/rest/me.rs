//! Endpoint self-service exposant l'identité/les groupes du principal courant (issue #24) — un
//! frontend a besoin de savoir "qui je suis" (ex: afficher ou non un lien de nav vers une page
//! admin) sans accès direct aux tables internes de miryad-core, que le projet évite justement de
//! réclamer côté app. Jamais les infos d'un autre utilisateur, dans l'esprit de `tokens_router`.

use axum::extract::{FromRef, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::auth::{AuthPrincipal, MiryadAuthState};
use crate::rest::admin::groups_by_user;
use crate::rest::error::RestError;
use crate::users::resolve_user;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MeResponse {
    pub subject: String,
    pub email: Option<String>,
    pub groups: Vec<String>,
}

/// Monte `GET /api/v1/me` — n'importe quel principal authentifié (dual-auth), toujours restreint
/// à son propre compte, jamais `AdminOnly` (contrairement à `users_router`). Réutilise
/// `MiryadAuthState` comme les autres routeurs. Préfixe `/api/v1` figé, cohérent avec
/// `resource_router`/`tokens_router`/`users_router`.
pub fn me_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>,
{
    Router::new().nest("/api/v1", Router::new().route("/me", get(me_handler)))
}

async fn me_handler(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
) -> Result<Json<MeResponse>, RestError> {
    let caller = resolve_user(&auth.db, &principal.subject, principal.email.as_deref()).await?;
    let groups = groups_by_user(&auth.db, std::iter::once(caller.id))
        .await?
        .remove(&caller.id)
        .unwrap_or_default();

    Ok(Json(MeResponse {
        subject: caller.subject,
        email: caller.email,
        groups,
    }))
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
            .merge(me_router::<MiryadAuthState>())
            .with_state(state)
    }

    async fn json_body(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        serde_json::from_slice(&bytes).expect("valid JSON body")
    }

    fn get_request(token: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri("/api/v1/me")
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .expect("valid request")
    }

    #[tokio::test]
    async fn me_reports_own_identity_and_groups() {
        let db = test_db().await;
        let alice = resolve_user(&db, "alice-sub", Some("alice@example.com"))
            .await
            .expect("resolve succeeds");
        sync_group_memberships(&db, alice.id, &["admin".to_string()])
            .await
            .expect("sync succeeds");
        let token = issue_token(&db, "alice-sub", "test", None)
            .await
            .expect("issuing succeeds")
            .token;

        let resp = app(test_state(db))
            .oneshot(get_request(&token))
            .await
            .expect("router does not fail");

        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["subject"], "alice-sub");
        assert_eq!(body["email"], "alice@example.com");
        assert_eq!(body["groups"], serde_json::json!(["admin"]));
    }

    #[tokio::test]
    async fn me_never_reports_another_users_groups() {
        let db = test_db().await;
        resolve_user(&db, "admin-sub", None)
            .await
            .expect("resolve succeeds");
        let bob = resolve_user(&db, "bob-sub", None)
            .await
            .expect("resolve succeeds");
        sync_group_memberships(&db, bob.id, &["admin".to_string()])
            .await
            .expect("sync succeeds");
        let token = issue_token(&db, "admin-sub", "test", None)
            .await
            .expect("issuing succeeds")
            .token;

        let resp = app(test_state(db))
            .oneshot(get_request(&token))
            .await
            .expect("router does not fail");

        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["subject"], "admin-sub");
        assert_eq!(body["groups"], serde_json::json!([]));
    }
}
