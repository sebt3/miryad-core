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
            preferred_username: identity.preferred_username,
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
            secure_cookies: false,
            token_pepper: String::new(),
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
        // Email et preferred_username portés par le payload (contrat « Cookie de session
        // valide seul » de dual.sdd et propagation AJOUT REQUIS 2026-09-27) : la session
        // doit les relivrer intacts jusqu'à AuthPrincipal.
        let identity = OidcIdentity {
            id_token: jwt,
            subject: "session-user".to_string(),
            email: Some("session@example.com".to_string()),
            preferred_username: Some("session-name".to_string()),
        };
        build_set_cookie(&identity, &state.cookie_key)
            .split(';')
            .next()
            .expect("cookie pair present")
            .to_string()
    }

    /// Rendu champ à champ du principal — utilisé pour verrouiller la propagation des
    /// champs optionnels sur le chemin cookie (le handler `protected_handler` ne rend
    /// que `subject` et la variante de `source`).
    async fn dump_handler(principal: AuthPrincipal) -> String {
        format!(
            "{}|{}|{}|{}",
            principal.subject,
            principal.email.unwrap_or_default(),
            principal.preferred_username.unwrap_or_default(),
            match principal.source {
                PrincipalSource::Session { .. } => "session",
                PrincipalSource::ApiToken { .. } => "token",
            }
        )
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
            .with_state(state.clone());
        let req = Request::builder()
            .uri("/protected")
            .header("Cookie", cookie.clone())
            .body(Body::empty())
            .expect("valid request");
        let resp = app.oneshot(req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        assert_eq!(&body[..], b"session-user:session");

        // Verrou de propagation (AJOUT REQUIS 2026-09-27, dual.sdd `Must`/`Returns`) :
        // email et preferred_username du payload chiffré traversent extract_session
        // jusqu'au AuthPrincipal rendu, byte pour byte.
        let app = Router::new().route("/dump", get(dump_handler)).with_state(state);
        let req = Request::builder()
            .uri("/dump")
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request");
        let resp = app.oneshot(req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        assert_eq!(
            &body[..],
            b"session-user|session@example.com|session-name|session",
            "le principal de session doit porter le quadruplé du cookie"
        );
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
