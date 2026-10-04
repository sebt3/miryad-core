//! Porte sortante HTTP nu vers un cluster Restate self-hosté : enregistrement idempotent du
//! déploiement auprès de l'admin API (`register_deployment`) et déclenchement asynchrone d'un run
//! via l'ingress (`trigger_run`, suffixe `/send` — jamais bloquant sur la complétion). Aucune
//! autre partie de la crate ne parle HTTP sortant à Restate : les fichiers service parlent
//! exclusivement le protocole `restate-sdk`. Compile seulement sous la feature `workflow`.

use crate::workflow::definition::DagSteps;
use crate::workflow::error::WorkflowError;
use crate::workflow::error::classify_transport_error;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

/// Corps exact de `POST /deployments` : `uri` puis `force`, compact — l'ordre des champs porte
/// sur le fil (spike ./client.sdd, `Must`), d'où un struct dédié plutôt qu'une valeur `json!`
/// (dont la Map trierait les clés alphabétiquement : `force` avant `uri`).
#[derive(Serialize)]
struct DeploymentRequest<'a> {
    uri: &'a str,
    force: bool,
}

/// Corps `2xx` observé de l'ingress sur `/send` : seul `invocationId` est consommé, `status`
/// est ignoré (./client.sdd, `Must` — lecture du corps, jamais de l'en-tête `x-restate-id`).
#[derive(Deserialize)]
struct IngressSendResponse {
    #[serde(rename = "invocationId")]
    invocation_id: String,
}

/// Emplacement des trois URL Restate d'une application consommatrice — construit par elle,
/// aucune lecture d'environnement ici (même convention que `auth::OidcConfig`).
pub struct WorkflowConfig {
    /// URL de base de l'admin API Restate (`POST {admin_url}/deployments`).
    pub admin_url: String,
    /// URL de base de l'ingress Restate (`POST {ingress_url}/DagInterpreter/{run_key}/run/send`).
    pub ingress_url: String,
    /// URL que Restate doit pouvoir rappeler pour joindre ce processus (endpoint `restate-sdk`
    /// servi par l'app) ; forme non restreinte ni validée par ce fichier.
    pub deployment_url: String,
}

impl WorkflowConfig {
    /// Vérifie les deux URL que ce fichier emprunte pour son propre compte : `admin_url` et
    /// `ingress_url` doivent chacune se parser en URL absolue de schéma `http` ou `https`.
    /// Pure, sans I/O : aucune connexion réseau n'est tentée ici — le contrôle est un verdict
    /// rendu avant tout appel (`./client.sdd`, arbitré 2026-09-29). `deployment_url` n'est ni
    /// validée ni inspectée : sa forme reste à la charge de l'application consommatrice
    /// (décision conservée du 2026-09-23).
    ///
    /// # Errors
    ///
    /// `MRD-WORKFLOW-007` (`WorkflowError::InvalidConfig`) : `admin_url` ou `ingress_url` vide,
    /// non parsable en URL absolue, ou de schéma hors `http`/`https` ; le message nomme le
    /// champ fautif.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        validate_http_url("admin_url", &self.admin_url)?;
        validate_http_url("ingress_url", &self.ingress_url)?;
        Ok(())
    }
}

/// Garde d'une seule URL de service : vide, non parsable ou de schéma autre que
/// `http`/`https` → `InvalidConfig` nommant `field`. `reqwest::Url` est le ré-export public de
/// `url` par `reqwest` (aucune dépendance ajoutée) ; le parse est syntaxique, jamais résolutif.
fn validate_http_url(field: &str, value: &str) -> Result<(), WorkflowError> {
    if value.is_empty() {
        return Err(WorkflowError::InvalidConfig(format!(
            "{field} is empty, expected an absolute http or https URL"
        )));
    }
    match reqwest::Url::parse(value) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => Ok(()),
        _ => Err(WorkflowError::InvalidConfig(format!(
            "{field} is not an absolute http or https URL: {value}"
        ))),
    }
}

