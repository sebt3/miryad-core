use axum::extract::{FromRef, FromRequestParts};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;

use crate::auth::cookie::extract_session;
use crate::auth::error::AuthError;
use crate::auth::principal::{AuthPrincipal, PrincipalSource};
use crate::auth::state::MiryadAuthState;
use crate::auth::token::validate_token;

/// Extracteur dual-auth : accepte soit un token API (`Authorization: Bearer <token>`), soit le
/// cookie de session (2a). Si un en-tête `Authorization: Bearer` est présent, il est traité comme
/// le choix explicite du client — pas de repli silencieux sur le cookie s'il est invalide.
impl<S> FromRequestParts<S> for AuthPrincipal
where
    S: Send + Sync,
    MiryadAuthState: FromRef<S>,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth_state = MiryadAuthState::from_ref(state);

        if let Some(token) = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
        {
            return validate_token(&auth_state.db, token).await;
        }

        let cookie_header = parts
            .headers
            .get("Cookie")
            .and_then(|v| v.to_str().ok())
            .map(std::string::ToString::to_string);

        let identity = extract_session(cookie_header.as_deref(), &auth_state.cookie_key)?;

        Ok(AuthPrincipal {
            subject: identity.subject,
            email: identity.email,
            source: PrincipalSource::Session {
                id_token: identity.id_token,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::cookie::build_set_cookie;
    use crate::auth::oidc::{MockOidcClient, OidcIdentity};
    use crate::auth::token::issue_token;
    use crate::migration::Migrator;
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        routing::get,
    };
    use sea_orm_migration::MigratorTrait;
    use tower::ServiceExt;

    async fn test_state() -> MiryadAuthState {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");

        MiryadAuthState {
            oidc_client: std::sync::Arc::new(MockOidcClient),
            cookie_key: cookie::Key::from(&[0u8; 64]),
            post_login_redirect: "/".to_string(),
            post_logout_redirect: "/".to_string(),
            db,
        }
    }

    async fn protected_handler(principal: AuthPrincipal) -> String {
        format!(
            "{}:{}",
            principal.subject,
            match principal.source {
                PrincipalSource::Session { .. } => "session",
                PrincipalSource::ApiToken { .. } => "token",
            }
        )
    }

    fn valid_session_cookie(state: &MiryadAuthState) -> String {
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_secs()
            + 3600;
        let jwt = format!("header.{}.sig", {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#))
        });
        let identity = OidcIdentity {
            id_token: jwt,
            subject: "session-user".to_string(),
            email: None,
        };
        build_set_cookie(&identity, &state.cookie_key)
            .split(';')
            .next()
            .expect("cookie pair present")
            .to_string()
    }

    #[tokio::test]
    async fn bearer_token_authenticates() {
        let state = test_state().await;
        let issued = issue_token(&state.db, "token-user", "test", None)
            .await
            .expect("issuing succeeds");

        let app = Router::new()
            .route("/protected", get(protected_handler))
            .with_state(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Authorization", format!("Bearer {}", issued.token))
            .body(Body::empty())
            .expect("valid request");
        let resp = app.oneshot(req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        assert_eq!(&body[..], b"token-user:token");
    }

    #[tokio::test]
    async fn session_cookie_authenticates_when_no_bearer_header() {
        let state = test_state().await;
        let cookie = valid_session_cookie(&state);

        let app = Router::new()
            .route("/protected", get(protected_handler))
            .with_state(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request");
        let resp = app.oneshot(req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        assert_eq!(&body[..], b"session-user:session");
    }

    #[tokio::test]
    async fn neither_credential_is_rejected() {
        let state = test_state().await;
        let app = Router::new()
            .route("/protected", get(protected_handler))
            .with_state(state);
        let req = Request::builder()
            .uri("/protected")
            .body(Body::empty())
            .expect("valid request");
        let resp = app.oneshot(req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn bearer_header_wins_over_cookie_when_both_present() {
        let state = test_state().await;
        let issued = issue_token(&state.db, "token-user", "test", None)
            .await
            .expect("issuing succeeds");
        let cookie = valid_session_cookie(&state);

        let app = Router::new()
            .route("/protected", get(protected_handler))
            .with_state(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Authorization", format!("Bearer {}", issued.token))
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request");
        let resp = app.oneshot(req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        assert_eq!(&body[..], b"token-user:token");
    }

    #[tokio::test]
    async fn invalid_bearer_token_does_not_fall_back_to_cookie() {
        let state = test_state().await;
        let cookie = valid_session_cookie(&state);

        let app = Router::new()
            .route("/protected", get(protected_handler))
            .with_state(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Authorization", "Bearer mrd_not-a-real-token")
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request");
        let resp = app.oneshot(req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
