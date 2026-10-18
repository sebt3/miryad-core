//! Connexion OAuth 2.0 « device code » auprès de MAS (`src/matrix/device_code.sdd`).

use std::future::Future;
use std::time::Duration;

use rand::Rng;
use reqwest::StatusCode;
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::time::{Instant, sleep};

use crate::matrix::error::MatrixError;

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEVICE_SCOPE: &str = "urn:matrix:org.matrix.msc2967.client:device:";
const API_SCOPE: &str = "urn:matrix:org.matrix.msc2967.client:api:*";
const DEFAULT_INTERVAL_SECS: u64 = 5;
const SLOW_DOWN_STEP_SECS: u64 = 5;
const DEVICE_ID_LEN: usize = 10;

/// Invite affichée à l'opérateur pour approuver la connexion (`src/matrix/device_code.sdd`).
#[derive(Debug, Clone)]
pub struct DeviceCodePrompt {
    /// URL où l'opérateur saisit le code.
    pub verification_uri: String,
    /// URL qui contient déjà le code, si le serveur en fournit une.
    pub verification_uri_complete: Option<String>,
    /// Code saisi par l'opérateur. Ne sort que par `on_prompt`.
    pub user_code: String,
    /// Durée de validité du code, en secondes.
    pub expires_in_secs: u64,
}

/// Jetons remis par le flux device code (`src/matrix/device_code.sdd`). `Debug` ne montre aucun jeton.
pub(crate) struct DeviceCodeTokens {
    /// Identifiant de client OAuth, à persister pour ne pas se ré-enregistrer.
    pub(crate) client_id: String,
    /// Identifiant Matrix du compte, lu depuis `whoami`.
    pub(crate) user_id: String,
    /// Identifiant d'appareil, identique à celui demandé dans la portée.
    pub(crate) device_id: String,
    /// Jeton d'accès (durée de vie courte, 300 s sur MAS).
    pub(crate) access_token: String,
    /// Jeton de rafraîchissement, s'il est rendu.
    pub(crate) refresh_token: Option<String>,
    /// Durée de validité du jeton d'accès, si le serveur l'annonce.
    pub(crate) expires_in_secs: Option<u64>,
}

impl std::fmt::Debug for DeviceCodeTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceCodeTokens")
            .field("client_id", &self.client_id)
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_in_secs", &self.expires_in_secs)
            .finish()
    }
}

/// Connexion par device code (`src/matrix/device_code.sdd`, étapes 1 à 7).
///
/// `on_prompt` est appelé une seule fois, avant la première interrogation du serveur de jetons.
///
/// # Errors
///
/// Retourne [`MatrixError::Login`] si le serveur n'annonce pas le device code, si le code expire
/// ou est refusé, ou si l'identité ne correspond pas à l'appareil demandé ; [`MatrixError::Network`]
/// si le serveur reste injoignable ou en erreur jusqu'à l'expiration du code.
pub(crate) async fn login(
    homeserver_url: &str,
    display_name: &str,
    existing_client_id: Option<&str>,
    on_prompt: impl FnOnce(DeviceCodePrompt),
    http: &reqwest::Client,
) -> Result<DeviceCodeTokens, MatrixError> {
    let base = homeserver_url.trim_end_matches('/');
    let meta = discover(http, base).await?;
    let device_endpoint = meta
        .device_authorization
        .as_deref()
        .ok_or_else(|| MatrixError::Login("le serveur n'annonce pas le device code".into()))?;

    let client_id = if let Some(id) = existing_client_id {
        id.to_owned()
    } else {
        let endpoint = meta.registration.as_deref().ok_or_else(|| {
            MatrixError::Login("le serveur n'annonce pas l'enregistrement dynamique".into())
        })?;
        register(http, endpoint, display_name, base).await?
    };

    let device_id = new_device_id();
    let auth = request_device_code(http, device_endpoint, &client_id, &device_id).await?;
    on_prompt(DeviceCodePrompt {
        verification_uri: auth.verification_uri,
        verification_uri_complete: auth.verification_uri_complete,
        user_code: auth.user_code,
        expires_in_secs: auth.expires_in,
    });

    let interval = auth.interval.unwrap_or(DEFAULT_INTERVAL_SECS).max(1);
    let token_url = meta.token.as_str();
    let client_ref = client_id.as_str();
    let code = auth.device_code.as_str();
    let granted = poll_until_done(auth.expires_in, interval, || async move {
        poll_token(http, token_url, client_ref, code).await
    })
    .await?;

    let whoami_url = format!("{base}/_matrix/client/v3/account/whoami");
    let req = http.get(&whoami_url).bearer_auth(&granted.access_token);
    let me: Whoami = json_ok(send(req, &whoami_url).await?).await?;
    if me.device_id.as_deref() != Some(device_id.as_str()) {
        return Err(MatrixError::Login("appareil inattendu".into()));
    }

    Ok(DeviceCodeTokens {
        client_id,
        user_id: me.user_id,
        device_id,
        access_token: granted.access_token,
        refresh_token: granted.refresh_token,
        expires_in_secs: granted.expires_in_secs,
    })
}

