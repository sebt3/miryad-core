//! Authentification OIDC + session cookie + tokens API.
//!
//! Point d'entrée : [`MiryadAuthState`](crate::auth::MiryadAuthState) et [`auth_router`](crate::auth::auth_router).
//! Dual-auth (cookie ou `Authorization: Bearer`) via [`AuthPrincipal`](crate::auth::AuthPrincipal).

/// Configuration `OIDC` fournie par l'application consommatrice — struct `OidcConfig` de huit
/// champs sans constructeur, validée seulement à la construction du client dans le module `oidc`.
pub mod config;

/// Cookie de session `miryad_session` : pose chiffrée `AES-256-GCM`, lecture vérifiée avec
/// ré-lecture de l'expiration, retrait par `Max-Age=0`. Le cookie pending `miryad_oidc_pending`
/// est posé et lu par ce fichier, pas par celui-là.
pub mod cookie;

/// Extracteur dual-auth de `AuthPrincipal` : `Bearer` token API résolu d'abord, cookie de
/// session en repli, aucun repli silencieux sur un `Bearer` reconnu.
pub mod dual;

/// `AuthError` : codes uniques `MRD-AUTH-NNN`, mapping variante → statut `HTTP` et rendu de
/// corps `axum` (`text/plain`).
pub mod error;

/// Extracteur `axum` `AuthUser` : identité extraite du seul cookie de session, rejet `401` porté
/// `MRD-AUTH-001`/`MRD-AUTH-002`, sans `RBAC` ni token API.
pub mod middleware;

/// Client du handshake `OIDC` : construction par discovery, génération de l'URL d'autorisation
/// (`CSRF`/nonce/`PKCE`), échange de code avec vérification des claims de l'`id_token`.
pub mod oidc;

/// Types de données `AuthPrincipal` et `PrincipalSource` : identité unifiée d'une requête
/// authentifiée, session ou token API.
pub mod principal;

/// `MiryadAuthState`, état minimal d'auth composé dans l'état `axum` de l'application
/// consommatrice (pattern `FromRef`, aucune structure d'état concrète imposée).
pub mod state;

/// Moteur des tokens API : secret `mrd_`, empreinte `HMAC-SHA256` poivrée hex, émission,
/// validation, révocation, garantie idempotente.
pub mod token;

pub use config::OidcConfig;
pub use error::AuthError;
pub use middleware::AuthUser;
pub use oidc::{OidcClient, OidcClientTrait, OidcIdentity};
pub use principal::{AuthPrincipal, PrincipalSource};
pub use state::MiryadAuthState;
pub use token::{ApiToken, IssuedToken, ensure_token, issue_token, revoke_token, validate_token};

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

/// Sous-routeur `/auth/login`, `/auth/callback`, `/auth/logout` — montable dans n'importe quel
/// `Router<S>` de l'app consommatrice tant que `MiryadAuthState: FromRef<S>` (pattern axum
/// standard pour les sous-états de bibliothèque, pas d'`AppState` concret imposé par
/// miryad-core). Préfixe `/auth` figé dans le crate (feature 6) — élimine par construction la
/// collision avec les routes SPA du frontend, plutôt que de compter sur l'app pour l'appliquer
/// elle-même.
pub fn auth_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>,
{
    Router::new().nest(
        "/auth",
        Router::new()
            .route("/login", axum::routing::get(handler_login))
            .route("/callback", axum::routing::get(handler_callback))
            .route("/logout", axum::routing::get(handler_logout)),
    )
}

#[derive(Deserialize)]
struct CallbackParams {
    code: String,
    state: String,
}

