pub mod config;
pub mod cookie;
pub mod dual;
pub mod error;
pub mod middleware;
pub mod oidc;
pub mod principal;
pub mod state;
pub mod token;

pub use config::OidcConfig;
pub use error::AuthError;
pub use middleware::AuthUser;
pub use oidc::{OidcClient, OidcClientTrait, OidcIdentity};
pub use principal::{AuthPrincipal, PrincipalSource};
pub use state::MiryadAuthState;
pub use token::{ApiToken, IssuedToken, issue_token, revoke_token, validate_token};

#[cfg(test)]
pub use oidc::MockOidcClient;

use axum::{
    Router,
    extract::{FromRef, Query, State},
    http::{StatusCode, header::SET_COOKIE},
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use ::cookie::{Cookie, CookieJar};

const PENDING_COOKIE_NAME: &str = "miryad_oidc_pending";

/// Sous-routeur `/login`, `/callback`, `/logout` — montable dans n'importe quel `Router<S>` de
/// l'app consommatrice tant que `MiryadAuthState: FromRef<S>` (pattern axum standard pour les
/// sous-états de bibliothèque, pas d'`AppState` concret imposé par miryad-core).
pub fn auth_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>,
{
    Router::new()
        .route("/login", axum::routing::get(handler_login))
        .route("/callback", axum::routing::get(handler_callback))
        .route("/logout", axum::routing::get(handler_logout))
}

#[derive(Deserialize)]
struct CallbackParams {
    code: String,
    state: String,
}

async fn handler_login(State(auth): State<MiryadAuthState>) -> Result<impl IntoResponse, AuthError> {
    let (url, csrf_token, nonce) = auth.oidc_client.authorization_url();

    let pending_value = format!("{}:{}", csrf_token.secret(), nonce.secret());
    let mut jar = CookieJar::new();
    let mut private_jar = jar.private_mut(&auth.cookie_key);
    private_jar.add(Cookie::new(PENDING_COOKIE_NAME, pending_value));
    let encrypted_value = jar.get(PENDING_COOKIE_NAME).map_or("", Cookie::value);
    let set_cookie_pending =
        format!("{PENDING_COOKIE_NAME}={encrypted_value}; HttpOnly; SameSite=Lax; Path=/; Max-Age=300");

    Response::builder()
        .status(StatusCode::FOUND)
        .header("Location", url.as_str())
        .header(SET_COOKIE, set_cookie_pending)
        .body(axum::body::Body::empty())
        .map_err(|e| AuthError::Oidc(e.to_string()))
}

async fn handler_callback(
    State(auth): State<MiryadAuthState>,
    Query(params): Query<CallbackParams>,
    headers: axum::http::HeaderMap,
) -> Result<impl IntoResponse, AuthError> {
    let cookie_header = headers
        .get("Cookie")
        .and_then(|v| v.to_str().ok())
        .map(std::string::ToString::to_string);

    let pending_value = cookie_header
        .as_ref()
        .and_then(|h| {
            h.split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .find_map(|c| {
                    c.split_once('=').and_then(|(name, value)| {
                        if name == PENDING_COOKIE_NAME {
                            Some(value.to_string())
                        } else {
                            None
                        }
                    })
                })
        })
        .ok_or_else(|| AuthError::Oidc("MRD-AUTH-012: missing oidc_pending cookie".to_string()))?;

    let jar = CookieJar::new();
    let private_jar = jar.private(&auth.cookie_key);
    let raw_cookie = Cookie::new(PENDING_COOKIE_NAME, pending_value);
    let decrypted = private_jar
        .decrypt(raw_cookie)
        .ok_or_else(|| AuthError::Oidc("MRD-AUTH-012: invalid oidc_pending cookie".to_string()))?;

    let value = decrypted.value();
    let parts: Vec<&str> = value.splitn(2, ':').collect();
    if parts.len() != 2 {
        return Err(AuthError::Oidc(
            "MRD-AUTH-012: malformed oidc_pending value".to_string(),
        ));
    }
    let expected_csrf = parts[0];
    let nonce = openidconnect::Nonce::new(parts[1].to_string());

    if params.state != expected_csrf {
        tracing::warn!("MRD-AUTH-013: CSRF state mismatch");
        return Err(AuthError::Oidc("MRD-AUTH-013: invalid CSRF state".to_string()));
    }

    let identity = auth.oidc_client.exchange_code(&params.code, &nonce).await?;

    let set_cookie_main = crate::auth::cookie::build_set_cookie(&identity, &auth.cookie_key);
    let set_cookie_clear_pending =
        format!("{PENDING_COOKIE_NAME}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0");

    tracing::info!(subject = %identity.subject, "OIDC authentication successful");

    Response::builder()
        .status(StatusCode::FOUND)
        .header("Location", auth.post_login_redirect.as_str())
        .header(SET_COOKIE, set_cookie_main)
        .header(SET_COOKIE, set_cookie_clear_pending)
        .body(axum::body::Body::empty())
        .map_err(|e| AuthError::Oidc(e.to_string()))
}

async fn handler_logout(State(auth): State<MiryadAuthState>) -> Result<impl IntoResponse, AuthError> {
    Response::builder()
        .status(StatusCode::FOUND)
        .header("Location", auth.post_logout_redirect.as_str())
        .header(SET_COOKIE, crate::auth::cookie::clear_cookie())
        .body(axum::body::Body::empty())
        .map_err(|e| AuthError::Oidc(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    fn test_state() -> MiryadAuthState {
        MiryadAuthState {
            oidc_client: std::sync::Arc::new(MockOidcClient),
            cookie_key: ::cookie::Key::from(&[0u8; 64]),
            post_login_redirect: "/".to_string(),
            post_logout_redirect: "/".to_string(),
            // Ces tests n'exercent que le flow cookie/OIDC — aucune requête n'atteint la base.
            db: sea_orm::MockDatabase::new(sea_orm::DatabaseBackend::Sqlite).into_connection(),
        }
    }

    fn make_app() -> Router {
        auth_router::<MiryadAuthState>().with_state(test_state())
    }

    #[tokio::test]
    async fn logout_clears_cookie_and_redirects() {
        let app = make_app();
        let req = Request::builder()
            .uri("/logout")
            .body(Body::empty())
            .expect("valid request");
        let resp = app.oneshot(req).await.expect("router does not fail");

        assert_eq!(resp.status(), StatusCode::FOUND);
        assert_eq!(resp.headers().get("Location").expect("set"), "/");
        let set_cookie = resp
            .headers()
            .get(SET_COOKIE)
            .expect("clears the session cookie")
            .to_str()
            .expect("ascii header");
        assert!(set_cookie.contains("Max-Age=0"));
        assert!(set_cookie.contains(crate::auth::cookie::SESSION_COOKIE_NAME));
    }

    #[tokio::test]
    async fn callback_without_pending_cookie_is_rejected() {
        let app = make_app();
        let req = Request::builder()
            .uri("/callback?code=test&state=wrong")
            .body(Body::empty())
            .expect("valid request");
        let resp = app.oneshot(req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn login_redirects_and_sets_pending_cookie() {
        let app = make_app();
        let req = Request::builder()
            .uri("/login")
            .body(Body::empty())
            .expect("valid request");
        let resp = app.oneshot(req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::FOUND);
        let set_cookie = resp
            .headers()
            .get(SET_COOKIE)
            .expect("sets the pending cookie")
            .to_str()
            .expect("ascii header");
        assert!(set_cookie.contains(PENDING_COOKIE_NAME));
    }

    #[tokio::test]
    async fn callback_with_csrf_mismatch_is_rejected() {
        let app = make_app();

        let login_req = Request::builder()
            .uri("/login")
            .body(Body::empty())
            .expect("valid request");
        let login_resp = app.clone().oneshot(login_req).await.expect("login ok");
        let pending_cookie = login_resp
            .headers()
            .get(SET_COOKIE)
            .expect("pending cookie set")
            .to_str()
            .expect("ascii header")
            .split(';')
            .next()
            .expect("cookie pair present")
            .to_string();

        let callback_req = Request::builder()
            .uri("/callback?code=irrelevant&state=not-the-real-csrf-token")
            .header("Cookie", pending_cookie)
            .body(Body::empty())
            .expect("valid request");
        let resp = app.oneshot(callback_req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }
}
