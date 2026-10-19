use axum::extract::{FromRef, FromRequestParts};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;

use crate::auth::cookie::extract_session;
use crate::auth::error::AuthError;
use crate::auth::principal::{AuthPrincipal, PrincipalSource};
use crate::auth::state::MiryadAuthState;
#[cfg(feature = "db")]
use crate::auth::token::validate_token;

/// Extrait le matériau du token derrière un schème `Bearer` reconnu insensible à la casse
/// (`Bearer`, `bearer`, `BEARER`, toute combinaison — arbitré 2026-09-27, `src/auth/dual.sdd`,
/// conforme RFC 7235/9110) : les six premiers caractères comparés via `eq_ignore_ascii_case`
/// sur la tranche `get(..6)`, puis l'espace unique exact qui suit est consommé. Le matériau
/// reste opaque et non trimé. `None` (valeur traitée comme absente, repli cookie) si la
/// valeur est plus courte que le schème, si sa limite d'octet 6 n'est pas une frontière
/// caractérielle, ou si l'espace exigé après le schème manque.
fn bearer_token_material(value: &str) -> Option<&str> {
    const SCHEME: &str = "Bearer";
    value
        .get(..SCHEME.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(SCHEME))
        .and_then(|_| value.get(SCHEME.len()..))
        .and_then(|rest| rest.strip_prefix(' '))
}

/// Matériau JWT-shaped (amendement 2026-10-19, `src/auth/dual.sdd`) : exactement deux `.`.
/// Règle déterministe, aucun parse : un token API `mrd_…` (base64url) ne contient jamais `.`.
fn is_jwt_shaped(material: &str) -> bool {
    material.matches('.').count() == 2
}

/// Chemin JWT Bearer (amendement 2026-10-19) : délégué à `verify_bearer` (synchrone), sans
/// aucune résolution d'utilisateur. Succès → `PrincipalSource::Bearer { jwt }` ; échec → l'erreur
/// de `verify_bearer` propagée telle quelle.
fn verify_bearer_jwt(state: &MiryadAuthState, jwt: &str) -> Result<AuthPrincipal, AuthError> {
    let identity = state.oidc_client.verify_bearer(jwt)?;
    Ok(AuthPrincipal {
        subject: identity.subject,
        email: identity.email,
        preferred_username: identity.preferred_username,
        source: PrincipalSource::Bearer {
            jwt: identity.id_token,
        },
    })
}

/// Chemin token API (`db`) : inchangé, délégué à `validate_token`.
#[cfg(feature = "db")]
async fn authenticate_api_token(state: &MiryadAuthState, token: &str) -> Result<AuthPrincipal, AuthError> {
    validate_token(&state.db, token, &state.token_pepper).await
}

/// Sans `db` il n'existe pas de token API : rejet immédiat `MRD-AUTH-019`, sans aucune E/S
/// (amendement 2026-10-19). `Ready` garde l'appelant identique sous les deux configurations.
#[cfg(not(feature = "db"))]
fn authenticate_api_token(
    _state: &MiryadAuthState,
    _token: &str,
) -> std::future::Ready<Result<AuthPrincipal, AuthError>> {
    std::future::ready(Err(AuthError::InvalidBearer))
}