/// Métadonnées de découverte (`src/matrix/device_code.sdd`, étape 1).
#[derive(Deserialize)]
struct AuthMetadata {
    #[serde(rename = "token_endpoint")]
    token: String,
    #[serde(rename = "device_authorization_endpoint")]
    device_authorization: Option<String>,
    #[serde(rename = "registration_endpoint")]
    registration: Option<String>,
}

#[derive(Deserialize)]
struct Registered {
    client_id: String,
}

/// Réponse de l'endpoint d'autorisation de périphérique (étape 4).
#[derive(Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

/// Corps de l'endpoint de jetons : erreur OAuth dans `error`, ou jetons (étape 6).
#[derive(Deserialize)]
struct TokenBody {
    error: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct Whoami {
    user_id: String,
    device_id: Option<String>,
}

/// Jetons remis par l'endpoint de jetons.
struct Granted {
    access_token: String,
    refresh_token: Option<String>,
    expires_in_secs: Option<u64>,
}

/// Issue d'une interrogation de l'endpoint de jetons, indépendante du transport
/// (`src/matrix/device_code.sdd`).
enum PollOutcome<T> {
    /// Pas encore approuvé : on réessaie après l'intervalle.
    Pending,
    /// Trop fréquent : l'intervalle s'allonge de 5 s.
    SlowDown,
    /// Code expiré côté serveur : erreur de login.
    Expired,
    /// Refusé par l'opérateur : erreur de login.
    Denied,
    /// Jetons remis.
    Done(T),
    /// Erreur réessayable (transport, 5xx, 429) : on réessaie tant que le délai n'est pas dépassé.
    Transient,
    /// Erreur non réessayable.
    Failed(MatrixError),
}

/// Boucle d'interrogation (`src/matrix/device_code.sdd`) : intervalle, `slow_down`, borne `expires_in`.
/// Indépendante du transport : `poll` fait une interrogation et rend son issue.
async fn poll_until_done<T, F, Fut>(
    expires_in_secs: u64,
    mut interval_secs: u64,
    mut poll: F,
) -> Result<T, MatrixError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = PollOutcome<T>>,
{
    let started = Instant::now();
    let budget = Duration::from_secs(expires_in_secs);
    loop {
        match poll().await {
            PollOutcome::Done(value) => return Ok(value),
            PollOutcome::Pending | PollOutcome::Transient => {}
            PollOutcome::SlowDown => interval_secs = interval_secs.saturating_add(SLOW_DOWN_STEP_SECS),
            PollOutcome::Expired => return Err(MatrixError::Login("code expiré".into())),
            PollOutcome::Denied => return Err(MatrixError::Login("refusé".into())),
            PollOutcome::Failed(e) => return Err(e),
        }
        // Attente bornée par le délai restant : on n'attend jamais au-delà de `expires_in`.
        let remaining = budget.saturating_sub(started.elapsed()).as_secs();
        if interval_secs > remaining {
            return Err(MatrixError::Login("code expiré".into()));
        }
        sleep(Duration::from_secs(interval_secs)).await;
    }
}

/// Erreur réseau réessayable : transport coupé ou délai dépassé.
fn transport() -> MatrixError {
    MatrixError::Network {
        message: "connection failed or timed out".into(),
        retryable: true,
    }
}

/// Erreur de statut HTTP : 429 et 5xx sont réessayables, le reste est un échec de login.
fn status_error(status: StatusCode) -> MatrixError {
    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        MatrixError::Network {
            message: format!("server error (HTTP {})", status.as_u16()),
            retryable: true,
        }
    } else {
        MatrixError::Login(format!("HTTP {}", status.as_u16()))
    }
}

fn invalid_response() -> MatrixError {
    MatrixError::Login("réponse invalide du serveur".into())
}