async fn handler_login(State(auth): State<MiryadAuthState>) -> Result<impl IntoResponse, AuthError> {
    // `authorization_url` rend un quadruplet (`oidc.sdd`, PKCE `S256` arbitré 2026-09-27) et les
    // trois secrets doivent survivre jusqu'au callback : la valeur scellée est
    // `csrf:nonce:verifier` (le verifier est du base64url, sans `:`), lue par le helper partagé
    // de ./cookie.rs — le verifier d'un pending à moins de trois segments est perdu, ce qui rend
    // l'échange impossible et le pending donc malformé (`MRD-AUTH-012`).
    let (url, csrf_token, nonce, pkce_verifier) = auth.oidc_client.authorization_url();

    let pending_value = format!(
        "{}:{}:{}",
        csrf_token.secret(),
        nonce.secret(),
        pkce_verifier.secret()
    );
    let mut jar = CookieJar::new();
    let mut private_jar = jar.private_mut(&auth.cookie_key);
    private_jar.add(Cookie::new(PENDING_COOKIE_NAME, pending_value));
    let encrypted_value = jar.get(PENDING_COOKIE_NAME).map_or("", Cookie::value);
    // `Secure` conditionnel (arbitré 2026-09-29, `mod.sdd` `Must`/`Returns`) : mêmes règles que
    // le cookie de session — inséré après `HttpOnly` selon `MiryadAuthState::secure_cookies`,
    // `SameSite=Lax` conservé (le retour de l'IdP est cross-site).
    let secure_attr = if auth.secure_cookies { "; Secure" } else { "" };
    let set_cookie_pending = format!(
        "{PENDING_COOKIE_NAME}={encrypted_value}; HttpOnly{secure_attr}; SameSite=Lax; Path=/; Max-Age=300"
    );

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
    // Parsing factorisé (arbitré 2026-09-27, `Must`/`Tasks`) : le même helper que
    // `cookie::extract_session`, paramétré par le nom et la clé — plus de découpage `;`/`=`
    // ici. Ses deux motifs (nom introuvable, scellé indéchiffrable) tombent sous le seul code
    // `MRD-AUTH-012`, comme le décide `Raises` : trois branches de pending, un seul code.
    let cookie_header = headers.get("Cookie").and_then(|value| value.to_str().ok());
    let value = crate::auth::cookie::find_sealed_cookie(cookie_header, PENDING_COOKIE_NAME, &auth.cookie_key)
        .map_err(|_| AuthError::InvalidCallback)?;

    // Contrat symétrique de `handler_login` : `csrf:nonce:pkce_verifier`. Un pending à moins de
    // trois segments ne restitue pas le verifier du couple, c'est un pending malformé (`012`).
    let mut pieces = value.splitn(3, ':');
    let (Some(expected_csrf), Some(nonce_part), Some(verifier_part)) =
        (pieces.next(), pieces.next(), pieces.next())
    else {
        return Err(AuthError::InvalidCallback);
    };
    let nonce = openidconnect::Nonce::new(nonce_part.to_string());
    let pkce_verifier = openidconnect::PkceCodeVerifier::new(verifier_part.to_string());

    if params.state != expected_csrf {
        tracing::warn!("MRD-AUTH-013: CSRF state mismatch");
        return Err(AuthError::CsrfMismatch);
    }

    let login_result = auth
        .oidc_client
        .exchange_code(&params.code, &nonce, &pkce_verifier)
        .await?;
    let identity = login_result.identity;

    let user = crate::users::resolve_user(&auth.db, &identity.subject, identity.email.as_deref()).await?;
    crate::users::sync_group_memberships(&auth.db, user.id, &login_result.groups).await?;

    let set_cookie_main =
        crate::auth::cookie::build_set_cookie(&identity, &auth.cookie_key, auth.secure_cookies);
    // La purge porte les mêmes attributs que la pose (arbitré 2026-09-29, `mod.sdd` `Must`) :
    // `Secure` inséré après `HttpOnly` selon `MiryadAuthState::secure_cookies`, sinon le
    // navigateur ne la substitue pas à la valeur posée en `Secure`.
    let secure_attr = if auth.secure_cookies { "; Secure" } else { "" };
    let set_cookie_clear_pending =
        format!("{PENDING_COOKIE_NAME}=; HttpOnly{secure_attr}; SameSite=Lax; Path=/; Max-Age=0");

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
        .header(SET_COOKIE, crate::auth::cookie::clear_cookie(auth.secure_cookies))
        .body(axum::body::Body::empty())
        .map_err(|e| AuthError::Oidc(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::cookie::SESSION_COOKIE_NAME;
    use crate::auth::oidc::OidcLoginResult;
    use crate::users::{group, membership, user};
    use axum::{
        body::Body,
        http::{HeaderMap, Request, StatusCode, request::Builder},
    };
    use sea_orm::{DatabaseBackend, DatabaseConnection, MockDatabase};
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    // ——— Fixtures ———

    /// Base `MockDatabase` volontairement non préparée : la première requête qui y touche rend une
    /// `DbErr` (donc un `500` `MRD-AUTH-016` rendu par ./error.rs). Un `400` ou un `302` obtenu
    /// sur cette base est donc la preuve que le handler n'a rien lu.
    fn unprepared_db() -> DatabaseConnection {
        MockDatabase::new(DatabaseBackend::Sqlite).into_connection()
    }

    /// État de test : `post_login_redirect` distinct de `post_logout_redirect` pour que le
    /// `Location` prouve le champ lu.
    fn state_on(client: MockOidcClient, db: DatabaseConnection, secure_cookies: bool) -> MiryadAuthState {
        MiryadAuthState {
            oidc_client: Arc::new(client),
            cookie_key: ::cookie::Key::from(&[0u8; 64]),
            post_login_redirect: "/login-done".to_string(),
            post_logout_redirect: "/".to_string(),
            db,
            secure_cookies,
            token_pepper: String::new(),
        }
    }

    /// État des parcours qui n'exercent que le flow cookie/OIDC : mock `OIDC` en mode panique à
    /// l'échange et base non préparée.
    fn test_state() -> MiryadAuthState {
        state_on(MockOidcClient::default(), unprepared_db(), false)
    }

    fn app_on(state: MiryadAuthState) -> Router {
        auth_router::<MiryadAuthState>().with_state(state)
    }

    fn make_app() -> Router {
        app_on(test_state())
    }

    // ——— Lecture des réponses ———

    /// Réponse servie, corps déjà lu : les assertions des `Scenario` portent sur le statut, les
    /// en-têtes et le corps rendu par ./error.rs.
    struct Served {
        status: StatusCode,
        headers: HeaderMap,
        body: String,
    }

    impl Served {
        fn location(&self) -> &str {
            self.headers
                .get("Location")
                .expect("un parcours de redirection porte `Location`")
                .to_str()
                .expect("ascii dans `Location`")
        }

        /// Toutes les lignes `Set-Cookie` émises, dans l'ordre des en-têtes.
        fn set_cookies(&self) -> Vec<String> {
            self.headers
                .get_all(SET_COOKIE)
                .iter()
                .map(|value| value.to_str().expect("ascii dans `Set-Cookie`").to_string())
                .collect()
        }

        /// `Must not` : sur chemin d'erreur, aucune en-tête `Set-Cookie` ne sort.
        fn assert_no_set_cookie(&self) {
            assert!(
                self.headers.get_all(SET_COOKIE).iter().next().is_none(),
                "aucun `Set-Cookie` sur un chemin d'erreur : {:?}",
                self.set_cookies()
            );
        }
    }

    async fn served(builder: Builder, app: Router) -> Served {
        let resp = app
            .oneshot(builder.body(Body::empty()).expect("requête valide"))
            .await
            .expect("le routeur ne rend jamais d'erreur de service");
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("corps lisible");
        Served {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).to_string(),
        }
    }

    // ——— Cookies du parcours ———

    /// Sceau `clear` sous le nom du pending avec `key` (même mécanisme que `handler_login`) et rend
    /// la ligne d'en-tête `Cookie` correspondante.
    fn pending_header(clear: &str, key: &::cookie::Key) -> String {
        let mut jar = CookieJar::new();
        jar.private_mut(key)
            .add(Cookie::new(PENDING_COOKIE_NAME, clear.to_string()));
        format!(
            "{PENDING_COOKIE_NAME}={}",
            jar.get(PENDING_COOKIE_NAME)
                .expect("pending scellé présent dans le delta du jar")
                .value()
        )
    }

    /// Paire `nom=valeur` isolée d'une ligne `Set-Cookie` (avant le premier `;`).
    fn cookie_pair(set_cookie: &str) -> String {
        set_cookie
            .split(';')
            .next()
            .expect("une ligne `Set-Cookie` porte une paire `nom=valeur`")
            .to_string()
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("horloge post-epoch")
            .as_secs()
    }

    /// `JWT` tri-segment dont la claim `exp` vaut `exp` (base64 url-safe sans bourrage), lu par
    /// `cookie::build_set_cookie` pour son `Max-Age`.
    fn make_jwt(exp: u64) -> String {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("header.{payload}.sig")
    }

    // ——— Capture des traces (patron des tests de ./oidc.rs et ./dual.rs) ———

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

    // ——— Les quinze `Scenario` de `mod.sdd` ———

    /// `Scenario` : « login redirige vers le fournisseur et pose le pending chiffré » — durci
    /// 2026-09-27 : attributs littéraux complets du `Set-Cookie`, aller-retour du format du
    /// pending par le helper partagé, et absence de cookie de session comme de purge.
    #[tokio::test]
    async fn login_redirects_and_sets_pending_cookie() {
        let got = served(Request::builder().uri("/auth/login"), make_app()).await;

        assert_eq!(got.status, StatusCode::FOUND);
        assert_eq!(got.location(), "https://issuer.example.com/authorize");
        assert!(got.body.is_empty(), "corps vide : {:#?}", got.body);

        let cookies = got.set_cookies();
        assert_eq!(cookies.len(), 1, "une seule ligne `Set-Cookie` : {cookies:?}");
        let pending = cookies.first().expect("une ligne de pending");
        assert!(
            pending.starts_with(&format!("{PENDING_COOKIE_NAME}=")),
            "le nom posé est le pending : {pending}"
        );
        assert!(
            pending.ends_with("; HttpOnly; SameSite=Lax; Path=/; Max-Age=300"),
            "attributs littéraux du pending : {pending}"
        );
        assert!(
            !pending.contains("; Secure"),
            "`secure_cookies: false` (le défaut de cette fixture) : aucun `Secure` sur le pending, \
             chaîne d'attributs exactement `HttpOnly; SameSite=Lax; Path=/; Max-Age=300` : {pending}"
        );
        assert!(
            !pending.contains(SESSION_COOKIE_NAME),
            "login ne pose jamais la session : {pending}"
        );
        assert!(!pending.contains("Max-Age=0"), "login ne purge rien : {pending}");

        // Le contrat symétrique du couple login/callback : ce que `handler_login` pose se relit
        // par le helper partagé de ./cookie.rs et porte les trois segments attendus.
        let clear = crate::auth::cookie::find_sealed_cookie(
            Some(&cookie_pair(pending)),
            PENDING_COOKIE_NAME,
            &::cookie::Key::from(&[0u8; 64]),
        )
        .expect("le pending posé par login se relit sous la clé de l'état");
        assert_eq!(
            clear.split(':').count(),
            3,
            "le pending porte `csrf:nonce:pkce_verifier` : {clear}"
        );
    }

    /// Deuxième test du `Scenario` « login redirige vers le fournisseur et pose le pending
    /// chiffré » (un test par valeur, arbitré 2026-09-29) : sous `secure_cookies: true`, le
    /// `; Secure` est inséré après `HttpOnly`, `SameSite=Lax` conservé.
    #[tokio::test]
    async fn login_redirects_and_sets_pending_cookie_with_secure_attribute() {
        let app = app_on(state_on(MockOidcClient::default(), unprepared_db(), true));

        let got = served(Request::builder().uri("/auth/login"), app).await;

        assert_eq!(got.status, StatusCode::FOUND);
        assert_eq!(got.location(), "https://issuer.example.com/authorize");

        let cookies = got.set_cookies();
        assert_eq!(cookies.len(), 1, "une seule ligne `Set-Cookie` : {cookies:?}");
        let pending = cookies.first().expect("une ligne de pending");
        assert!(
            pending.starts_with(&format!("{PENDING_COOKIE_NAME}=")),
            "le nom posé est le pending : {pending}"
        );
        assert!(
            pending.ends_with("; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=300"),
            "attributs littéraux du pending sous `secure_cookies: true` : `Secure` après \
             `HttpOnly`, `SameSite=Lax` conservé : {pending}"
        );
        assert!(
            !pending.contains(SESSION_COOKIE_NAME),
            "login ne pose jamais la session : {pending}"
        );
        assert!(!pending.contains("Max-Age=0"), "login ne purge rien : {pending}");
    }

    /// `Scenario` : « callback sans en-tête Cookie est rejeté avant la base » — `400` et plus le
    /// `502` d'un `AuthError::Oidc` portant `012` en charge utile.
    #[tokio::test]
    async fn callback_without_pending_cookie_is_rejected() {
        let got = served(
            Request::builder().uri("/auth/callback?code=x&state=y"),
            make_app(),
        )
        .await;

        assert_eq!(got.status, StatusCode::BAD_REQUEST);
        assert!(got.body.contains("MRD-AUTH-012"), "corps rendu : {}", got.body);
        assert!(
            !got.body.contains("MRD-AUTH-016"),
            "la base non préparée n'a pas été approchée : {}",
            got.body
        );
        got.assert_no_set_cookie();
    }

    /// `Scenario` : « pending absent d'un en-tête Cookie d'autres noms » — la recherche est
    /// exacte sur le nom `miryad_oidc_pending`.
    #[tokio::test]
    async fn callback_pending_absent_from_other_cookie_names() {
        let got = served(
            Request::builder()
                .uri("/auth/callback?code=x&state=y")
                .header("Cookie", "miryad_session=zzz; autre=valeur"),
            make_app(),
        )
        .await;

        assert_eq!(got.status, StatusCode::BAD_REQUEST);
        assert!(got.body.contains("MRD-AUTH-012"), "corps rendu : {}", got.body);
        got.assert_no_set_cookie();
    }

    /// `Scenario` : « pending chiffré sous une autre clé est indéchiffrable » — `decrypt` rend
    /// `None`, la cause n'est pas distinguable par le client ; le `state` pourtant cohérent ne
    /// sauve rien, le déchiffrement précède la comparaison.
    #[tokio::test]
    async fn callback_pending_under_foreign_key_is_undecryptable() {
        let header = pending_header("csrf-ok:nonce-ok:verifier-ok", &::cookie::Key::from(&[7u8; 64]));
        let got = served(
            Request::builder()
                .uri("/auth/callback?code=valide&state=csrf-ok")
                .header("Cookie", header),
            make_app(),
        )
        .await;

        assert_eq!(got.status, StatusCode::BAD_REQUEST);
        assert!(got.body.contains("MRD-AUTH-012"), "corps rendu : {}", got.body);
        assert!(
            !got.body.contains("MRD-AUTH-013"),
            "sans pending lisible, il n'y a pas de CSRF à comparer : {}",
            got.body
        );
        got.assert_no_set_cookie();
    }

    /// `Scenario` : « valeur déchiffrée sans séparateur est malformée » — le tiers manquant est le
    /// verifier du couple, le pending est rejeté sous `MRD-AUTH-012`.
    #[tokio::test]
    async fn callback_pending_without_separator_is_malformed() {
        let header = pending_header("csrfSansSeparateur", &::cookie::Key::from(&[0u8; 64]));
        let got = served(
            Request::builder()
                .uri("/auth/callback?code=valide&state=csrfSansSeparateur")
                .header("Cookie", header),
            make_app(),
        )
        .await;

        assert_eq!(got.status, StatusCode::BAD_REQUEST);
        assert!(got.body.contains("MRD-AUTH-012"), "corps rendu : {}", got.body);
        got.assert_no_set_cookie();
    }

    /// `Scenario` : « CSRF divergent rejette sans toucher le fournisseur ni purger le pending » —
    /// durci 2026-09-27 : `400` de la variante `CsrfMismatch` (plus le `502` d'un `Oidc` à charge
    /// utile), la trace `warn` au littéral exact, et `exchange_code` jamais exercé (le mock en mode
    /// panique ferait échouer le test).
    #[tokio::test]
    async fn callback_with_csrf_mismatch_is_rejected() {
        let (_guard, captured) = capture_traces();
        let app = make_app();

        let login = served(Request::builder().uri("/auth/login"), app.clone()).await;
        let pending = login.set_cookies();
        let pair = cookie_pair(pending.first().expect("pending posé par login"));

        let got = served(
            Request::builder()
                .uri("/auth/callback?code=irrelevant&state=pas-le-bon")
                .header("Cookie", pair),
            app,
        )
        .await;

        assert_eq!(got.status, StatusCode::BAD_REQUEST);
        assert!(got.body.contains("MRD-AUTH-013"), "corps rendu : {}", got.body);
        assert!(
            !got.body.contains("MRD-AUTH-012"),
            "le pending est bien lisible, ce n'est pas un `012` : {}",
            got.body
        );
        got.assert_no_set_cookie();
        assert_eq!(
            captured.lines(),
            vec!["message=MRD-AUTH-013: CSRF state mismatch".to_string()],
            "seule trace du parcours, au littéral contractuel, sans contenu d'état ni de token"
        );
    }

    /// `Scenario` : « callback sans paramètre code est un 400 de désérialisation » — rejet du
    /// extracteur `Query`, avant l'entrée dans le handler : aucun code `MRD-AUTH-`, aucun cookie.
    #[tokio::test]
    async fn callback_without_code_param_is_query_rejection() {
        let got = served(Request::builder().uri("/auth/callback?state=abc"), make_app()).await;

        assert_eq!(got.status, StatusCode::BAD_REQUEST);
        assert!(
            got.body.starts_with("Failed to deserialize query string"),
            "rejet `Query` d'axum : {}",
            got.body
        );
        assert!(
            !got.body.contains("MRD-AUTH-"),
            "le handler n'a pas été appelé : {}",
            got.body
        );
        got.assert_no_set_cookie();
    }

    /// `Scenario` : « paramètre vide et inconnu traversent la désérialisation » — chaîne vide
    /// acceptée, param `from` ignoré, le flux entre dans le handler et retombe sur le pending
    /// absent.
    #[tokio::test]
    async fn callback_empty_and_unknown_params_reach_handler() {
        let got = served(
            Request::builder().uri("/auth/callback?code=&state=&from=spam"),
            make_app(),
        )
        .await;

        assert_eq!(got.status, StatusCode::BAD_REQUEST);
        assert!(
            !got.body.starts_with("Failed to deserialize query string"),
            "la désérialisation est passée : {}",
            got.body
        );
        assert!(got.body.contains("MRD-AUTH-012"), "corps rendu : {}", got.body);
        got.assert_no_set_cookie();
    }

    /// `Scenario` : « logout sans session redirige et purge toujours » — attributs complets de la
    /// ligne de purge, sans `Secure` (`secure_cookies: false`), base non approchée.
    #[tokio::test]
    async fn logout_clears_cookie_and_redirects() {
        let got = served(Request::builder().uri("/auth/logout"), make_app()).await;

        assert_eq!(got.status, StatusCode::FOUND);
        assert_eq!(got.location(), "/");
        assert!(got.body.is_empty(), "corps vide");
        assert_eq!(
            got.set_cookies(),
            vec!["miryad_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0".to_string()],
            "purge de la session aux attributs complets, sans `Secure` sous `secure_cookies: false"
        );
    }

    /// `Scenario` : « verbe non monté sous /auth renvoie 405 avec Allow » sur les trois routes.
    /// `Allow` = `GET,HEAD` : contrat aligné sur la réalité amont (`get()` seul ajoute `HEAD`,
    /// axum 0.8.9), arbitré par Sébastien le 2026-09-28 (`./mod.sdd`, même posture que la
    /// découverte OIDC).
    #[tokio::test]
    async fn unmounted_method_under_auth_is_405_with_allow() {
        for uri in ["/auth/login", "/auth/callback", "/auth/logout"] {
            let got = served(Request::builder().method("POST").uri(uri), make_app()).await;

            assert_eq!(got.status, StatusCode::METHOD_NOT_ALLOWED, "POST {uri}");
            assert_eq!(
                got.headers
                    .get("Allow")
                    .unwrap_or_else(|| panic!("`Allow` posé par le `MethodRouter` sur POST {uri}"))
                    .to_str()
                    .expect("ascii dans `Allow`"),
                "GET,HEAD",
                "`get()` seul monté sur {uri} : axum ajoute `HEAD` à l'`Allow`"
            );
        }
    }

    /// `Scenario` : « le préfixe nu n'est pas une route » — `nest` d'un `Router` n'enregistre que
    /// les trois routes préfixées, sans préfixe nu ni catch-all : `404` hors fallback.
    #[tokio::test]
    async fn bare_auth_prefix_is_not_a_route() {
        for uri in ["/auth", "/auth/inconnu"] {
            let got = served(Request::builder().uri(uri), make_app()).await;

            assert_eq!(got.status, StatusCode::NOT_FOUND, "GET {uri}");
            got.assert_no_set_cookie();
        }
    }

    /// Parcours « callback succès » partagé par les deux tests du `Scenario`, paramétré par
    /// `secure_cookies` (un test par valeur, arbitré 2026-09-29) : `MockOidcClient` en mode
    /// configurable, `MockDatabase` préparée pour `resolve_user`, `ensure_group` et la lecture des
    /// appartenances, pending au bon CSRF. Rend la réponse servie, les traces captées et l'`exp`
    /// du JWT de session.
    async fn serve_callback_success(secure: bool) -> (Served, Vec<String>, u64) {
        let exp = now_secs() + 3600;
        let login_result = OidcLoginResult {
            identity: OidcIdentity {
                id_token: make_jwt(exp),
                subject: "user-1".to_string(),
                email: Some("user-1@example.com".to_string()),
                preferred_username: Some("alice".to_string()),
            },
            groups: vec!["admin".to_string()],
        };
        // Trois lectures, zéro écriture : l'utilisateur existe, le groupe `admin` existe,
        // l'appartenance est déjà à jour (`sync_group_memberships` ne deleting ni insérant rien).
        let db = MockDatabase::new(DatabaseBackend::Sqlite)
            .append_query_results([[user::Model {
                id: 1,
                subject: "user-1".to_string(),
                email: Some("user-1@example.com".to_string()),
                display_name: None,
                created_at: chrono::Utc::now(),
            }]])
            .append_query_results([[group::Model {
                id: 10,
                name: "admin".to_string(),
                created_at: chrono::Utc::now(),
            }]])
            .append_query_results([[membership::Model {
                id: 100,
                user_id: 1,
                group_id: 10,
            }]])
            .into_connection();

        let app = app_on(state_on(
            MockOidcClient::with_login_result(login_result),
            db,
            secure,
        ));
        let header = pending_header("csrf-ok:nonce-ok:verifier-ok", &::cookie::Key::from(&[0u8; 64]));

        let (_guard, captured) = capture_traces();
        let got = served(
            Request::builder()
                .uri("/auth/callback?code=valide&state=csrf-ok")
                .header("Cookie", header),
            app,
        )
        .await;
        (got, captured.lines(), exp)
    }

    /// `Scenario` : « callback succès pose la session puis purge le pending » — variante
    /// `secure_cookies: true` (arbitré 2026-09-29) : la purge du pending porte les mêmes
    /// attributs que la pose, donc le `; Secure` inséré après `HttpOnly`. Deux `Set-Cookie`
    /// ordonnés, trace `info` avec `subject`.
    #[tokio::test]
    async fn callback_success_sets_session_then_purges_pending() {
        let (got, lines, exp) = serve_callback_success(true).await;

        assert_eq!(got.status, StatusCode::FOUND);
        assert_eq!(
            got.location(),
            "/login-done",
            "le `post_login_redirect` de l'état"
        );
        assert!(got.body.is_empty(), "corps vide");

        let cookies = got.set_cookies();
        assert_eq!(cookies.len(), 2, "session puis purge du pending : {cookies:?}");
        let session = cookies.first().expect("première ligne : la session");
        let purge = cookies.last().expect("seconde ligne : la purge du pending");

        let pair = cookie_pair(session);
        assert!(
            pair.starts_with(&format!("{SESSION_COOKIE_NAME}=")),
            "la session est posée en premier : {pair}"
        );
        let (head, max_age_raw) = session
            .rsplit_once("; Max-Age=")
            .expect("`Max-Age` terminal séparé par `; `");
        let max_age: u64 = max_age_raw
            .parse()
            .expect("`Max-Age` numérique terminal, sans `;` finale");
        let expected = exp - now_secs();
        assert!(
            (expected.saturating_sub(1)..=expected).contains(&max_age),
            "Max-Age {max_age} aligné sur l'`exp` du jeton ({expected})"
        );
        assert_eq!(
            head,
            format!("{pair}; HttpOnly; Secure; SameSite=Strict; Path=/"),
            "`Secure` posé par `state.secure_cookies: true`, attributs dans l'ordre contractuel"
        );
        assert_eq!(
            purge,
            &format!("{PENDING_COOKIE_NAME}=; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=0"),
            "purge du pending aux mêmes attributs que la pose sous `secure_cookies: true` : \
             `Secure` après `HttpOnly`, `SameSite=Lax` conservé"
        );

        // Sur-assertion `lines.len() == 1` levée le 2026-10-03 (`../users/membership.sdd`,
        // arbitré option A) : le contrat ne verrouille que la présence de la trace `info` du
        // succès portant le champ `subject`, pas le compte des lignes capturées.
        assert!(
            lines.iter().any(|line| line.contains("subject=user-1")),
            "le succès émet une trace `info` portant le champ `subject` : {lines:?}"
        );
    }

    /// `Scenario` : « callback succès pose la session puis purge le pending » — variante
    /// `secure_cookies: false` (un test par valeur, arbitré 2026-09-29) : purge sans `Secure`,
    /// `SameSite=Lax` conservé ; la session non plus ne porte pas `Secure`.
    #[tokio::test]
    async fn callback_success_sans_secure_cookies_purge_pending_sans_secure() {
        let (got, lines, _exp) = serve_callback_success(false).await;

        assert_eq!(got.status, StatusCode::FOUND);
        assert_eq!(got.location(), "/login-done");
        assert!(got.body.is_empty(), "corps vide");

        let cookies = got.set_cookies();
        assert_eq!(cookies.len(), 2, "session puis purge du pending : {cookies:?}");
        let session = cookies.first().expect("première ligne : la session");
        let purge = cookies.last().expect("seconde ligne : la purge du pending");

        assert!(
            cookie_pair(session).starts_with(&format!("{SESSION_COOKIE_NAME}=")),
            "la session est posée en premier : {session}"
        );
        assert!(
            !session.contains("; Secure"),
            "`secure_cookies: false` : aucune attribution `Secure` sur la session : {session}"
        );
        assert_eq!(
            purge,
            &format!("{PENDING_COOKIE_NAME}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"),
            "purge littérale du pending sans `Secure` sous `secure_cookies: false`"
        );

        // Sur-assertion `lines.len() == 1` levée le 2026-10-03 (`../users/membership.sdd`,
        // arbitré option A) : le contrat ne verrouille que la présence de la trace `info` du
        // succès portant le champ `subject`, pas le compte des lignes capturées.
        assert!(
            lines.iter().any(|line| line.contains("subject=user-1")),
            "le succès émet une trace `info` portant le champ `subject` : {lines:?}"
        );
    }

    /// `Scenario` : « toute la surface publique du module résout sans raccourci racine » — test de
    /// compilation (patron `witness` de ./lib.rs) : les vingt chemins de l'`Exposes` et du
    /// `Scenario` sont référencés par le chemin `auth::…`, aucun symbole importé directement sous
    /// `miryad_core`. L'absence d'équivalent à la racine est le contrat de ./src/lib.rs (aucun
    /// `pub use` racine), tenu à la lecture de ce fichier.
    #[test]
    fn public_surface_paths_resolve_without_root_shortcuts() {
        fn witness_flat(
            _: Option<OidcConfig>,
            _: Option<AuthError>,
            _: Option<AuthUser>,
            _: Option<OidcClient>,
            _: Option<Box<dyn OidcClientTrait>>,
            _: Option<OidcIdentity>,
            _: Option<AuthPrincipal>,
        ) {
        }
        fn witness_child_paths(
            _: Option<PrincipalSource>,
            _: Option<MiryadAuthState>,
            _: Option<ApiToken>,
            _: Option<IssuedToken>,
            _: Option<crate::auth::oidc::OidcLoginResult>,
            _: Option<crate::auth::token::Model>,
            _: Option<crate::auth::token::Column>,
        ) {
        }
        fn resolves_function_paths<FnIssue, FnRevoke, FnValidate, FnEnsure, FnClear>(
            _: (FnIssue, FnRevoke, FnValidate, FnEnsure, FnClear),
        ) {
        }
        witness_flat(None, None, None, None, None, None, None);
        witness_child_paths(None, None, None, None, None, None, None);

        // Les cinq fonctions promises en mise à plat résolvent par le chemin plat (passées en
        // position de paramètre générique, jamais appelées : aucun appel ne part donc en base).
        resolves_function_paths((
            crate::auth::issue_token,
            crate::auth::revoke_token,
            crate::auth::validate_token,
            crate::auth::ensure_token,
            crate::auth::cookie::clear_cookie,
        ));
        assert_eq!(crate::auth::cookie::SESSION_COOKIE_NAME, "miryad_session");

        // Les deux chemins du mock — plat et sous-module — résolvent tous deux sous `cfg(test)`.
        assert_eq!(
            std::any::type_name::<MockOidcClient>(),
            std::any::type_name::<crate::auth::oidc::MockOidcClient>(),
            "`auth::MockOidcClient` et `auth::oidc::MockOidcClient` sont le même item"
        );
    }

    /// `Scenario` : « un état composé de l'app monte le routeur sans `AppState` imposé » — test de
    /// compilation d'abord : `merge` ne type le routeur que sur `AppState`, `MiryadAuthState`
    /// n'apparaissant jamais comme état du routeur (la projection `FromRef` opère à la requête,
    /// axum-core 0.5.6). Le `Then` runtime est le `302` vers le `post_logout_redirect` embarqué.
    #[tokio::test]
    async fn composed_app_state_mounts_router_via_from_ref() {
        #[derive(Clone)]
        struct AppState {
            auth: MiryadAuthState,
        }
        impl FromRef<AppState> for MiryadAuthState {
            fn from_ref(input: &AppState) -> Self {
                input.auth.clone()
            }
        }

        let app = Router::<AppState>::new()
            .merge(auth_router::<AppState>())
            .with_state(AppState { auth: test_state() });

        let got = served(Request::builder().uri("/auth/logout"), app).await;

        assert_eq!(got.status, StatusCode::FOUND);
        assert_eq!(got.location(), "/");
    }

    /// `Scenario` : « le module compile sans aucune feature activée » — part inline : ni le
    /// routeur ni le parcours navigateur ne portent de `#[cfg(feature = ...)]`, ce test est donc
    /// compilé et servi sur `--no-default-features` comme sur `--all-features`. La preuve de la
    /// matrice complète (huit combinaisons, `-D warnings`) est hors fichier, dans `tooling.sdd`.
    #[tokio::test]
    async fn auth_surface_is_feature_free() {
        assert!(
            std::any::type_name::<MiryadAuthState>().starts_with("miryad_core::auth::"),
            "le chemin plat résout sous toute combinaison de features"
        );
        let got = served(Request::builder().uri("/auth/login"), make_app()).await;
        assert_eq!(got.status, StatusCode::FOUND);
        assert_eq!(got.location(), "https://issuer.example.com/authorize");
    }
}
