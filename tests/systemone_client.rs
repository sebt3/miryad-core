//! Crate d'intégration rattachée à `src/systemone/client.sdd` (`Can modify`) : exerce
//! `SystemOneClient::ask` contre un serveur HTTP local (axum, `127.0.0.1:0`, port éphémère)
//! qui joue le rôle du proxy. Aucun accès réseau externe.
//!
//! Lancement : `cargo test --no-default-features --features systemone --test systemone_client`.

// Famille panic/unwrap/indexation tolérée dans cette crate de test : en-tête d'exemption
// équivalent à celui de `src/lib.rs`, posé d'après le `Must` de `tooling.sdd` — une crate
// d'intégration n'hérite pas des attributs de la librairie. Groupes `pedantic` et `cargo`
// restent `deny` sous `cfg(test)`.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::dbg_macro,
        clippy::todo,
        clippy::unimplemented,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing,
        clippy::unwrap_in_result,
        clippy::panic_in_result_fn
    )
)]
#![cfg(feature = "systemone")]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::http::Method;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::post;
use miryad_core::systemone::SystemOneClient;
use miryad_core::systemone::SystemOneConfig;
use miryad_core::systemone::SystemOneError;
use miryad_core::systemone::SystemOneRequest;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

/// Requête reçue par le serveur local, telle qu'elle est arrivée sur le fil.
#[derive(Debug, Clone)]
struct Captured {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

/// Journal partagé des requêtes reçues par un serveur local.
#[derive(Debug, Clone, Default)]
struct Recorder {
    requests: Arc<Mutex<Vec<Captured>>>,
}

impl Recorder {
    fn record(&self, method: &Method, uri: &Uri, headers: &HeaderMap, body: &Bytes) {
        let authorization = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let captured = Captured {
            method: method.to_string(),
            path: uri.path().to_string(),
            authorization,
            body: body.to_vec(),
        };
        self.requests.lock().unwrap().push(captured);
    }

    fn all(&self) -> Vec<Captured> {
        self.requests.lock().unwrap().clone()
    }
}

/// Lance un routeur axum sur `127.0.0.1:0` et renvoie son URL de base (`http://127.0.0.1:PORT`).
async fn spawn(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{addr}")
}

/// Serveur qui enregistre chaque requête dans `recorder` et répond `status` + `body` JSON brut.
fn fixed_server(recorder: Recorder, status: StatusCode, body: &'static str) -> Router {
    Router::new().route(
        "/v1/systemone",
        post(
            move |method: Method, uri: Uri, headers: HeaderMap, payload: Bytes| {
                let recorder = recorder.clone();
                async move {
                    recorder.record(&method, &uri, &headers, &payload);
                    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
                }
            },
        ),
    )
}

/// Requête minimale : modèle `m`, état `hello`, une question `q` de type `noul`.
fn sample_request() -> SystemOneRequest {
    let mut questions = Map::new();
    questions.insert("q".to_string(), json!({"type": "noul", "instructions": "Oui ?"}));
    SystemOneRequest {
        model: "m".to_string(),
        state: json!("hello"),
        questions,
    }
}

/// Client configuré sur `base_url` avec timeout par défaut et clé optionnelle.
fn client_for(base_url: &str, api_key: Option<&str>, timeout_secs: u64) -> SystemOneClient {
    let mut config = SystemOneConfig::new(base_url);
    config.api_key = api_key.map(str::to_string);
    config.timeout_secs = timeout_secs;
    SystemOneClient::new(config).expect("configuration de test valide")
}

/// Scenario « ask envoie le corps et l'en-tête attendus ».
#[tokio::test]
async fn ask_envoie_le_corps_et_l_entete_attendus() {
    let recorder = Recorder::default();
    let server = fixed_server(
        recorder.clone(),
        StatusCode::OK,
        r#"{"model":"m","answers":{"q":{"type":"noul","noul":0.9}},"usage":{"input_tokens":3,"output_tokens":1}}"#,
    );
    let base = spawn(server).await;
    let client = client_for(&base, Some("k"), 5);

    let response = client.ask(&sample_request()).await.expect("ask doit réussir");

    let received = recorder.all();
    assert_eq!(received.len(), 1, "une seule requête attendue");
    let sent = &received[0];
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.path, "/v1/systemone");
    assert_eq!(sent.authorization.as_deref(), Some("Bearer k"));
    let body: Value = serde_json::from_slice(&sent.body).expect("corps JSON");
    assert_eq!(
        body,
        json!({
            "model": "m",
            "state": "hello",
            "questions": {"q": {"type": "noul", "instructions": "Oui ?"}}
        })
    );
    assert_eq!(response.answers["q"]["noul"], json!(0.9));
    assert_eq!(
        response.usage,
        Some(json!({"input_tokens": 3, "output_tokens": 1}))
    );
}

/// Scenario « sans clé, aucun en-tête Authorization ».
#[tokio::test]
async fn sans_cle_aucun_entete_authorization() {
    let recorder = Recorder::default();
    let server = fixed_server(recorder.clone(), StatusCode::OK, r#"{"model":"m","answers":{}}"#);
    let base = spawn(server).await;
    let client = client_for(&base, None, 5);

    client.ask(&sample_request()).await.expect("ask doit réussir");

    let received = recorder.all();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].authorization, None);
}