/// Poignée rendue par `trigger_run` : les deux identifiants du run déclenché, à l'appelant de
/// décider lesquels il expose (signal, statut — lectures hors du périmètre de ce fichier).
pub struct RunHandle {
    /// Clé de run générée (`uuid::Uuid::new_v4`, hyphéné), nécessaire à tout appel ultérieur
    /// ciblant ce workflow.
    pub run_key: String,
    /// `invocationId` du corps de réponse de l'ingress, identifiant Restate global de
    /// l'invocation.
    pub invocation_id: String,
}

/// Enregistre (ou ré-enregistre, `force: true` explicite à chaque appel) le déploiement auprès de
/// l'admin API : `POST {admin_url}/deployments`, corps `{"uri": deployment_url, "force": true}`,
/// `content-type: application/json`. Tout statut `2xx` rend `Ok(())` sans inspecter le corps —
/// idempotent, à appeler au démarrage de chaque réplica.
///
/// La configuration est validée en tête (`WorkflowConfig::validate`), avant toute ouverture de
/// connexion.
///
/// # Errors
///
/// - `MRD-WORKFLOW-001` (`WorkflowError::RestateUnreachable`) : admin API injoignable
///   (connexion refusée, DNS, timeout).
/// - `MRD-WORKFLOW-002` (`WorkflowError::RestateRejected`) : l'admin API répond hors `2xx` —
///   `status` observé, `body` verbatim.
/// - `MRD-WORKFLOW-007` (`WorkflowError::InvalidConfig`) : `admin_url` (ou `ingress_url`) vide,
///   non parsable ou de schéma hors `http`/`https` — refusée avant tout appel réseau.
pub async fn register_deployment(
    config: &WorkflowConfig,
    http: &reqwest::Client,
) -> Result<(), WorkflowError> {
    config.validate()?;
    let response = http
        .post(format!("{}/deployments", config.admin_url))
        .json(&DeploymentRequest {
            uri: &config.deployment_url,
            force: true,
        })
        .send()
        .await
        .map_err(classify_transport_error)?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.map_err(classify_transport_error)?;
        return Err(WorkflowError::RestateRejected {
            status: status.as_u16(),
            body,
        });
    }
    Ok(())
}

/// Déclenche une exécution de `DagInterpreter` sans attendre sa complétion : clé de run neuve
/// (`uuid::Uuid::new_v4`) à chaque appel — jamais réutilisée, jamais d'`Idempotency-Key` — puis
/// `POST {ingress_url}/DagInterpreter/{run_key}/run/send`, corps = `dag` sérialisé tel quel, sans
/// enveloppe ni champ ajouté. Un `202` (tout `2xx` en pratique) rend un `RunHandle` dont
/// `invocation_id` est lu dans le **corps** de réponse, pas dans un en-tête.
///
/// La cible contractuelle est `&DagSteps` (./definition.rs, ./client.sdd `Exposes`) — rive
/// effectuée le 2026-09-23 au batch de ./definition.rs : le paramètre est le type réel de la
/// colonne `steps`, plus un générique lié par `serde::Serialize`. ./client.rs ne valide pas le
/// DAG, il le sérialise tel quel (sa validation relève de ./definition.rs).
///
/// La configuration est validée en tête (`WorkflowConfig::validate`), avant toute ouverture de
/// connexion — une `admin_url` invalide est donc refusée ici aussi, bien que seul l'`ingress_url`
/// soit emprunté par cette fonction.
///
/// # Errors
///
/// - `MRD-WORKFLOW-001` (`WorkflowError::RestateUnreachable`) : ingress injoignable.
/// - `MRD-WORKFLOW-002` (`WorkflowError::RestateRejected`) : l'ingress répond hors `2xx` (par
///   ex. service `DagInterpreter` non enregistré, `404`) — `body` verbatim.
/// - `MRD-WORKFLOW-003` (`WorkflowError::Serialization`) : corps `2xx` qui ne se décode pas en
///   la forme attendue (`invocationId` manquant par ex.).
/// - `MRD-WORKFLOW-007` (`WorkflowError::InvalidConfig`) : `admin_url` ou `ingress_url` vide,
///   non parsable ou de schéma hors `http`/`https` — refusée avant tout appel réseau.
pub async fn trigger_run(
    config: &WorkflowConfig,
    http: &reqwest::Client,
    dag: &DagSteps,
) -> Result<RunHandle, WorkflowError> {
    config.validate()?;
    let run_key = Uuid::new_v4().to_string();
    let response = http
        .post(format!(
            "{}/DagInterpreter/{}/run/send",
            config.ingress_url, run_key
        ))
        .json(dag)
        .send()
        .await
        .map_err(classify_transport_error)?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.map_err(classify_transport_error)?;
        return Err(WorkflowError::RestateRejected {
            status: status.as_u16(),
            body,
        });
    }
    let decoded: IngressSendResponse = response.json().await.map_err(classify_transport_error)?;
    Ok(RunHandle {
        run_key,
        invocation_id: decoded.invocation_id,
    })
}

