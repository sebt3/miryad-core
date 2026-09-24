//! Taxonomie d'erreur du moteur de workflow : échecs de communication avec le cluster Restate
//! (admin API et ingress), de validation de DAG et de configuration au démarrage, sous codes `MRD-WORKFLOW-NNN`. Compile
//! seulement sous la feature `workflow`.

use thiserror::Error;

/// Erreur interne de la crate pour tout le module `workflow` : communication avec le cluster
/// Restate self-hosté (`admin_url`, `ingress_url`) et validation structurelle des DAG.
///
/// `reqwest::Error` embarqué interdisant `Clone` et `PartialEq`, le type ne derive que `Debug`
/// et `Error`. jamais de `From<reqwest::Error>` générique : la classification passe uniquement
/// par `classify_transport_error`, point de vérité unique et testable.
#[derive(Debug, Error)]
pub enum WorkflowError {
    /// `MRD-WORKFLOW-001` — `RestateUnreachable` : échec de transport vers l'admin API ou
    /// l'ingress (connexion refusée, timeout, construction de requête), rendu par
    /// `classify_transport_error`.
    #[error("MRD-WORKFLOW-001: restate unreachable: {0}")]
    RestateUnreachable(reqwest::Error),
    /// `MRD-WORKFLOW-002` — `RestateRejected` : réponse HTTP reçue hors `2xx`, construite à la
    /// main par `./client.rs` après lecture explicite du statut ; `body` verbatim, `status` le
    /// code numérique observé.
    #[error("MRD-WORKFLOW-002: restate rejected request (status {status}): {body}")]
    RestateRejected {
        /// Code numérique observé sur la réponse hors `2xx`.
        status: u16,
        /// Corps texte brut de la réponse, verbatim (sans troncature ni rédaction).
        body: String,
    },
    /// `MRD-WORKFLOW-003` — `Serialization` : la réponse de Restate ne se décode pas dans le
    /// type attendu (`is_decode`/`is_body`), rendu par `classify_transport_error`.
    #[error("MRD-WORKFLOW-003: could not decode restate response: {0}")]
    Serialization(reqwest::Error),
    /// `MRD-WORKFLOW-004` — `InvalidDag` : DAG structurellement invalide (`id` dupliqué,
    /// `depends_on` vers un step absent, cycle, liste de steps vide), rendu exclusivement par
    /// `definition::validate_dag` (./definition.sdd) en texte libre identifiant la cause.
    #[error("MRD-WORKFLOW-004: {0}")]
    InvalidDag(String),
    /// `MRD-WORKFLOW-005` — `PolicyAlreadySet` : second appel à `definition::configure_policy`
    /// (erreur de configuration de l'app au démarrage) ; la première politique reste effective.
    #[error("MRD-WORKFLOW-005: workflow policy already configured")]
    PolicyAlreadySet,
    /// `MRD-WORKFLOW-006` — `DuplicateStepKind` : `StepRegistry::register` d'un `kind()` déjà
    /// enregistré ; le premier kind reste celui du registre.
    #[error("MRD-WORKFLOW-006: step kind already registered: {0}")]
    DuplicateStepKind(String),
}

/// Range un `reqwest::Error` nu dans l'une des deux catégories transport : `is_decode()` ou
/// `is_body()` devient `Serialization`, tout le reste devient `RestateUnreachable`. Ne construit
/// jamais `RestateRejected` et ne retourne jamais de `Result` — la fonction ne peut pas échouer.
pub(crate) fn classify_transport_error(err: reqwest::Error) -> WorkflowError {
    if err.is_decode() || err.is_body() {
        WorkflowError::Serialization(err)
    } else {
        WorkflowError::RestateUnreachable(err)
    }
}

#[cfg(test)]
mod tests {
    use super::WorkflowError;
    use super::classify_transport_error;
    use std::io::Read as _;
    use std::io::Write as _;
    use std::time::Duration;