/// Envoie une requête et refuse toute redirection vers une autre origine que celle appelée.
async fn send(req: reqwest::RequestBuilder, url: &str) -> Result<reqwest::Response, MatrixError> {
    let expected = reqwest::Url::parse(url).map_err(|_| invalid_response())?;
    let resp = req.send().await.map_err(|_| transport())?;
    if resp.url().origin() != expected.origin() {
        return Err(MatrixError::Login(
            "redirection hors de l'hôte appelé refusée".into(),
        ));
    }
    Ok(resp)
}

async fn json_ok<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, MatrixError> {
    if !resp.status().is_success() {
        return Err(status_error(resp.status()));
    }
    resp.json::<T>().await.map_err(|_| invalid_response())
}

/// Corps `application/x-www-form-urlencoded`, encodé par l'analyseur d'URL déjà présent.
fn form_body(pairs: &[(&str, &str)]) -> Result<String, MatrixError> {
    let url = reqwest::Url::parse_with_params("http://localhost/", pairs).map_err(|_| invalid_response())?;
    Ok(url.query().unwrap_or_default().to_owned())
}

/// Identifiant d'appareil aléatoire de 10 lettres majuscules (étape 3).
fn new_device_id() -> String {
    let mut rng = rand::thread_rng();
    (0..DEVICE_ID_LEN).map(|_| rng.gen_range('A'..='Z')).collect()
}

async fn discover(http: &reqwest::Client, base: &str) -> Result<AuthMetadata, MatrixError> {
    let stable = format!("{base}/_matrix/client/v1/auth_metadata");
    let mut resp = send(http.get(&stable), &stable).await?;
    if resp.status() == StatusCode::NOT_FOUND {
        let unstable = format!("{base}/_matrix/client/unstable/org.matrix.msc2965/auth_metadata");
        resp = send(http.get(&unstable), &unstable).await?;
    }
    json_ok(resp).await
}

async fn register(
    http: &reqwest::Client,
    endpoint: &str,
    display_name: &str,
    homeserver: &str,
) -> Result<String, MatrixError> {
    let body = json!({
        "client_name": display_name,
        "grant_types": [DEVICE_GRANT, "refresh_token"],
        "response_types": [],
        "token_endpoint_auth_method": "none",
        "application_type": "native",
        "redirect_uris": [],
        "client_uri": homeserver,
    });
    let registered: Registered = json_ok(send(http.post(endpoint).json(&body), endpoint).await?).await?;
    Ok(registered.client_id)
}

async fn request_device_code(
    http: &reqwest::Client,
    endpoint: &str,
    client_id: &str,
    device_id: &str,
) -> Result<DeviceAuthorization, MatrixError> {
    let scope = format!("openid {API_SCOPE} {DEVICE_SCOPE}{device_id}");
    let body = form_body(&[("client_id", client_id), ("scope", scope.as_str())])?;
    let req = http
        .post(endpoint)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body);
    json_ok(send(req, endpoint).await?).await
}

/// Une interrogation de l'endpoint de jetons (étape 6) : traduit l'échec technique en issue.
async fn poll_token(
    http: &reqwest::Client,
    endpoint: &str,
    client_id: &str,
    device_code: &str,
) -> PollOutcome<Granted> {
    match poll_token_response(http, endpoint, client_id, device_code).await {
        Ok(outcome) => outcome,
        Err(e) if e.retryable() => PollOutcome::Transient,
        Err(e) => PollOutcome::Failed(e),
    }
}

