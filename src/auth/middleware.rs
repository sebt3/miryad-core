use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;

use crate::auth::cookie::extract_session;
use crate::auth::error::AuthError;
use crate::auth::state::MiryadAuthState;

/// Identité de la requête courante, extraite du cookie de session — pas d'évaluation RBAC ici,
/// juste "qui fait la requête" (cf. feature 3 pour le "a le droit de quoi").
///
/// `Debug` est dérivé (arbitré 2026-09-27) : les trois champs sont des données publiques une
/// fois authentifiées, aucun secret à rédiger — `id_token` est déjà lisible par son porteur.
/// `Clone` et `PartialEq` volontairement absents ; type adjacent du flow navigateur, la
/// confusion avec `AuthPrincipal` (dual-auth) est la frontière actée avec `dual.rs`.
#[derive(Debug)]
pub struct AuthUser {
    /// Claim `sub` de l'`id_token` validé, recopié verbatim de l'`OidcIdentity` rendu par
    /// `extract_session` — aucune validation de format ici.
    pub subject: String,
    /// Claim `email` du login, `Some` verbatim ou `None` si le fournisseur ne l'a pas
    /// transmise — métadonnée, jamais une credential.
    pub email: Option<String>,
    /// `id_token` OIDC brut du payload déchiffré, jamais re-sérialisé ni revérifié (la seule
    /// borne de fraîcheur est le contrôle d'`exp` de `cookie.rs`).
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

    fn mock_db() -> sea_orm::DatabaseConnection {
        sea_orm::MockDatabase::new(sea_orm::DatabaseBackend::Sqlite).into_connection()
    }

    fn test_state() -> MiryadAuthState {
        MiryadAuthState {
            oidc_client: std::sync::Arc::new(MockOidcClient::default()),
            cookie_key: Key::from(&[0u8; 64]),
            post_login_redirect: "/".to_string(),
            post_logout_redirect: "/".to_string(),
            db: mock_db(),
            secure_cookies: false,
            token_pepper: String::new(),
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
            preferred_username: None,
        };
        let set_cookie = build_set_cookie(&identity, &key, false);
        let cookie_value = set_cookie
            .split(';')
            .next()
            .expect("cookie pair present")
            .to_string();

        let app = Router::new()
            .route("/protected", get(protected_handler))
            .with_state(MiryadAuthState {
                oidc_client: std::sync::Arc::new(MockOidcClient::default()),
                cookie_key: key,
                post_login_redirect: "/".to_string(),
                post_logout_redirect: "/".to_string(),
                db: mock_db(),
                secure_cookies: false,
                token_pepper: String::new(),
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

    /// `Scenario` : « Debug affiche les trois champs sans panic » (arbitré 2026-09-27) —
    /// aucun secret à rédiger : les trois champs sont déjà lisibles par le porteur
    /// authentifié lui-même.
    #[test]
    fn debug_displays_three_fields_without_panic() {
        let user = AuthUser {
            subject: "user-123".to_string(),
            email: Some("test@example.com".to_string()),
            id_token: "header.payload.sig".to_string(),
        };

        let rendered = format!("{user:?}");
        for attendu in ["AuthUser", "user-123", "test@example.com", "header.payload.sig"] {
            assert!(
                rendered.contains(attendu),
                "le `Debug` doit contenir {attendu:?} : {rendered}"
            );
        }
    }
}