/// Scenario « un statut d'erreur devient Status avec le corps tronqué » : 2000 caractères
/// multi-octets, tronqués à 512 caractères (pas octets) sans panic de frontière UTF-8.
#[tokio::test]
async fn statut_422_devient_status_avec_corps_tronque_a_512_caracteres() {
    let long_body: &'static str = Box::leak("é".repeat(2000).into_boxed_str());
    let server = fixed_server(Recorder::default(), StatusCode::UNPROCESSABLE_ENTITY, long_body);
    let base = spawn(server).await;
    let client = client_for(&base, Some("k"), 5);

    let err = client
        .ask(&sample_request())
        .await
        .expect_err("un 422 doit être une erreur");

    match &err {
        SystemOneError::Status { status, body } => {
            assert_eq!(*status, 422);
            assert_eq!(body.chars().count(), 512);
            assert!(body.chars().all(|c| c == 'é'));
        }
        other => panic!("Status attendu, reçu {other:?}"),
    }
    assert!(!err.retryable());
}

/// Tâche « 401 et 500 » de `client.sdd` : statuts d'erreur ordinaires, corps conservé.
#[tokio::test]
async fn statuts_401_et_500_deviennent_status() {
    for code in [401_u16, 500] {
        let status = StatusCode::from_u16(code).unwrap();
        let server = fixed_server(Recorder::default(), status, "refus");
        let base = spawn(server).await;
        let client = client_for(&base, Some("k"), 5);

        match client.ask(&sample_request()).await {
            Err(SystemOneError::Status { status, body }) => {
                assert_eq!(status, code);
                assert_eq!(body, "refus");
            }
            other => panic!("Status {code} attendu, reçu {other:?}"),
        }
    }
}

/// Scenario « 429 et 529 sont retryables » : le serveur répond `429`, puis `529`.
#[tokio::test]
async fn statuts_429_et_529_sont_retryables() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let router = Router::new().route(
        "/v1/systemone",
        post(move || {
            let counter = counter.clone();
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let status = if n == 0 {
                    StatusCode::TOO_MANY_REQUESTS
                } else {
                    StatusCode::from_u16(529).unwrap()
                };
                (status, "surcharge").into_response()
            }
        }),
    );
    let base = spawn(router).await;
    let client = client_for(&base, Some("k"), 5);

    let first = client.ask(&sample_request()).await.expect_err("429 doit échouer");
    let second = client.ask(&sample_request()).await.expect_err("529 doit échouer");

    match (&first, &second) {
        (SystemOneError::Status { status: 429, .. }, SystemOneError::Status { status: 529, .. }) => {}
        other => panic!("Status 429 puis 529 attendus, reçu {other:?}"),
    }
    assert!(first.retryable());
    assert!(second.retryable());
}

/// Scenario « une réponse 2xx illisible est `InvalidResponse` » : `{"answers": 3}`.
#[tokio::test]
async fn reponse_2xx_illisible_est_invalid_response() {
    let server = fixed_server(Recorder::default(), StatusCode::OK, r#"{"answers": 3}"#);
    let base = spawn(server).await;
    let client = client_for(&base, Some("k"), 5);

    let err = client
        .ask(&sample_request())
        .await
        .expect_err("une réponse illisible doit échouer");

    assert!(matches!(err, SystemOneError::InvalidResponse(_)), "reçu {err:?}");
    assert!(!err.retryable());
}

/// Scenario « une redirection n'est pas suivie » : `307` vers un second listener local ; aucune
/// requête ne doit arriver sur ce second listener.
#[tokio::test]
async fn redirection_n_est_pas_suivie() {
    let elsewhere = Recorder::default();
    let other_base = spawn(fixed_server(
        elsewhere.clone(),
        StatusCode::OK,
        r#"{"model":"m","answers":{}}"#,
    ))
    .await;
    let location = format!("{other_base}/v1/systemone");
    let redirector = Router::new().route(
        "/v1/systemone",
        post(move || {
            let location = location.clone();
            async move {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(header::LOCATION, location)],
                    "",
                )
                    .into_response()
            }
        }),
    );
    let base = spawn(redirector).await;
    let client = client_for(&base, Some("k"), 5);

    let err = client
        .ask(&sample_request())
        .await
        .expect_err("une redirection ne doit pas être suivie");

    assert!(
        matches!(err, SystemOneError::Status { status: 307, .. }),
        "reçu {err:?}"
    );
    assert!(
        elsewhere.all().is_empty(),
        "aucune requête ne doit partir vers l'autre hôte"
    );
}

/// Scenario « un délai dépassé est une erreur de transport retryable » : le serveur dort
/// 3 s, le client a `timeout_secs = 1`.
#[tokio::test]
async fn delai_depasse_est_transport_retryable() {
    let router = Router::new().route(
        "/v1/systemone",
        post(|| async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            (StatusCode::OK, r#"{"model":"m","answers":{}}"#).into_response()
        }),
    );
    let base = spawn(router).await;
    let client = client_for(&base, Some("k"), 1);

    let err = client
        .ask(&sample_request())
        .await
        .expect_err("le délai doit être dépassé");

    assert!(matches!(err, SystemOneError::Transport(_)), "reçu {err:?}");
    assert!(err.retryable());
}
