use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;

use crate::auth::cookie::extract_session;
use crate::auth::error::AuthError;
use crate::auth::state::MiryadAuthState;

/// Identité de la requête courante, extraite du cookie de session — pas d'évaluation RBAC ici,
/// juste "qui fait la requête" (cf. feature 3 pour le "a le droit de quoi").
pub struct AuthUser {
    pub subject: String,
    pub email: Option<String>,
    pub id_token: String,
}

impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
    MiryadAuthState: FromRef<S>,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth_state = MiryadAuthState::from_ref(state);

        let cookie_header = parts
            .headers
            .get("Cookie")
            .and_then(|v| v.to_str().ok())
            .map(std::string::ToString::to_string);

        let identity = extract_session(cookie_header.as_deref(), &auth_state.cookie_key)
            .inspect_err(|e| tracing::debug!("auth rejected: {}", e))?;

        tracing::debug!(subject = %identity.subject, "auth ok");
        Ok(Self {
            subject: identity.subject,
            email: identity.email,
            id_token: identity.id_token,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::cookie::build_set_cookie;
    use crate::auth::oidc::{MockOidcClient, OidcIdentity};
    use ::cookie::Key;
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        routing::get,
    };
    use tower::ServiceExt;

    fn test_state() -> MiryadAuthState {
        MiryadAuthState {
            oidc_client: std::sync::Arc::new(MockOidcClient),
            cookie_key: Key::from(&[0u8; 64]),
            post_login_redirect: "/".to_string(),
            post_logout_redirect: "/".to_string(),
        }
    }

    async fn protected_handler(user: AuthUser) -> String {
        user.subject
    }

    fn make_app() -> Router {
        Router::new()
            .route("/protected", get(protected_handler))
            .with_state(test_state())
    }

    #[tokio::test]
    async fn protected_without_cookie_returns_401() {
        let app = make_app();
        let req = Request::builder()
            .uri("/protected")
            .body(Body::empty())
            .expect("valid request");
        let resp = app.oneshot(req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn protected_with_valid_session_passes_and_exposes_subject() {
        let key = Key::from(&[0u8; 64]);
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
            subject: "user-123".to_string(),
            email: Some("test@example.com".to_string()),
        };
        let set_cookie = build_set_cookie(&identity, &key);
        let cookie_value = set_cookie
            .split(';')
            .next()
            .expect("cookie pair present")
            .to_string();

        let app = Router::new()
            .route("/protected", get(protected_handler))
            .with_state(MiryadAuthState {
                oidc_client: std::sync::Arc::new(MockOidcClient),
                cookie_key: key,
                post_login_redirect: "/".to_string(),
                post_logout_redirect: "/".to_string(),
            });
        let req = Request::builder()
            .uri("/protected")
            .header("Cookie", cookie_value)
            .body(Body::empty())
            .expect("valid request");
        let resp = app.oneshot(req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        assert_eq!(&body[..], b"user-123");
    }
}