/// Lit la réponse de l'endpoint de jetons. Les erreurs OAuth se lisent dans `error` du corps,
/// jamais dans le statut (MAS répond `authorization_pending` en 403).
async fn poll_token_response(
    http: &reqwest::Client,
    endpoint: &str,
    client_id: &str,
    device_code: &str,
) -> Result<PollOutcome<Granted>, MatrixError> {
    let body = form_body(&[
        ("grant_type", DEVICE_GRANT),
        ("device_code", device_code),
        ("client_id", client_id),
    ])?;
    let req = http
        .post(endpoint)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body);
    let resp = send(req, endpoint).await?;
    let status = resp.status();
    let text = resp.text().await.map_err(|_| transport())?;

    let Ok(body) = serde_json::from_str::<TokenBody>(&text) else {
        return Err(if status.is_server_error() {
            status_error(status)
        } else {
            invalid_response()
        });
    };
    if let Some(error) = body.error.as_deref() {
        return match error {
            "authorization_pending" => Ok(PollOutcome::Pending),
            "slow_down" => Ok(PollOutcome::SlowDown),
            "expired_token" => Ok(PollOutcome::Expired),
            "access_denied" => Ok(PollOutcome::Denied),
            _ if status.is_server_error() => Err(status_error(status)),
            other => Err(MatrixError::Login(format!("erreur OAuth : {other}"))),
        };
    }
    if status.is_success() {
        return match body.access_token {
            Some(access_token) => Ok(PollOutcome::Done(Granted {
                access_token,
                refresh_token: body.refresh_token,
                expires_in_secs: body.expires_in,
            })),
            None => Err(invalid_response()),
        };
    }
    Err(status_error(status))
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::io;
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
    use std::time::Duration;

    use axum::extract::State;
    use axum::http::header::AUTHORIZATION;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use axum::{Form, Json, Router};
    use serde_json::{Value, json};
    use tokio::time::Instant;

    use super::*;

    const DEVICE_PREFIX: &str = "urn:matrix:org.matrix.msc2967.client:device:";
    const USER_CODE: &str = "ABCD-1234";
    const DEVICE_CODE: &str = "DEVICE-CODE-SECRET";
    const ACCESS: &str = "ACCESS-SECRET";
    const REFRESH: &str = "REFRESH-SECRET";
    const USER_ID: &str = "@media-test-matrix-bot:example";

    // Réponses scriptées du faux endpoint de jetons.
    fn pending() -> (u16, Value) {
        (403, json!({"error": "authorization_pending"}))
    }
    fn slow_down() -> (u16, Value) {
        (400, json!({"error": "slow_down"}))
    }
    fn expired() -> (u16, Value) {
        (400, json!({"error": "expired_token"}))
    }
    fn denied() -> (u16, Value) {
        (400, json!({"error": "access_denied"}))
    }
    fn success() -> (u16, Value) {
        (
            200,
            json!({"access_token": ACCESS, "refresh_token": REFRESH, "expires_in": 300, "token_type": "Bearer"}),
        )
    }

    /// Configuration d'un faux MAS. `script` est consommé par appel ; à vide, `pending` est rendu.
    #[derive(Clone)]
    struct Setup {
        device: bool,
        registration: bool,
        v1_404: bool,
        expires_in: u64,
        script: Vec<(u16, Value)>,
    }

    impl Setup {
        fn ok(script: Vec<(u16, Value)>) -> Self {
            Self {
                device: true,
                registration: true,
                v1_404: false,
                expires_in: 1200,
                script,
            }
        }
    }

    #[derive(Default)]
    struct Mas {
        base: String,
        device: bool,
        registration: bool,
        v1_404: bool,
        expires_in: u64,
        script: VecDeque<(u16, Value)>,
        token_calls: Vec<Instant>,
        registrations: usize,
        device_client_ids: Vec<String>,
        device_id: Option<String>,
        discovery_hits: Vec<&'static str>,
    }

    impl Mas {
        fn metadata(&self) -> Value {
            let mut meta = json!({
                "issuer": format!("{}/", self.base),
                "token_endpoint": format!("{}/oauth2/token", self.base),
            });
            if self.device {
                meta["device_authorization_endpoint"] = json!(format!("{}/oauth2/device", self.base));
            }
            if self.registration {
                meta["registration_endpoint"] = json!(format!("{}/oauth2/registration", self.base));
            }
            meta
        }
    }

    type Shared = Arc<Mutex<Mas>>;

    fn lock(state: &Shared) -> MutexGuard<'_, Mas> {
        state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn v1_metadata(State(state): State<Shared>) -> Response {
        let mut mas = lock(&state);
        mas.discovery_hits.push("v1");
        if mas.v1_404 {
            return StatusCode::NOT_FOUND.into_response();
        }
        Json(mas.metadata()).into_response()
    }

    async fn unstable_metadata(State(state): State<Shared>) -> Response {
        let mut mas = lock(&state);
        mas.discovery_hits.push("unstable");
        Json(mas.metadata()).into_response()
    }

    async fn registration(State(state): State<Shared>, Json(_body): Json<Value>) -> Response {
        lock(&state).registrations += 1;
        (StatusCode::CREATED, Json(json!({"client_id": "reg-client-1"}))).into_response()
    }

    async fn device_auth(State(state): State<Shared>, Form(form): Form<HashMap<String, String>>) -> Response {
        let mut mas = lock(&state);
        mas.device_client_ids
            .push(form.get("client_id").cloned().unwrap_or_default());
        mas.device_id = form.get("scope").and_then(|scope| {
            scope
                .split_whitespace()
                .find_map(|token| token.strip_prefix(DEVICE_PREFIX))
                .map(str::to_owned)
        });
        Json(json!({
            "device_code": DEVICE_CODE,
            "user_code": USER_CODE,
            "verification_uri": format!("{}/link", mas.base),
            "expires_in": mas.expires_in,
            "interval": 5,
        }))
        .into_response()
    }

    async fn token(State(state): State<Shared>) -> Response {
        let mut mas = lock(&state);
        mas.token_calls.push(Instant::now());
        let (status, body) = mas.script.pop_front().unwrap_or_else(pending);
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(body)).into_response()
    }

    async fn whoami(State(state): State<Shared>, headers: HeaderMap) -> Response {
        let mas = lock(&state);
        let expected = format!("Bearer {ACCESS}");
        if headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) != Some(expected.as_str()) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        match &mas.device_id {
            Some(device) => Json(json!({"user_id": USER_ID, "device_id": device})).into_response(),
            None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }

    /// Faux MAS en `axum` sur port éphémère.
    struct Fake {
        base: String,
        state: Shared,
    }

    impl Fake {
        async fn start(setup: Setup) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind du faux MAS");
            let addr = listener.local_addr().expect("adresse du faux MAS");
            let base = format!("http://{addr}");
            let state: Shared = Arc::new(Mutex::new(Mas {
                base: base.clone(),
                device: setup.device,
                registration: setup.registration,
                v1_404: setup.v1_404,
                expires_in: setup.expires_in,
                script: setup.script.into(),
                ..Mas::default()
            }));
            let app = Router::new()
                .route("/_matrix/client/v1/auth_metadata", get(v1_metadata))
                .route(
                    "/_matrix/client/unstable/org.matrix.msc2965/auth_metadata",
                    get(unstable_metadata),
                )
                .route("/oauth2/registration", post(registration))
                .route("/oauth2/device", post(device_auth))
                .route("/oauth2/token", post(token))
                .route("/_matrix/client/v3/account/whoami", get(whoami))
                .with_state(Arc::clone(&state));
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Self { base, state }
        }
    }

    type Prompts = Arc<Mutex<Vec<DeviceCodePrompt>>>;

    fn new_prompts() -> Prompts {
        Arc::new(Mutex::new(Vec::new()))
    }

    async fn run(
        fake: &Fake,
        existing: Option<&str>,
        prompts: &Prompts,
    ) -> Result<DeviceCodeTokens, MatrixError> {
        let sink = Arc::clone(prompts);
        login(
            &fake.base,
            "miryad-test",
            existing,
            move |prompt| sink.lock().unwrap_or_else(PoisonError::into_inner).push(prompt),
            &reqwest::Client::new(),
        )
        .await
    }

    // Scenario: le flux nominal aboutit
    #[tokio::test(start_paused = true)]
    async fn nominal_flow_returns_tokens() {
        let fake = Fake::start(Setup::ok(vec![pending(), pending(), success()])).await;
        let prompts = new_prompts();
        let tokens = run(&fake, None, &prompts).await.expect("login nominal");

        let shown = prompts.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(shown.len(), 1, "on_prompt doit être appelé exactement une fois");
        assert_eq!(shown[0].user_code, USER_CODE);
        drop(shown);

        let mas = lock(&fake.state);
        assert_eq!(mas.token_calls.len(), 3, "deux pending puis un jeton");
        assert_eq!(tokens.user_id, USER_ID);
        assert_eq!(Some(&tokens.device_id), mas.device_id.as_ref());
        assert_eq!(tokens.access_token, ACCESS);
        assert_eq!(tokens.refresh_token.as_deref(), Some(REFRESH));
    }

    // Scenario: slow_down allonge l'intervalle
    // Porte sur la boucle seule (indépendante du transport) sous temps simulé, sans E/S réseau.
    #[tokio::test(start_paused = true)]
    async fn slow_down_extends_interval() {
        let calls: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(VecDeque::from(vec![
            PollOutcome::SlowDown,
            PollOutcome::Done(()),
        ])));
        let (rec, next) = (Arc::clone(&calls), Arc::clone(&script));
        let result = poll_until_done(1200, 5, move || {
            let (rec, next) = (Arc::clone(&rec), Arc::clone(&next));
            async move {
                rec.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(Instant::now());
                let outcome = next.lock().unwrap_or_else(PoisonError::into_inner).pop_front();
                outcome.unwrap_or(PollOutcome::Pending)
            }
        })
        .await;
        assert!(result.is_ok(), "obtenu : {result:?}");

        let calls = calls.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(calls.len(), 2, "deux interrogations attendues");
        assert_eq!(
            calls[1].duration_since(calls[0]),
            Duration::from_secs(10),
            "intervalle après slow_down"
        );
    }

    // Scenario: un code expiré ou refusé est une erreur de login
    #[tokio::test(start_paused = true)]
    async fn expired_or_denied_code_is_login_error() {
        for script in [expired(), denied()] {
            let fake = Fake::start(Setup::ok(vec![script])).await;
            let result = run(&fake, None, &new_prompts()).await;
            assert!(
                matches!(result, Err(MatrixError::Login(_))),
                "attendu Err(Login), obtenu : {result:?}"
            );
            assert_eq!(
                lock(&fake.state).token_calls.len(),
                1,
                "aucune autre interrogation"
            );
        }
    }

    // Scenario: le délai total borne l'attente
    // Porte sur la boucle seule, sous temps simulé : une interrogation toujours en attente et
    // `expires_in = 30` doivent rendre une erreur après 30 s simulées au plus.
    #[tokio::test(start_paused = true)]
    async fn total_deadline_bounds_wait() {
        let calls: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        let started = Instant::now();
        let result: Result<(), MatrixError> = poll_until_done(30, 5, move || {
            let recorded = Arc::clone(&recorded);
            async move {
                recorded
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(Instant::now());
                PollOutcome::Pending
            }
        })
        .await;
        let elapsed = started.elapsed();

        assert!(
            matches!(result, Err(MatrixError::Login(_))),
            "attendu Err(Login), obtenu : {result:?}"
        );
        assert!(
            calls.lock().unwrap_or_else(PoisonError::into_inner).len() >= 2,
            "la boucle doit avoir interrogé plusieurs fois"
        );
        assert!(
            elapsed <= Duration::from_secs(30),
            "attente simulée au-delà de expires_in : {elapsed:?}"
        );
    }

    // Scenario: la découverte retombe sur le chemin instable
    #[tokio::test(start_paused = true)]
    async fn discovery_falls_back_to_unstable_path() {
        let mut setup = Setup::ok(vec![success()]);
        setup.v1_404 = true;
        let fake = Fake::start(setup).await;
        let result = run(&fake, None, &new_prompts()).await;
        assert!(result.is_ok(), "obtenu : {result:?}");
        assert_eq!(lock(&fake.state).discovery_hits, vec!["v1", "unstable"]);
    }

    // Scenario: un serveur sans device code est refusé explicitement
    #[tokio::test(start_paused = true)]
    async fn server_without_device_code_is_refused() {
        let mut setup = Setup::ok(Vec::new());
        setup.device = false;
        let fake = Fake::start(setup).await;
        let result = run(&fake, None, &new_prompts()).await;
        match result {
            Err(MatrixError::Login(message)) => {
                assert!(message.contains("device code"), "message : {message}");
            }
            other => panic!("attendu Err(Login), obtenu : {other:?}"),
        }
        assert_eq!(lock(&fake.state).token_calls.len(), 0);
    }

    // Scenario: un client_id existant évite l'enregistrement
    #[tokio::test(start_paused = true)]
    async fn existing_client_id_skips_registration() {
        let mut setup = Setup::ok(vec![success()]);
        setup.registration = false;
        let fake = Fake::start(setup).await;
        let result = run(&fake, Some("abc"), &new_prompts()).await;
        assert!(result.is_ok(), "obtenu : {result:?}");

        let mas = lock(&fake.state);
        assert_eq!(mas.registrations, 0, "aucun enregistrement attendu");
        assert_eq!(mas.device_client_ids, vec!["abc".to_owned()]);
    }

    /// Tampon partagé du subscriber de capture.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // Scenario: aucun secret n'est journalisé
    #[tokio::test(start_paused = true)]
    async fn no_secret_is_logged() {
        let fake = Fake::start(Setup::ok(vec![pending(), success()])).await;
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);

        let result = run(&fake, None, &new_prompts()).await;
        drop(guard);
        assert!(result.is_ok(), "obtenu : {result:?}");

        let logs =
            String::from_utf8_lossy(&capture.0.lock().unwrap_or_else(PoisonError::into_inner)).into_owned();
        for secret in [DEVICE_CODE, USER_CODE, ACCESS, REFRESH] {
            assert!(!logs.contains(secret), "secret journalisé : {secret}\n{logs}");
        }
    }
}