#[cfg(test)]
mod tests {
    use super::RunHandle;
    use super::WorkflowConfig;
    use super::register_deployment;
    use super::trigger_run;
    use crate::workflow::definition::DagSteps;
    use crate::workflow::definition::StepDefinition;
    use crate::workflow::error::WorkflowError;
    use std::io::Read as _;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;
    use uuid::Uuid;
    use uuid::Version;

    /// `deployment_url` de fixture — une forme de `Service` Kubernetes stable, aucune validation
    /// de forme n'étant attendue de ./client.rs (./client.sdd, `Accepts`).
    const DEPLOYMENT_URL: &str = "http://miryad-app.restate-callback.svc.cluster.local:9080";

    /// Corps de réponse `202` observé au spike Restate.
    const SEND_ACCEPTED_BODY: &str = "{\"invocationId\":\"inv_test123\",\"status\":\"Accepted\"}";

    // ---------------------------------------------------------------------------
    // Fixture : un vrai @DagSteps (./definition.rs). Le `Scenario` « le corps envoyé à
    // `trigger_run` est le `DagSteps` tel quel, sans enveloppe » l'exige dans son type réel —
    // la rive de la signature (./client.sdd `Tasks`) a retiré le générique. ./client.rs ne
    // valide pas le DAG, il le sérialise seulement ; la fixture reste structurellement valide
    // (aucune arête pendante, aucun cycle) pour refléter l'`Accepts` contractuel.
    // ---------------------------------------------------------------------------

    fn dag_fixture() -> DagSteps {
        DagSteps(vec![
            StepDefinition {
                id: "deploy".to_string(),
                depends_on: vec![],
                kind: "rhai".to_string(),
                config: serde_json::json!({ "script": "1 + 1" }),
            },
            StepDefinition {
                id: "notify".to_string(),
                depends_on: vec!["deploy".to_string()],
                kind: "rhai".to_string(),
                config: serde_json::json!({ "channel": "ops" }),
            },
        ])
    }

    // ---------------------------------------------------------------------------
    // Serveur HTTP de test : purs threads + TcpListener (même sobriété que ./error.rs,
    // pas de wiremock). Capture méthode, URI, en-têtes (clé en minuscules) et corps de
    // chaque requête reçue ; répond un statut et un corps programmés, une requête par
    // connexion (`connection: close`), en boucle d'acceptation pour les suites de deux
    // appels.
    // ---------------------------------------------------------------------------