    /// Se lie à un port libre de la boucle locale puis le relâche : toute connexion suivante est
    /// refusée — le « serveur Restate down » des scenarios, sans serveur du tout.
    fn closed_port() -> u16 {
        let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0") else {
            panic!("impossible de binder un port libre sur 127.0.0.1");
        };
        let Ok(addr) = listener.local_addr() else {
            panic!("adresse locale indisponible");
        };
        drop(listener);
        addr.port()
    }

    /// Client reqwest sortant borné par un timeout, pour qu'aucun test ne puisse suspendre la
    /// suite si la boucle locale se comportait mal.
    fn test_client() -> reqwest::Client {
        let Ok(client) = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
        else {
            panic!("le client reqwest de test doit se construire");
        };
        client
    }

    /// Mini-serveur HTTP sur thread dédié : répond `200` `application/json` avec un corps qui
    /// n'est pas du JSON valide. Reproduit exactement la condition observable du scenario
    /// « Serialization sur réponse au format inattendu » sans dépendance de test supplémentaire.
    fn spawn_garbage_json_server() -> u16 {
        let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0") else {
            panic!("impossible de binder un port libre sur 127.0.0.1");
        };
        let Ok(addr) = listener.local_addr() else {
            panic!("adresse locale indisponible");
        };
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let Ok(read) = stream.read(&mut chunk) else {
                    return;
                };
                if read == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..read]);
                if buf.windows(4).position(|w| w == b"\r\n\r\n").is_some() {
                    break;
                }
            }
            let body: &[u8] = b"not-json!!\n";
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            let _ = stream.write_all(body);
            let _ = stream.flush();
        });
        addr.port()
    }

    /// Scenario « Unreachable sur échec de connexion » : une erreur `.send()` dont
    /// `is_connect()` vaut `true` est rangée en `RestateUnreachable` (`MRD-WORKFLOW-001`).
    #[tokio::test]
    async fn unreachable_on_connect_failure() {
        let url = format!("http://127.0.0.1:{}/", closed_port());
        let Err(err) = test_client().get(&url).send().await else {
            panic!("la connexion vers un port refusant devait échouer");
        };
        assert!(
            err.is_connect(),
            "la erreur devait être un échec de connexion : {err}"
        );
        let classified = classify_transport_error(err);
        assert!(matches!(classified, WorkflowError::RestateUnreachable(_)));
        assert!(
            classified
                .to_string()
                .starts_with("MRD-WORKFLOW-001: restate unreachable: "),
            "rendu inattendu : {classified}"
        );
    }

    /// Scenario « Rejected sur statut non-2xx de l'admin API » — partie garantie par error.rs :
    /// la variante construite à la main par ./client.rs (status 400, corps verbatim) rend exactement
    /// le `@Display` contracté. L'émission sur réponse reçue s'exercera au batch de ./client.rs.
    #[test]
    fn rejected_display_admin_non_2xx() {
        let err = WorkflowError::RestateRejected {
            status: 400,
            body: "{\"message\":\"bad deployment uri\"}".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "MRD-WORKFLOW-002: restate rejected request (status 400): {\"message\":\"bad deployment uri\"}"
        );
    }

    /// Scenario « Rejected sur statut non-2xx de l'ingress » — corps observé au spike Restate,
    /// rendu verbatim. Même restriction de périmètre qu'au test précédent.
    #[test]
    fn rejected_display_ingress_non_2xx() {
        let body = "{\"code\":404,\"message\":\"bad path: /UnknownWorkflow/x/run\",\"source\":\"ingress\"}";
        let err = WorkflowError::RestateRejected {
            status: 404,
            body: body.to_string(),
        };
        assert_eq!(
            err.to_string(),
            format!("MRD-WORKFLOW-002: restate rejected request (status 404): {body}")
        );
    }

    /// Scenario « Serialization sur réponse au format inattendu » : une erreur de décodage JSON
    /// (`is_decode()`) est rangée en `Serialization` (`MRD-WORKFLOW-003`).
    #[tokio::test]
    async fn serialization_on_unexpected_format() {
        let url = format!("http://127.0.0.1:{}/", spawn_garbage_json_server());
        let response = match test_client().get(&url).send().await {
            Ok(response) => response,
            Err(err) => panic!("la réponse HTTP devait arriver : {err}"),
        };
        let Err(err) = response.json::<serde_json::Value>().await else {
            panic!("le corps n'était pas un JSON valide, le décodage devait échouer");
        };
        assert!(
            err.is_decode(),
            "l'erreur devait être un échec de décodage : {err}"
        );
        let classified = classify_transport_error(err);
        assert!(matches!(classified, WorkflowError::Serialization(_)));
        assert!(
            classified
                .to_string()
                .starts_with("MRD-WORKFLOW-003: could not decode restate response: "),
            "rendu inattendu : {classified}"
        );
    }

    /// Scenario « `classify_transport_error` ne construit jamais Rejected » : sur une erreur de
    /// connexion, une erreur de construction de requête et une erreur de décodage, jamais la
    /// variante `RestateRejected` ne sort de la classification.
    #[tokio::test]
    async fn classify_never_returns_rejected() {
        let closed_url = format!("http://127.0.0.1:{}/", closed_port());
        let garbage_url = format!("http://127.0.0.1:{}/", spawn_garbage_json_server());

        let Err(connect_err) = test_client().get(&closed_url).send().await else {
            panic!("la connexion vers un port refusant devait échouer");
        };
        let Err(builder_err) = test_client().get("http://example.invalid:port80/").send().await else {
            panic!("une URL invalide devait échouer à la construction de la requête");
        };
        let garbage_response = match test_client().get(&garbage_url).send().await {
            Ok(response) => response,
            Err(err) => panic!("la réponse HTTP devait arriver : {err}"),
        };
        let Err(decode_err) = garbage_response.json::<serde_json::Value>().await else {
            panic!("le corps n'était pas un JSON valide, le décodage devait échouer");
        };

        assert!(
            connect_err.is_connect(),
            "erreur de connexion attendue : {connect_err}"
        );
        assert!(
            builder_err.is_builder(),
            "erreur de construction de requête attendue : {builder_err}"
        );
        assert!(
            decode_err.is_decode(),
            "erreur de décodage attendue : {decode_err}"
        );
        for err in [connect_err, builder_err, decode_err] {
            let classified = classify_transport_error(err);
            assert!(
                !matches!(classified, WorkflowError::RestateRejected { .. }),
                "classify_transport_error a construit la variante Rejected : {classified}"
            );
        }
    }

    /// Scenario « `PolicyAlreadySet` et `DuplicateStepKind` rendent leurs codes » : construction
    /// directe des deux variantes de configuration au démarrage, `Display` verbatim.
    #[test]
    fn variantes_de_configuration_rendent_leurs_codes() {
        assert_eq!(
            WorkflowError::PolicyAlreadySet.to_string(),
            "MRD-WORKFLOW-005: workflow policy already configured"
        );
        assert_eq!(
            WorkflowError::DuplicateStepKind("rhai".to_string()).to_string(),
            "MRD-WORKFLOW-006: step kind already registered: rhai"
        );
    }

    /// Scenario « `InvalidDag` rend le code MRD-WORKFLOW-004 quelle que soit la cause
    /// structurelle » — partie garantie par error.rs : construction directe de la variante,
    /// message distinct lisible par cause, rendu `MRD-WORKFLOW-004: {0}`. Les quatre causes
    /// réelles (`id` dupliqué, `depends_on` absent, cycle, DAG vide) exercées par
    /// `definition::validate_dag` s'exerceront au batch de ./definition.rs.
    #[test]
    fn invalid_dag_display_each_cause() {
        let causes = [
            "id de step dupliqué : deploy",
            "depends_on vers un id de step absent : notify",
            "cycle détecté : a -> b -> a",
            "DAG vide : aucun step à exécuter",
        ];
        for cause in causes {
            let err = WorkflowError::InvalidDag(cause.to_string());
            let rendered = err.to_string();
            assert!(
                rendered.starts_with("MRD-WORKFLOW-004: "),
                "rendu inattendu : {rendered}"
            );
            assert!(
                rendered.contains(cause),
                "la cause doit figurer dans le message : {rendered}"
            );
        }
    }
}