/// Extracteur dual-auth : accepte soit un token API (`Authorization: Bearer <token>`), soit le
/// cookie de session (2a). Le schème `Bearer` est reconnu insensible à la casse (arbitré
/// 2026-09-27) ; quand ce schème reconnu est présent, il est traité comme le choix explicite
/// du client — pas de repli silencieux sur le cookie s'il est invalide. Un `Authorization`
/// d'un autre schème (`Basic`, valeur illisible en ASCII, etc.) est traité comme absent : le
/// cookie reprend la main (comportement gelé, arbitré 2026-09-27).
///
/// Trace `debug` sur chaque résolution (`auth ok` / `auth rejected: <code>`), champ structuré
/// `path` (`bearer`/`token`/`session`), jamais le token ni le cookie — alignée sur `AuthUser`
/// (`middleware.rs`, arbitré 2026-09-27).
impl<S> FromRequestParts<S> for AuthPrincipal
where
    S: Send + Sync,
    MiryadAuthState: FromRef<S>,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth_state = MiryadAuthState::from_ref(state);

        if let Some(material) = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(bearer_token_material)
        {
            // Choix définitif : aucun repli cookie, quel que soit le résultat (amendement 2026-10-19).
            let (path, outcome) = if is_jwt_shaped(material) {
                ("bearer", verify_bearer_jwt(&auth_state, material))
            } else {
                ("token", authenticate_api_token(&auth_state, material).await)
            };
            match &outcome {
                Ok(_) => tracing::debug!(path, "auth ok"),
                Err(err) => tracing::debug!(path, "auth rejected: {err}"),
            }
            return outcome;
        }

        let cookie_header = parts
            .headers
            .get("Cookie")
            .and_then(|v| v.to_str().ok())
            .map(std::string::ToString::to_string);

        let identity = match extract_session(cookie_header.as_deref(), &auth_state.cookie_key) {
            Ok(identity) => identity,
            Err(err) => {
                tracing::debug!(path = "session", "auth rejected: {err}");
                return Err(err);
            }
        };

        tracing::debug!(path = "session", "auth ok");
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
    #[cfg(feature = "db")]
    use crate::auth::token::{issue_token, revoke_token};
    #[cfg(feature = "db")]
    use crate::migration::Migrator;
    use axum::{
        Router,
        body::Body,
        http::{HeaderValue, Request, StatusCode},
        routing::get,
    };
    #[cfg(feature = "db")]
    use sea_orm_migration::MigratorTrait;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    /// Corps `text/plain` exacts rendus par `AuthError::into_response` (`src/auth/error.sdd`) —
    /// chaque rejet affirmé ici l'est par la chaîne entière, jamais par le seul statut.
    const BODY_001: &str = "MRD-AUTH-001: not authenticated (no session cookie)";
    const BODY_002: &str = "MRD-AUTH-002: invalid or expired session";
    const BODY_014: &str = "MRD-AUTH-014: invalid or unknown API token";
    const BODY_015: &str = "MRD-AUTH-015: expired API token";
    const BODY_019: &str = "MRD-AUTH-019: invalid bearer token";
    const BODY_020: &str = "MRD-AUTH-020: expired bearer token";

    #[cfg(feature = "db")]
    fn state_around(db: sea_orm::DatabaseConnection) -> MiryadAuthState {
        MiryadAuthState {
            oidc_client: std::sync::Arc::new(MockOidcClient::default()),
            cookie_key: cookie::Key::from(&[0u8; 64]),
            post_login_redirect: "/".to_string(),
            post_logout_redirect: "/".to_string(),
            db,
            secure_cookies: false,
            token_pepper: String::new(),
        }
    }

    #[cfg(feature = "db")]
    async fn test_state() -> MiryadAuthState {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        state_around(db)
    }

    /// `test_state` dont le client OIDC est le mock configuré (amendement 2026-10-19).
    #[cfg(feature = "db")]
    async fn test_state_with_oidc(oidc: MockOidcClient) -> MiryadAuthState {
        MiryadAuthState {
            oidc_client: std::sync::Arc::new(oidc),
            ..test_state().await
        }
    }

    /// État sur base `SQLite` en mémoire sans aucune migration appliquée : la table
    /// `miryad_api_tokens` n'existe pas (scenarios `MRD-AUTH-016` et insensibilité du
    /// chemin cookie à la base).
    #[cfg(feature = "db")]
    async fn unmigrated_state() -> MiryadAuthState {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        state_around(db)
    }

    /// Identité vérifiée d'un JWT Bearer des scenarios 2026-10-19 : `user-1`, avec email et
    /// `preferred_username` pour affirmer leur propagation ; `id_token` = le JWT reçu.
    fn bearer_identity(jwt: &str) -> OidcIdentity {
        OidcIdentity {
            id_token: jwt.to_string(),
            subject: "user-1".to_string(),
            email: Some("user-1@example.com".to_string()),
            preferred_username: Some("user-one".to_string()),
        }
    }

    async fn protected_handler(principal: AuthPrincipal) -> String {
        format!(
            "{}:{}",
            principal.subject,
            match principal.source {
                PrincipalSource::Session { .. } => "session",
                PrincipalSource::ApiToken { .. } => "token",
                PrincipalSource::Bearer { .. } => "bearer",
            }
        )
    }

    /// Rendu champ à champ du principal : `subject|email|preferred_username|source`, la
    /// charge utile de `source` rendue verbatim (`token:<token_id>` / `session:<id_token>`)
    /// pour affirmer les `Then` sur `token_id` (chemin token) et `id_token` octet pour
    /// octet (chemin session). Les `Option` `None` rendent la chaîne vide.
    async fn dump_handler(principal: AuthPrincipal) -> String {
        let source = match principal.source {
            PrincipalSource::Session { id_token } => format!("session:{id_token}"),
            PrincipalSource::ApiToken { token_id } => format!("token:{token_id}"),
            PrincipalSource::Bearer { jwt } => format!("bearer:{jwt}"),
        };
        format!(
            "{}|{}|{}|{source}",
            principal.subject,
            principal.email.unwrap_or_default(),
            principal.preferred_username.unwrap_or_default(),
        )
    }

    fn protected_app(state: MiryadAuthState) -> Router {
        Router::new()
            .route("/protected", get(protected_handler))
            .with_state(state)
    }

    fn dump_app(state: MiryadAuthState) -> Router {
        Router::new().route("/dump", get(dump_handler)).with_state(state)
    }

    /// (statut, corps UTF-8) d'une requête servie par `oneshot` — le corps est affirmé
    /// par chaîne exacte dans tout scenario de rejet.
    async fn serve(app: Router, req: Request<Body>) -> (StatusCode, String) {
        let resp = app.oneshot(req).await.expect("router does not fail");
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    /// JWT tri-segment dont la claim `exp` vaut `exp` (base64 url-safe sans bourrage).
    fn make_jwt(exp: u64) -> String {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("header.{payload}.sig")
    }

    fn future_exp() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_secs()
            + 3600
    }

    /// Identité de session des scenarios : `session-user`, email et `preferred_username`
    /// portés par le payload (propagation AJOUT REQUIS 2026-09-27), `id_token` donné.
    fn session_identity(id_token: String) -> OidcIdentity {
        OidcIdentity {
            id_token,
            subject: "session-user".to_string(),
            email: Some("session@example.com".to_string()),
            preferred_username: Some("session-name".to_string()),
        }
    }

    /// Paire `nom=valeur` isolée d'un en-tête `Set-Cookie` (premier segment avant `;`).
    fn cookie_pair(set_cookie: &str) -> String {
        set_cookie
            .split(';')
            .next()
            .expect("cookie pair present")
            .to_string()
    }

    /// Cookie `miryad_session` valide signé de la clé de l'état (`exp` dans une heure).
    fn valid_session_cookie(state: &MiryadAuthState) -> String {
        cookie_pair(&build_set_cookie(
            &session_identity(make_jwt(future_exp())),
            &state.cookie_key,
            state.secure_cookies,
        ))
    }

    /// Cookie bien scellé sous la clé de l'état mais dont l'`exp` interne est passée.
    fn expired_session_cookie(state: &MiryadAuthState) -> String {
        cookie_pair(&build_set_cookie(
            &session_identity(make_jwt(1_000_000)),
            &state.cookie_key,
            state.secure_cookies,
        ))
    }

    /// Cookie scellé d'une AUTRE clé — indéchiffrable par l'état : leurre du scenario
    /// « Bearer évince le cookie sans le lire », et scenario « autre clé » en propre.
    fn other_key_session_cookie(state: &MiryadAuthState) -> String {
        let other_key = cookie::Key::from(&[7u8; 64]);
        cookie_pair(&build_set_cookie(
            &session_identity(make_jwt(future_exp())),
            &other_key,
            state.secure_cookies,
        ))
    }

    // ——— Capture des traces (`Scenario` traces, stratégie fixture alignée sur `oidc.rs`) ———

    #[derive(Clone, Default)]
    struct CapturedTraces(Arc<Mutex<Vec<String>>>);

    impl CapturedTraces {
        fn lines(&self) -> Vec<String> {
            self.0.lock().expect("test mutex is not poisoned").clone()
        }
    }

    struct CaptureLayer(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            if !event.metadata().target().starts_with("miryad_core") {
                return;
            }
            let mut visitor = MessageVisitor::default();
            event.record(&mut visitor);
            self.0
                .lock()
                .expect("test mutex is not poisoned")
                .push(visitor.finish());
        }
    }

    #[derive(Default)]
    struct MessageVisitor {
        rendered: String,
    }

    impl MessageVisitor {
        fn finish(&self) -> String {
            self.rendered.clone()
        }
    }

    impl tracing::field::Visit for MessageVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;

            if !self.rendered.is_empty() {
                self.rendered.push(' ');
            }
            let _ = write!(&mut self.rendered, "{}={value:?}", field.name());
        }
    }

    fn capture_traces() -> (tracing::subscriber::DefaultGuard, CapturedTraces) {
        use tracing_subscriber::layer::SubscriberExt as _;

        let captured = CapturedTraces::default();
        let subscriber =
            tracing_subscriber::registry::Registry::default().with(CaptureLayer(Arc::clone(&captured.0)));
        let guard = tracing::subscriber::set_default(subscriber);
        (guard, captured)
    }

    // ——— Les 19 Scenario de `dual.sdd` ———

    /// `Scenario` : « Bearer token valide seul » — Then complets (renforcé 2026-09-27) :
    /// `token_id` égal à la clé primaire émise, `email` `None`, `preferred_username` `None`.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn bearer_token_authenticates() {
        let state = test_state().await;
        let issued = issue_token(&state.db, "token-user", "test", None, &state.token_pepper)
            .await
            .expect("issuing succeeds");

        let app = dump_app(state);
        let req = Request::builder()
            .uri("/dump")
            .header("Authorization", format!("Bearer {}", issued.token))
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            format!("token-user|||token:{}", issued.id),
            "chemin token : subject de la ligne, email et preferred_username `None`, \
             token_id égal à la clé primaire de la ligne émise"
        );
    }

    /// `Scenario` : « Cookie de session valide seul » — Then complets (renforcé 2026-09-27) :
    /// `email` `Some` exact et `id_token` identique octet pour octet au payload du cookie.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn session_cookie_authenticates_when_no_bearer_header() {
        let state = test_state().await;
        let jwt = make_jwt(future_exp());
        let cookie = cookie_pair(&build_set_cookie(
            &session_identity(jwt.clone()),
            &state.cookie_key,
            state.secure_cookies,
        ));

        let app = dump_app(state);
        let req = Request::builder()
            .uri("/dump")
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            format!("session-user|session@example.com|session-name|session:{jwt}"),
            "Verrou de propagation (AJOUT REQUIS 2026-09-27) : le quadruplé du cookie traverse \
             extract_session jusqu'au AuthPrincipal, id_token inclus octet pour octet"
        );
    }

    /// `Scenario` : « Bearer évince le cookie sans le lire » — renforcé 2026-09-27 : le
    /// cookie valide est remplacé par un cookie INDECHIFFRABLE (clé différente) ; s'il
    /// était ne serait-ce que consulté, un `MRD-AUTH-002` surgirait.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn bearer_header_wins_over_cookie_when_both_present() {
        let state = test_state().await;
        let issued = issue_token(&state.db, "token-user", "test", None, &state.token_pepper)
            .await
            .expect("issuing succeeds");
        let decoy = other_key_session_cookie(&state);

        let app = protected_app(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Authorization", format!("Bearer {}", issued.token))
            .header("Cookie", decoy)
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body, "token-user:token",
            "le chemin token réussit alors que le cookie présent est indéchiffrable : \
             aucun `MRD-AUTH-002` ne surgit, le cookie n'est jamais examiné"
        );
    }

    /// `Scenario` : « Aucune credential » — renforcé 2026-09-27 : corps exact affirmé,
    /// pas seulement le statut.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn neither_credential_is_rejected() {
        let state = test_state().await;
        let app = protected_app(state);
        let req = Request::builder()
            .uri("/protected")
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, BODY_001);
    }

    /// `Scenario` : « Bearer invalide sans repli sur le cookie valide » — renforcé
    /// 2026-09-27 : corps `MRD-AUTH-014` exact affirmé, le cookie valide n'est jamais
    /// consulté (sinon ce serait `200`, ou `MRD-AUTH-*` d'une session).
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn invalid_bearer_token_does_not_fall_back_to_cookie() {
        let state = test_state().await;
        let cookie = valid_session_cookie(&state);

        let app = protected_app(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Authorization", "Bearer mrd_not-a-real-token")
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, BODY_014);
    }

    /// `Scenario` : « Token expiré rendu par 015 sans repli » — la distinction
    /// `014`/`015` ne se voit que par le corps, cookie valide présent pourtant.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn expired_token_renders_015_without_fallback() {
        let state = test_state().await;
        let past = sea_orm::prelude::DateTimeUtc::from_timestamp(chrono::Utc::now().timestamp() - 60, 0)
            .expect("valid timestamp");
        let issued = issue_token(&state.db, "token-user", "doomed", Some(past), &state.token_pepper)
            .await
            .expect("issuing with a past expires_at succeeds");
        let cookie = valid_session_cookie(&state);

        let app = protected_app(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Authorization", format!("Bearer {}", issued.token))
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            body, BODY_015,
            "ni MRD-AUTH-001 ni session acceptée : seul le corps distingue 015 de 014"
        );
    }

    /// `Scenario` : « Token revoqué rendu par 014 » — indiscernable du token inconnu,
    /// sans en-tête `Cookie`.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn revoked_token_renders_014() {
        let state = test_state().await;
        let issued = issue_token(&state.db, "token-user", "to revoke", None, &state.token_pepper)
            .await
            .expect("issuing succeeds");
        revoke_token(&state.db, issued.id)
            .await
            .expect("revocation succeeds");

        let app = protected_app(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Authorization", format!("Bearer {}", issued.token))
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, BODY_014);
    }

    /// `Scenario` : « Session expirée rendue par 002 » — cookie bien formé, `exp` passée.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn expired_session_renders_002() {
        let state = test_state().await;
        let cookie = expired_session_cookie(&state);

        let app = protected_app(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, BODY_002);
    }

    /// `Scenario` : « Session signée d'une autre clé rendue par 002 » — pas
    /// `MRD-AUTH-001` : le cookie a été trouvé puis refusé, non pas absent (l'un et
    /// l'autre corps sont exclusifs, affirmés par chaîne exacte).
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn session_signed_with_other_key_renders_002() {
        let state = test_state().await;
        let cookie = other_key_session_cookie(&state);

        let app = protected_app(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, BODY_002);
    }

    /// `Scenario` : « Schème Basic retombe sur le cookie » — gelé (arbitré 2026-09-27) :
    /// un schème qui n'égale pas `Bearer` n'est pas un choix Bearer, le cookie reprend
    /// la main.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn basic_scheme_falls_back_to_cookie() {
        let state = test_state().await;
        let cookie = valid_session_cookie(&state);

        let app = protected_app(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Authorization", "Basic dXNlcjpwYXNz")
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "session-user:session");
    }

    /// `Scenario` : « Schème en casse basse authentifie désormais par le token » —
    /// remplace l'ancien test de repli cookie (`Then` inversé, arbitré 2026-09-27) :
    /// `bearer` (et toute combinaison de casse) est reconnu comme `Bearer`, sans cookie.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn lowercase_bearer_scheme_now_authenticates() {
        let state = test_state().await;
        let issued = issue_token(&state.db, "token-user", "test", None, &state.token_pepper)
            .await
            .expect("issuing succeeds");

        let app = dump_app(state.clone());
        let req = Request::builder()
            .uri("/dump")
            .header("authorization", format!("bearer {}", issued.token))
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            format!("token-user|||token:{}", issued.id),
            "le schème est insensible à la casse (RFC 7235/9110, arbitré 2026-09-27) : \
             `bearer` authentifie par le chemin token, plus de repli cookie pour ce cas"
        );

        // Toute autre combinaison de casse : `BEARER` est reconnu de même.
        let app = dump_app(state);
        let req = Request::builder()
            .uri("/dump")
            .header("authorization", format!("BEARER {}", issued.token))
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, format!("token-user|||token:{}", issued.id));
    }

    /// `Scenario` : « Bearer de token vide est un chemin définitif » — la chaîne vide
    /// est hachée et cherchée comme un vrai token, cookie valide présent mais non consulté.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn empty_bearer_is_a_terminal_014() {
        let state = test_state().await;
        let cookie = valid_session_cookie(&state);

        let app = protected_app(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Authorization", "Bearer ")
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, BODY_014);
    }

    /// `Scenario` : « Espace parasite du bearer aucun trim » — le matériau passé à
    /// `validate_token` commence par cet espace supplémentaire, aucune seconde tentative
    /// trimée ni repli sur le cookie.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn parasite_space_bearer_is_not_trimmed() {
        let state = test_state().await;
        let issued = issue_token(&state.db, "token-user", "test", None, &state.token_pepper)
            .await
            .expect("issuing succeeds");
        let cookie = valid_session_cookie(&state);

        let app = protected_app(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Authorization", format!("Bearer  {}", issued.token))
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            body, BODY_014,
            "le matériau ` <token>` (espace initial inclus) n'est jamais trimé : la lookup \
             ne trouve rien et le cookie n'est pas consulté"
        );
    }

    /// `Scenario` : « Deux en-têtes Authorization la première seule compte » — le Bearer
    /// de la seconde en-tête n'est jamais vu (comportement `get` de `HeaderMap`, gelé).
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn first_authorization_header_alone_counts() {
        let state = test_state().await;
        let issued = issue_token(&state.db, "token-user", "test", None, &state.token_pepper)
            .await
            .expect("issuing succeeds");

        let app = protected_app(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Authorization", "Basic x")
            .header("Authorization", format!("Bearer {}", issued.token))
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, BODY_001);
    }

    /// `Scenario` : « Octet illisible de Authorization retombe sur le cookie » — `to_str`
    /// échoue, la valeur est traitée comme absente ; l'événement `auth ok` capture ne
    /// porte ni la valeur illisible ni le cookie, seulement `session`.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn unreadable_authorization_falls_back_to_cookie() {
        let state = test_state().await;
        let jwt = make_jwt(future_exp());
        let cookie = cookie_pair(&build_set_cookie(
            &session_identity(jwt.clone()),
            &state.cookie_key,
            state.secure_cookies,
        ));
        let app = protected_app(state);

        let (_guard, captured) = capture_traces();
        let req = Request::builder()
            .uri("/protected")
            .header(
                AUTHORIZATION,
                HeaderValue::from_bytes(b"Bearer \xff")
                    .expect("http 1.5.0 accepts any byte but NUL, CR and LF"),
            )
            .header("Cookie", cookie.clone())
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "session-user:session");

        let lines = captured.lines();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("auth ok") && line.contains("session")),
            "`auth ok` de debug avec le chemin `session` (arbitré 2026-09-27) : {lines:?}"
        );
        assert!(
            lines
                .iter()
                .all(|line| !line.contains(&cookie) && !line.contains(&jwt)),
            "aucune trace ne porte le cookie, sa valeur ou l'id_token : {lines:?}"
        );
    }

    /// `Scenario` : « Deux en-têtes Cookie la première seule compte » — le cookie de
    /// session de la seconde en-tête n'est jamais lu (gelé, arbitré 2026-09-27).
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn first_cookie_header_alone_counts() {
        let state = test_state().await;
        let cookie = valid_session_cookie(&state);

        let app = protected_app(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Cookie", "autre=1")
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, BODY_001);
    }

    /// `Scenario` : « Table token absente rendue 016 en 500 » — base sans migration,
    /// le corps commence par le préfixe `MRD-AUTH-016: database error: `.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn missing_token_table_renders_016_in_500() {
        let state = unmigrated_state().await;
        let app = protected_app(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Authorization", "Bearer quoi_que_ce_soit")
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            body.starts_with("MRD-AUTH-016: database error: "),
            "corps attendu préfixé MRD-AUTH-016, reçu : {body}"
        );
    }

    /// `Scenario` : « Session insensible à une base absente » — le chemin cookie ne
    /// touche jamais `db` : sans la moindre table, une session valide authentifie.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn session_is_insensitive_to_missing_token_table() {
        let state = unmigrated_state().await;
        let cookie = valid_session_cookie(&state);

        let app = protected_app(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body, "session-user:session",
            "aucun accès à la base sur le chemin cookie : une base sans migration n'y change rien"
        );
    }

    /// `Scenario` : « Les traces de résolution ne portent jamais de credential » —
    /// premier événement `auth ok` avec le chemin `token`, second `auth rejected` avec
    /// le chemin `token` et le code `MRD-AUTH-014`, aucun événement ne porte un secret.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn successful_and_rejected_resolutions_emit_debug_traces_without_leaking_credentials() {
        let state = test_state().await;
        let issued = issue_token(&state.db, "token-user", "test", None, &state.token_pepper)
            .await
            .expect("issuing succeeds");

        let (_guard, captured) = capture_traces();

        let app = protected_app(state.clone());
        let req = Request::builder()
            .uri("/protected")
            .header("Authorization", format!("Bearer {}", issued.token))
            .body(Body::empty())
            .expect("valid request");
        let (status, _) = serve(app, req).await;
        assert_eq!(status, StatusCode::OK);

        let app = protected_app(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Authorization", "Bearer mrd_inconnu")
            .body(Body::empty())
            .expect("valid request");
        let (status, _) = serve(app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let lines = captured.lines();
        assert_eq!(
            lines.len(),
            2,
            "exactement deux événements miryad_core (couche moteur muette par principe) : {lines:?}"
        );
        let ok_line = lines.first().expect("first event present");
        assert!(
            ok_line.contains("auth ok") && ok_line.contains("token"),
            "le premier événement est `auth ok` avec le champ chemin `token` : {ok_line}"
        );
        let rejected_line = lines.get(1).expect("second event present");
        assert!(
            rejected_line.contains("auth rejected")
                && rejected_line.contains("token")
                && rejected_line.contains("MRD-AUTH-014"),
            "le second événement est `auth rejected` avec le chemin `token` et le code \
             MRD-AUTH-014 : {rejected_line}"
        );
        assert!(
            lines
                .iter()
                .all(|line| !line.contains(&issued.token) && !line.contains("mrd_inconnu")),
            "aucun événement ne porte le secret du token, une valeur de cookie ou l'id_token : \
             {lines:?}"
        );
    }

    // ——— Amendement 2026-10-19 : JWT OIDC Bearer (`src/auth/dual.sdd`) ———

    /// `Scenario` : « JWT Bearer valide authentifie sans toucher la base » — la base est une
    /// `MockDatabase` sans requête préparée : toute requête ferait échouer la chaîne.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn bearer_jwt_valid_authenticates_without_touching_db() {
        let db = sea_orm::MockDatabase::new(sea_orm::DatabaseBackend::Sqlite).into_connection();
        let state = MiryadAuthState {
            oidc_client: Arc::new(MockOidcClient::with_bearer_result(Ok(bearer_identity(
                "aaa.bbb.ccc",
            )))),
            ..state_around(db)
        };

        let app = dump_app(state);
        let req = Request::builder()
            .uri("/dump")
            .header("Authorization", "Bearer aaa.bbb.ccc")
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body, "user-1|user-1@example.com|user-one|bearer:aaa.bbb.ccc",
            "subject/email/preferred_username de l'identité vérifiée, source `Bearer {{ jwt }}` \
             avec le JWT rendu par `verify_bearer` ; aucune requête base"
        );
    }

    /// `Scenario` : « JWT Bearer invalide est définitif, sans repli cookie » — le cookie
    /// valide présent n'est jamais lu : le corps reste `MRD-AUTH-019`.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn bearer_jwt_invalid_is_definitive_no_cookie_fallback() {
        let state =
            test_state_with_oidc(MockOidcClient::with_bearer_result(Err(AuthError::InvalidBearer))).await;
        let cookie = valid_session_cookie(&state);

        let app = protected_app(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Authorization", "Bearer aaa.bbb.ccc")
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, BODY_019);
    }

    /// `Scenario` : « JWT Bearer échu rend 020 » — l'erreur de `verify_bearer` est propagée
    /// telle quelle.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn bearer_jwt_expired_returns_020() {
        let state =
            test_state_with_oidc(MockOidcClient::with_bearer_result(Err(AuthError::BearerExpired))).await;

        let app = protected_app(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Authorization", "Bearer aaa.bbb.ccc")
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, BODY_020);
    }

    /// `Scenario` : « Bearer non JWT sans db est rejeté 019 sans E/S » — le mock par défaut
    /// panique s'il est appelé : `verify_bearer` ne doit pas l'être pour `mrd_abc`.
    #[cfg(not(feature = "db"))]
    #[tokio::test]
    async fn bearer_mrd_token_without_db_is_rejected_019() {
        let state = test_state().await;

        let app = protected_app(state);
        let req = Request::builder()
            .uri("/protected")
            .header("Authorization", "Bearer mrd_abc")
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, BODY_019);
    }

    /// `Scenario` : « Bearer API token reste un chemin base sous db » — `verify_bearer` par
    /// défaut (panique s'il est appelé) : le jeton `mrd_…` ne doit jamais l'atteindre.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn bearer_api_token_stays_a_db_path_under_db() {
        let state = test_state().await;
        let issued = issue_token(&state.db, "token-user", "test", None, &state.token_pepper)
            .await
            .expect("issuing succeeds");

        let app = dump_app(state);
        let req = Request::builder()
            .uri("/dump")
            .header("Authorization", format!("Bearer {}", issued.token))
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, format!("token-user|||token:{}", issued.id));
    }

    /// `Scenario` : « cookie seul reste le chemin de session sans db » — source `Session`
    /// portant l'`id_token` du cookie.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn session_cookie_alone_yields_session_source() {
        let state = test_state().await;
        let jwt = make_jwt(future_exp());
        let cookie = cookie_pair(&build_set_cookie(
            &session_identity(jwt.clone()),
            &state.cookie_key,
            state.secure_cookies,
        ));

        let app = dump_app(state);
        let req = Request::builder()
            .uri("/dump")
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request");
        let (status, body) = serve(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            format!("session-user|session@example.com|session-name|session:{jwt}")
        );
    }

    /// `Scenario` : « `is_jwt_shaped` » — exactement deux `.` ; toute autre forme est refusée.
    #[test]
    fn is_jwt_shaped_accepts_exactly_two_dots() {
        assert!(is_jwt_shaped("a.b.c"));
    }

    #[test]
    fn is_jwt_shaped_rejects_other_dot_counts_and_plain_material() {
        for material in ["a.b", "a.b.c.d", "abc", "", "mrd_xxxx"] {
            assert!(
                !is_jwt_shaped(material),
                "`{material}` ne doit pas être JWT-shaped"
            );
        }
    }
}