    #[derive(Clone, Debug)]
    struct CapturedRequest {
        method: String,
        uri: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl CapturedRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        }
    }

    struct StubServer {
        port: u16,
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
    }

    impl StubServer {
        fn spawn(status: u16, body: String) -> StubServer {
            let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
                panic!("impossible de binder un port libre sur 127.0.0.1");
            };
            let Ok(addr) = listener.local_addr() else {
                panic!("adresse locale indisponible");
            };
            let port = addr.port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let shared = Arc::clone(&requests);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { return };
                    let Ok(()) = stream.set_read_timeout(Some(Duration::from_secs(10))) else {
                        continue;
                    };
                    let Some(request) = read_one_request(&mut stream) else {
                        continue;
                    };
                    shared.lock().unwrap().push(request);
                    let response = format!(
                        "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    if stream.write_all(response.as_bytes()).is_err() {
                        return;
                    }
                    let _ = stream.flush();
                }
            });
            StubServer { port, requests }
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }

        /// `WorkflowConfig` pointant ce stub comme admin API **et** comme ingress ; les deux
        /// fonctions n'exercent chacune qu'un seul de ces deux champs par scenario.
        fn config(&self) -> WorkflowConfig {
            WorkflowConfig {
                admin_url: self.url(),
                ingress_url: self.url(),
                deployment_url: DEPLOYMENT_URL.to_string(),
            }
        }

        fn requests(&self) -> Vec<CapturedRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    /// Lit exactement une requête HTTP (head jusqu'à `\r\n\r\n`, corps selon
    /// `content-length`) ; `None` si le flux se coupe avant la fin de la requête.
    fn read_one_request(stream: &mut TcpStream) -> Option<CapturedRequest> {
        let mut buffer: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1024];
        let head_end = loop {
            if buffer.len() > 1024 * 1024 {
                return None;
            }
            let Ok(read) = stream.read(&mut chunk) else {
                return None;
            };
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break position;
            }
        };
        let head = std::str::from_utf8(&buffer[..head_end]).ok()?;
        let mut lines = head.split("\r\n");
        let mut request_line = lines.next()?.split(' ');
        let method = request_line.next()?.to_string();
        let uri = request_line.next()?.to_string();
        let mut headers = Vec::new();
        let mut content_length = 0usize;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let name = name.trim().to_lowercase();
            if name == "content-length" {
                content_length = value.trim().parse().ok()?;
            }
            headers.push((name, value.trim().to_string()));
        }
        let total = head_end + 4 + content_length;
        while buffer.len() < total {
            let Ok(read) = stream.read(&mut chunk) else {
                return None;
            };
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        Some(CapturedRequest {
            method,
            uri,
            headers,
            body: buffer[head_end + 4..total].to_vec(),
        })
    }

    /// Se lie à un port libre puis le relâche : toute connexion suivante est refusée — le
    /// « serveur Restate down » des scenarios (même helper que ./error.rs, modules de tests
    /// séparés donc redéclaration locale).
    fn closed_port() -> u16 {
        let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
            panic!("impossible de binder un port libre sur 127.0.0.1");
        };
        let Ok(addr) = listener.local_addr() else {
            panic!("adresse locale indisponible");
        };
        drop(listener);
        addr.port()
    }

    /// Client reqwest sortant borné par un timeout, pour qu'aucun test ne suspende la suite.
    fn test_http() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("le client reqwest de test doit se construire")
    }

    /// Extrait la clé de run d'une URI capturée sous la forme contractée
    /// `/DagInterpreter/{run_key}/run/send`, `None` sinon.
    fn captured_run_key(request: &CapturedRequest) -> Option<&str> {
        request
            .uri
            .strip_prefix("/DagInterpreter/")?
            .strip_suffix("/run/send")
    }

    // ---------------------------------------------------------------------------
    // register_deployment
    // ---------------------------------------------------------------------------

    /// Scenario « enregistrement réussi » : `200` → `Ok(())`, et la requête capturée est
    /// exactement `POST /deployments`, corps `{"uri":"<deployment_url>","force":true}` octet
    /// pour octet, `content-type: application/json`.
    #[tokio::test]
    async fn register_deployment_reussi_corps_force_true() {
        let stub = StubServer::spawn(200, "{\"deploymentId\":\"dp_ZQ6q0J\"}".to_string());
        let config = stub.config();
        let result = register_deployment(&config, &test_http()).await;
        assert!(
            matches!(result, Ok(())),
            "le 200 devait rendre Ok(()) : {result:?}"
        );

        let requests = stub.requests();
        assert_eq!(
            requests.len(),
            1,
            "une seule requête POST /deployments était attendue"
        );
        let request = requests.first().expect("la requête capturée doit exister");
        assert_eq!(request.method, "POST");
        assert_eq!(request.uri, "/deployments");
        assert_eq!(request.header("content-type"), Some("application/json"));
        let expected_body = format!("{{\"uri\":\"{DEPLOYMENT_URL}\",\"force\":true}}");
        assert_eq!(
            String::from_utf8_lossy(&request.body),
            expected_body,
            "le corps doit être exactement uri puis force, compact"
        );
        assert!(
            request.header("idempotency-key").is_none(),
            "./client.rs ne pose jamais d'Idempotency-Key"
        );
    }

    /// Scenario « le schéma changé n'est repris qu'avec force: true » : le champ `force: true`
    /// est posé explicitement à chaque appel, dans tous les corps capturés — aucune branche
    /// d'omission n'existe.
    #[tokio::test]
    async fn register_deployment_force_toujours_pose() {
        let stub = StubServer::spawn(200, "{\"deploymentId\":\"dp_1\",\"revision\":2}".to_string());
        let config = stub.config();
        let http = test_http();
        register_deployment(&config, &http)
            .await
            .expect("le premier 200 devait rendre Ok(())");
        register_deployment(&config, &http)
            .await
            .expect("le second 200 devait rendre Ok(())");

        let requests = stub.requests();
        assert_eq!(requests.len(), 2, "deux appels = deux requêtes capturées");
        for request in &requests {
            let value: serde_json::Value =
                serde_json::from_slice(&request.body).expect("le corps doit être un JSON objet");
            assert_eq!(
                value.get("force"),
                Some(&serde_json::Value::Bool(true)),
                "force doit être posé explicitement à true sur chaque POST /deployments"
            );
            assert_eq!(
                value.get("uri"),
                Some(&serde_json::Value::String(DEPLOYMENT_URL.to_string()))
            );
        }
    }

    /// Scenario « enregistrement rejeté par l'admin API » : `400` avec corps
    /// `{"message":"invalid uri"}` → `RestateRejected { status: 400, body verbatim }`.
    #[tokio::test]
    async fn register_deployment_rejet_400() {
        let stub = StubServer::spawn(400, "{\"message\":\"invalid uri\"}".to_string());
        let Err(err) = register_deployment(&stub.config(), &test_http()).await else {
            panic!("le statut 400 devait rendre un Err");
        };
        let WorkflowError::RestateRejected { status, body } = err else {
            panic!("variante attendue RestateRejected : {err:?}");
        };
        assert_eq!(status, 400);
        assert_eq!(body, "{\"message\":\"invalid uri\"}");
    }

    /// Scenario « enregistrement sur serveur injoignable » : connexion refusée sur
    /// `admin_url` → `RestateUnreachable` (`MRD-WORKFLOW-001`).
    #[tokio::test]
    async fn register_deployment_injoignable() {
        let config = WorkflowConfig {
            admin_url: format!("http://127.0.0.1:{}", closed_port()),
            ingress_url: "http://127.0.0.1:80".to_string(),
            deployment_url: DEPLOYMENT_URL.to_string(),
        };
        let Err(err) = register_deployment(&config, &test_http()).await else {
            panic!("un port refusant la connexion devait rendre un Err");
        };
        assert!(
            matches!(err, WorkflowError::RestateUnreachable(_)),
            "variante attendue RestateUnreachable : {err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("MRD-WORKFLOW-001: restate unreachable: "),
            "rendu inattendu : {err}"
        );
    }

    // ---------------------------------------------------------------------------
    // trigger_run
    // ---------------------------------------------------------------------------

    /// Scenario « déclenchement réussi génère une clé neuve et poste sur /send » : deux appels
    /// avec la même fixture produisent deux URIs `/DagInterpreter/{run_key}/run/send` aux clés
    /// distinctes (UUID v4 hyphéné), et chaque `RunHandle` porte la clé de SON appel et
    /// l'`invocationId` du CORPS de réponse — sans jamais poser d'`Idempotency-Key`.
    #[tokio::test]
    async fn trigger_run_cle_neuve_et_send() {
        let stub = StubServer::spawn(202, SEND_ACCEPTED_BODY.to_string());
        let config = stub.config();
        let http = test_http();
        let dag = dag_fixture();
        let first = trigger_run(&config, &http, &dag)
            .await
            .expect("le 202 devait rendre Ok(RunHandle)");
        let second = trigger_run(&config, &http, &dag)
            .await
            .expect("le second 202 devait rendre Ok(RunHandle)");

        let requests = stub.requests();
        assert_eq!(requests.len(), 2, "deux appels = deux requêtes capturées");
        let mut keys = Vec::new();
        for request in &requests {
            assert_eq!(request.method, "POST");
            let key = captured_run_key(request).expect(
                "l'URI doit être /DagInterpreter/{run_key}/run/send — jamais la forme bloquante sans /send",
            );
            keys.push(key.to_string());
            assert!(
                request.header("idempotency-key").is_none(),
                "./client.rs ne pose jamais d'Idempotency-Key (clé de run neuve à chaque appel)"
            );
        }
        assert_ne!(
            keys.first().expect("première clé"),
            keys.get(1).expect("seconde clé"),
            "deux appels ne doivent jamais partager la même clé"
        );
        for (handle, key) in [(first, &keys[0]), (second, &keys[1])] {
            assert_eq!(
                handle.run_key, *key,
                "le run_key rendu doit être la clé générée pour CET appel"
            );
            assert_eq!(
                handle.invocation_id, "inv_test123",
                "invocation_id doit être lu dans le CORPS de réponse (pas d'en-tête émis par le stub)"
            );
            let parsed = Uuid::parse_str(key).expect("run_key doit être un UUID v4");
            assert_eq!(&parsed.to_string(), key, "format hyphéné standard attendu");
            assert!(
                matches!(parsed.get_version(), Some(Version::Random)),
                "UUID v4 (Random) attendu"
            );
        }
    }

    /// Scenario « déclenchement sur service non enregistré » : `404` avec le corps exact
    /// observé au spike → `RestateRejected { status: 404, body verbatim }`.
    #[tokio::test]
    async fn trigger_run_service_inconnu_404() {
        let spike_body = "{\"code\":404,\"message\":\"service 'DagInterpreter' not found, \
                          make sure to register the service before calling it.\",\"source\":\"ingress\"}";
        let stub = StubServer::spawn(404, spike_body.to_string());
        let Err(err) = trigger_run(&stub.config(), &test_http(), &dag_fixture()).await else {
            panic!("le statut 404 devait rendre un Err");
        };
        let WorkflowError::RestateRejected { status, body } = err else {
            panic!("variante attendue RestateRejected : {err:?}");
        };
        assert_eq!(status, 404);
        assert_eq!(body, spike_body);
    }

    /// Scenario « déclenchement avec réponse au format inattendu » : `202` avec
    /// `{"status":"Accepted"}` seul (pas d'`invocationId`) → `Serialization`
    /// (`MRD-WORKFLOW-003`), jamais un `RunHandle` fabricqué.
    #[tokio::test]
    async fn trigger_run_reponse_sans_invocation_id() {
        let stub = StubServer::spawn(202, "{\"status\":\"Accepted\"}".to_string());
        let Err(err) = trigger_run(&stub.config(), &test_http(), &dag_fixture()).await else {
            panic!("un 202 sans invocationId devait rendre un Err");
        };
        assert!(
            matches!(err, WorkflowError::Serialization(_)),
            "variante attendue Serialization : {err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("MRD-WORKFLOW-003: could not decode restate response: "),
            "rendu inattendu : {err}"
        );
    }

    /// Scenario « le corps envoyé à `trigger_run` est le `DagSteps` tel quel, sans enveloppe » :
    /// le capturée se re-désérialise en la fixture DAG à deux steps, octet pour octet
    /// identique à sa sérialisation — rien d'ajouté (pas de `run_key`, il ne vit que dans
    /// l'URI), rien de retiré.
    #[tokio::test]
    async fn corps_envoye_sans_enveloppe() {
        let stub = StubServer::spawn(202, SEND_ACCEPTED_BODY.to_string());
        let dag = dag_fixture();
        let handle: RunHandle = trigger_run(&stub.config(), &test_http(), &dag)
            .await
            .expect("le 202 devait rendre Ok(RunHandle)");

        let requests = stub.requests();
        assert_eq!(requests.len(), 1, "une seule requête était attendue");
        let request = requests.first().expect("la requête capturée doit exister");
        assert!(
            request.uri.ends_with("/run/send"),
            "URI capturée : {}",
            request.uri
        );

        let expected_bytes = serde_json::to_vec(&dag).expect("la fixture doit être sérialisable");
        assert_eq!(
            request.body, expected_bytes,
            "le corps doit être la fixture sérialisée telle quelle, sans enveloppe ajoutée"
        );
        let round_trip: DagSteps =
            serde_json::from_slice(&request.body).expect("le corps doit se re-désérialiser en la fixture");
        assert_eq!(round_trip, dag, "aucun champ retiré");

        let value: serde_json::Value =
            serde_json::from_slice(&request.body).expect("le corps doit être un JSON");
        let steps = value
            .as_array()
            .expect("DagSteps (newtype sur Vec) se sérialise en tableau JSON nu, pas en objet enveloppe");
        assert_eq!(steps.len(), 2, "la fixture a deux steps");
        for step in steps {
            let object = step.as_object().expect("un step est un objet JSON");
            assert!(
                object.get("run_key").is_none(),
                "aucun run_key dans le corps — la clé ne vit que dans l'URI"
            );
            let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                vec!["config", "depends_on", "id", "kind"],
                "aucun champ ajouté aux quatre champs du step"
            );
        }
        assert!(
            !handle.run_key.is_empty(),
            "une clé neuve est générée et rendue, elle ne passe pas par le corps"
        );
    }

    // ---------------------------------------------------------------------------
    // Validation de WorkflowConfig (arbitré 2026-09-29)
    // ---------------------------------------------------------------------------

    /// Scenario « URL d'admin vide refusée avant tout appel réseau » : `admin_url` vide →
    /// `InvalidConfig` (`MRD-WORKFLOW-007`) nommant `admin_url`. Preuve de l'absence d'appel
    /// réseau : la seule adresse joignable de cette config (l'`ingress_url` pointant sur un
    /// listener vivant) n'enregistre **aucune** connexion, et la variante `InvalidConfig` est
    /// la seule que `validate` construit — le chemin HTTP n'a donc jamais été entré (il n'en
    /// sort que `RestateUnreachable`/`Rejected`/`Serialization`, aucun `reqwest::Error` étant
    /// impliqué ici, ./error.sdd `Raises`).
    #[tokio::test]
    async fn register_deployment_url_admin_vide_refusee_sans_appel_reseau() {
        let stub = StubServer::spawn(200, "{\"deploymentId\":\"dp_unused\"}".to_string());
        let config = WorkflowConfig {
            admin_url: String::new(),
            ingress_url: stub.url(),
            deployment_url: DEPLOYMENT_URL.to_string(),
        };
        let Err(err) = register_deployment(&config, &test_http()).await else {
            panic!("un admin_url vide devait rendre Err(InvalidConfig)");
        };
        assert!(
            matches!(err, WorkflowError::InvalidConfig(_)),
            "variante attendue InvalidConfig : {err:?}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("MRD-WORKFLOW-007: invalid workflow configuration: "),
            "rendu inattendu : {rendered}"
        );
        assert!(
            rendered.contains("admin_url"),
            "le message doit nommer le champ fautif : {rendered}"
        );
        assert!(
            stub.requests().is_empty(),
            "le listener vivant sur le port n'aurait dû voir aucune connexion"
        );
    }

    /// Scenario « URL non parsable ou de schéma inattendu refusée », second volet :
    /// `admin_url` vaut `ftp://restate:9070` et `ingress_url` est parfaitement valide (elle
    /// pointe un stub vivant) — `trigger_run` doit refuser la config **sans appel réseau**.
    /// Sans validation en tête, `trigger_run` ne lit aujourd'hui que `ingress_url` et posterait
    /// sur le stub : le test échoue dès qu'une connexion est capturée.
    #[tokio::test]
    async fn trigger_run_url_admin_ftp_refusee_sans_appel_reseau() {
        let stub = StubServer::spawn(202, SEND_ACCEPTED_BODY.to_string());
        let config = WorkflowConfig {
            admin_url: "ftp://restate:9070".to_string(),
            ingress_url: stub.url(),
            deployment_url: DEPLOYMENT_URL.to_string(),
        };
        let Err(err) = trigger_run(&config, &test_http(), &dag_fixture()).await else {
            panic!("un admin_url de schéma ftp devait rendre Err(InvalidConfig), ingress valide ou non");
        };
        assert!(
            matches!(err, WorkflowError::InvalidConfig(_)),
            "variante attendue InvalidConfig : {err:?}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("MRD-WORKFLOW-007: invalid workflow configuration: "),
            "rendu inattendu : {rendered}"
        );
        assert!(
            rendered.contains("admin_url"),
            "le message doit nommer le champ fautif : {rendered}"
        );
        assert!(
            stub.requests().is_empty(),
            "aucun appel réseau : le stub ingresseur sur le port n'aurait rien dû capturer"
        );
    }

    /// Scenario « URL non parsable ou de schéma inattendu refusée », premier volet :
    /// `WorkflowConfig::validate` appelée sur `ingress_url` = `pas une url`, puis sur
    /// `admin_url` = `ftp://restate:9070` — chacune rend `Err(InvalidConfig)` (`MRD-WORKFLOW-007`)
    /// nommant le champ fautif. Aucune de ces deux configs n'est jamais jointe (`validate` est
    /// pure, sans I/O) : les hôtes `restate-admin`/`restate-ingress` ne sont jamais résolus.
    #[test]
    fn validate_refuse_url_non_parsable_et_schema_ftp() {
        let non_parsable = WorkflowConfig {
            admin_url: "https://restate-admin:9070".to_string(),
            ingress_url: "pas une url".to_string(),
            deployment_url: DEPLOYMENT_URL.to_string(),
        };
        let Err(err) = non_parsable.validate() else {
            panic!("`pas une url` en ingress_url devait rendre Err(InvalidConfig)");
        };
        assert!(
            matches!(err, WorkflowError::InvalidConfig(_)),
            "variante attendue InvalidConfig : {err:?}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("MRD-WORKFLOW-007: invalid workflow configuration: ")
                && rendered.contains("ingress_url"),
            "le message doit porter le code et nommer ingress_url : {rendered}"
        );

        let schema_ftp = WorkflowConfig {
            admin_url: "ftp://restate:9070".to_string(),
            ingress_url: "https://restate-ingress:9071".to_string(),
            deployment_url: DEPLOYMENT_URL.to_string(),
        };
        let Err(err) = schema_ftp.validate() else {
            panic!("`ftp://restate:9070` en admin_url devait rendre Err(InvalidConfig)");
        };
        assert!(
            matches!(err, WorkflowError::InvalidConfig(_)),
            "variante attendue InvalidConfig : {err:?}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("MRD-WORKFLOW-007: invalid workflow configuration: ")
                && rendered.contains("admin_url"),
            "le message doit porter le code et nommer admin_url : {rendered}"
        );
    }

    /// Scenario « `deployment_url` n'est pas validée » : une config valide dont
    /// `deployment_url` vaut `n'importe quoi` passe `validate` (`Ok`), et le contrat existant
    /// de 2026-09-23 tient côté appel — l'URL absurde traverse la validation et va au réseau :
    /// `register_deployment` atteint le stub, dont le corps capturé porte l'uri verbatim.
    #[tokio::test]
    async fn validate_ignore_totalement_deployment_url() {
        let stub = StubServer::spawn(200, "{\"deploymentId\":\"dp_X\"}".to_string());
        let config = WorkflowConfig {
            admin_url: stub.url(),
            ingress_url: stub.url(),
            deployment_url: "n'importe quoi".to_string(),
        };
        config
            .validate()
            .expect("la forme de deployment_url reste à la charge de l'application");

        register_deployment(&config, &test_http())
            .await
            .expect("une deployment_url absurde avec une config valide passe et va au réseau");
        let requests = stub.requests();
        assert_eq!(requests.len(), 1, "la requête a bien été postée au stub");
        let request = requests.first().expect("une requête capturée");
        assert_eq!(
            String::from_utf8_lossy(&request.body),
            "{\"uri\":\"n'importe quoi\",\"force\":true}",
            "le stub reçoit l'uri absurde verbatim — ./client.rs ne la restreint pas"
        );
    }
}
