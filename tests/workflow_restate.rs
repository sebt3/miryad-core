//! Crate d'intégration rattachée à `src/workflow/interpreter.sdd` (`Tasks` : « valider par un test
//! d'intégration le protocole de fan-out avec l'implémentation réelle »). Contrairement à la
//! batterie unitaire, ces tests parlent à un vrai `restate-server` (conteneur `podman`, image
//! `docker.restate.dev/restatedev/restate:latest`, réseau hôte, ports libres) et servent le vrai
//! `DagInterpreter` + le vrai `StepDispatcher` (registre de kinds de fixture) depuis le processus
//! de test.
//!
//! Tous `#[ignore]` : ils exigent `podman` et l'image, ce que ni la batterie ni la CI n'ont.
//! Lancement explicite :
//! `cargo test --no-default-features --features workflow --test workflow_restate -- --ignored`.
//!
//! Hors périmètre (reste au spike du 2026-09-22 et à `docs/architecture.md`) : la reprise sur
//! crash du *processus service* — le serveur de test vit dans le processus du test et ne peut pas
//! être tué sans tuer le test. Le rejeu d'un step transitoire, lui, est exercé ici.

// Famille panic/unwrap/indexation tolérée dans cette crate de test : en-tête d'exemption
// équivalent à celui de `src/lib.rs`, posé d'après le `Must` de `tooling.sdd` — une crate
// d'intégration n'hérite pas des attributs de la librairie. Groupes `pedantic` et `cargo`
// restent `deny` sous `cfg(test)` : les deux `duration_suboptimal_units` mesurés ici sont
// corrigés dans le code (unités canoniques), non éteints par l'en-tête.
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
#![cfg(feature = "workflow")]

use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use miryad_core::workflow::DagInterpreter;
use miryad_core::workflow::DagSteps;
use miryad_core::workflow::MiryadWorkflowStep;
use miryad_core::workflow::StepDefinition;
use miryad_core::workflow::StepDispatcher;
use miryad_core::workflow::StepError;
use miryad_core::workflow::StepRegistry;
// `RunInfo` n'est pas encore ré-exporté à plat (tâche ./mod.sdd #26, lot B) : chemin du module
// `step`, public d'office par ./workflow/mod.rs.
use miryad_core::workflow::WorkflowConfig;
use miryad_core::workflow::recommended_options;
use miryad_core::workflow::register_deployment;
use miryad_core::workflow::step::RunInfo;
use miryad_core::workflow::trigger_run;
use restate_sdk::prelude::Endpoint;
use restate_sdk::prelude::HttpServer;
use restate_sdk::prelude::IntoServiceDefinition;
use serde_json::Value;
use serde_json::json;

const IMAGE: &str = "docker.restate.dev/restatedev/restate:latest";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("un port libre doit être obtenable")
}

/// Conteneur Restate jetable : supprimé au `Drop`, même sur échec de test.
struct RestateContainer {
    name: String,
}

impl Drop for RestateContainer {
    fn drop(&mut self) {
        let _ = Command::new("podman").args(["rm", "-f", &self.name]).output();
    }
}

/// Journal partagé des exécutions de steps (`start:<id>`, `end:<id>`), dans l'ordre observé.
type Log = Arc<Mutex<Vec<String>>>;

/// Kind `"record"` : journalise `start:<id>`, dort `sleep_ms`, journalise `end:<id>`, rend
/// `{ "id": <id>, "inputs": <inputs reçus> }`.
struct Record(Log);
/// Kind `"fail"` : échoue toujours, non retryable, message `boom:<id>`.
struct Fail;
/// Kind `"flaky"` : échoue `retryable: true` tant que son compteur est sous 3, puis réussit.
struct Flaky(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl MiryadWorkflowStep for Record {
    fn kind(&self) -> &'static str {
        "record"
    }
    async fn run(
        &self,
        _run: &RunInfo,
        config: Value,
        inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        let id = config["id"].as_str().unwrap_or_default().to_string();
        let sleep_ms = config["sleep_ms"].as_u64().unwrap_or(0);
        self.0.lock().expect("journal").push(format!("start:{id}"));
        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
        self.0.lock().expect("journal").push(format!("end:{id}"));
        Ok(json!({ "id": id, "inputs": inputs }))
    }
}

#[async_trait::async_trait]
impl MiryadWorkflowStep for Fail {
    fn kind(&self) -> &'static str {
        "fail"
    }
    async fn run(
        &self,
        _run: &RunInfo,
        config: Value,
        _inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        Err(StepError {
            message: format!("boom:{}", config["id"].as_str().unwrap_or_default()),
            retryable: false,
        })
    }
}

#[async_trait::async_trait]
impl MiryadWorkflowStep for Flaky {
    fn kind(&self) -> &'static str {
        "flaky"
    }
    async fn run(
        &self,
        _run: &RunInfo,
        _config: Value,
        _inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        if self.0.fetch_add(1, Ordering::SeqCst) < 2 {
            return Err(StepError {
                message: "transitoire".to_string(),
                retryable: true,
            });
        }
        Ok(json!({ "flaky": "ok" }))
    }
}

/// Pile de test : Restate en conteneur + endpoint applicatif en tâche tokio, déploiement
/// enregistré. `_container` garde le conteneur vivant jusqu'à la fin du test.
struct Stack {
    _container: RestateContainer,
    config: WorkflowConfig,
    http: reqwest::Client,
    log: Log,
    flaky_calls: Arc<AtomicUsize>,
}

impl Stack {
    async fn start() -> Self {
        let (ingress, admin, node, app) = (free_port(), free_port(), free_port(), free_port());
        let name = format!("miryad-restate-test-{app}");
        let started = Command::new("podman")
            .args(["run", "-d", "--name", &name, "--network=host"])
            .args([
                "-e",
                &format!("RESTATE_INGRESS__BIND_ADDRESS=127.0.0.1:{ingress}"),
            ])
            .args(["-e", &format!("RESTATE_ADMIN__BIND_ADDRESS=127.0.0.1:{admin}")])
            .args(["-e", &format!("RESTATE_BIND_ADDRESS=127.0.0.1:{node}")])
            .args([
                "-e",
                &format!("RESTATE_ADVERTISED_ADDRESS=http://127.0.0.1:{node}"),
            ])
            .arg(IMAGE)
            .output()
            .expect("podman doit être installé");
        assert!(
            started.status.success(),
            "podman run a échoué : {}",
            String::from_utf8_lossy(&started.stderr)
        );
        let container = RestateContainer { name };

        let log: Log = Arc::default();
        let flaky_calls = Arc::new(AtomicUsize::new(0));
        let mut registry = StepRegistry::new();
        registry
            .register(Record(Arc::clone(&log)))
            .and_then(|r| r.register(Fail))
            .and_then(|r| r.register(Flaky(Arc::clone(&flaky_calls))))
            .expect("kinds distincts");
        let endpoint = Endpoint::builder()
            .bind(DagInterpreter)
            .bind(
                StepDispatcher::new(registry)
                    .into_service_definition()
                    .options(recommended_options()),
            )
            .build();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", app))
            .await
            .expect("le port de l'endpoint doit se binder");
        tokio::spawn(HttpServer::new(endpoint).serve(listener));

        let config = WorkflowConfig {
            admin_url: format!("http://127.0.0.1:{admin}"),
            ingress_url: format!("http://127.0.0.1:{ingress}"),
            deployment_url: format!("http://127.0.0.1:{app}"),
        };
        let http = reqwest::Client::new();
        // Attente de disponibilité de l'admin API, puis enregistrement du déploiement.
        let deadline = Instant::now() + Duration::from_mins(1);
        loop {
            let up = http
                .get(format!("{}/health", config.admin_url))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success());
            if up {
                break;
            }
            assert!(Instant::now() < deadline, "Restate n'a pas démarré en 60 s");
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        register_deployment(&config, &http)
            .await
            .expect("l'enregistrement du déploiement doit réussir");
        Self {
            _container: container,
            config,
            http,
            log,
            flaky_calls,
        }
    }

    /// Déclenche `dag` puis attend sa complétion (`attach`) : `(statut HTTP, corps JSON)`.
    async fn run_to_completion(&self, dag: &DagSteps) -> (u16, Value) {
        let handle = trigger_run(&self.config, &self.http, dag)
            .await
            .expect("le déclenchement doit être accepté");
        let response = self
            .http
            .get(format!(
                "{}/restate/workflow/DagInterpreter/{}/attach",
                self.config.ingress_url, handle.run_key
            ))
            .timeout(Duration::from_mins(2))
            .send()
            .await
            .expect("l'attache au workflow doit répondre");
        let status = response.status().as_u16();
        let body = response.json::<Value>().await.unwrap_or(Value::Null);
        (status, body)
    }

    fn journal(&self) -> Vec<String> {
        self.log.lock().expect("journal").clone()
    }
}

fn step(id: &str, depends_on: &[&str], kind: &str, config: Value) -> StepDefinition {
    StepDefinition {
        id: id.to_string(),
        depends_on: depends_on.iter().map(ToString::to_string).collect(),
        kind: kind.to_string(),
        config,
    }
}

fn position(journal: &[String], entry: &str) -> usize {
    journal
        .iter()
        .position(|e| e == entry)
        .unwrap_or_else(|| panic!("`{entry}` absent du journal : {journal:?}"))
}

/// Scenario « fan-out / fan-in d'un losange » : A → (B, C) → D. B et C partent ensemble (leurs
/// deux `start` précèdent leurs `end`), D reçoit exactement les sorties de B et C, chaque step
/// s'exécute exactement une fois, la table finale porte les quatre sorties.
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn losange_fan_out_fan_in() {
    let stack = Stack::start().await;
    let dag = DagSteps(vec![
        step("A", &[], "record", json!({ "id": "A" })),
        step("B", &["A"], "record", json!({ "id": "B", "sleep_ms": 1000 })),
        step("C", &["A"], "record", json!({ "id": "C", "sleep_ms": 1000 })),
        step("D", &["B", "C"], "record", json!({ "id": "D" })),
    ]);
    let (status, body) = stack.run_to_completion(&dag).await;
    assert_eq!(status, 200, "corps : {body}");
    let results = body.as_object().expect("table id → sortie");
    assert_eq!(results.len(), 4, "quatre sorties attendues : {body}");
    let d_inputs = results["D"]["inputs"].as_object().expect("inputs de D");
    assert_eq!(d_inputs.len(), 2, "D ne reçoit que B et C : {body}");
    assert_eq!(d_inputs["B"]["id"], "B");
    assert_eq!(d_inputs["C"]["id"], "C");
    let journal = stack.journal();
    assert_eq!(journal.len(), 8, "chaque step exactement une fois : {journal:?}");
    let first_end = position(&journal, "end:B").min(position(&journal, "end:C"));
    assert!(
        position(&journal, "start:B") < first_end && position(&journal, "start:C") < first_end,
        "B et C doivent partir ensemble (fan-out) : {journal:?}"
    );
    assert!(
        position(&journal, "end:A") < position(&journal, "start:B"),
        "B attend A : {journal:?}"
    );
    assert!(
        position(&journal, "end:B").max(position(&journal, "end:C")) < position(&journal, "start:D"),
        "D attend B et C : {journal:?}"
    );
}

/// Scenario « premier échec de couche interrompt sans attendre les frères » : X échoue tout de
/// suite, Y (frère de la même couche) dort 20 s ; le run échoue avec l'erreur de X bien avant la
/// fin de Y (seuil 12 s : la latence de base d'un premier run est de quelques secondes).
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn premier_echec_de_couche_interrompt_sans_attendre_les_freres() {
    let stack = Stack::start().await;
    let dag = DagSteps(vec![
        step("X", &[], "fail", json!({ "id": "X" })),
        step("Y", &[], "record", json!({ "id": "Y", "sleep_ms": 20000 })),
    ]);
    let started = Instant::now();
    let (status, body) = stack.run_to_completion(&dag).await;
    assert!(
        status >= 400,
        "un échec de step doit échouer le run : {status} {body}"
    );
    assert!(
        body.to_string().contains("boom:X"),
        "erreur de X attendue : {body}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(12),
        "le run ne devait pas attendre le frère lent : {:?}",
        started.elapsed()
    );
}

/// Scenario « run rejette un DAG structurellement invalide avant tout step » : dépendance
/// pendante → erreur terminale `400` `MRD-WORKFLOW-004:`, aucun step exécuté.
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn dag_invalide_est_rejete_en_400_avant_tout_step() {
    let stack = Stack::start().await;
    let dag = DagSteps(vec![step("A", &["absent"], "record", json!({ "id": "A" }))]);
    let (status, body) = stack.run_to_completion(&dag).await;
    assert_eq!(status, 400, "corps : {body}");
    assert!(
        body.to_string().contains("MRD-WORKFLOW-004:"),
        "message MRD-WORKFLOW-004 attendu : {body}"
    );
    assert!(stack.journal().is_empty(), "aucun step ne devait s'exécuter");
}

/// Scenario « un step transitoire est rejoué sans rejouer les steps déjà terminés » : A puis F
/// (échoue deux fois `retryable`, réussit à la troisième) ; le run réussit, F est appelé trois
/// fois, A exactement une.
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn step_transitoire_rejoue_sans_rejouer_les_steps_termines() {
    let stack = Stack::start().await;
    let dag = DagSteps(vec![
        step("A", &[], "record", json!({ "id": "A" })),
        step("F", &["A"], "flaky", json!({})),
    ]);
    let (status, body) = stack.run_to_completion(&dag).await;
    assert_eq!(status, 200, "corps : {body}");
    assert_eq!(body["F"]["flaky"], "ok");
    assert_eq!(
        stack.flaky_calls.load(Ordering::SeqCst),
        3,
        "deux échecs puis un succès"
    );
    let journal = stack.journal();
    assert_eq!(
        journal.iter().filter(|e| e.as_str() == "start:A").count(),
        1,
        "A ne doit jamais être rejoué : {journal:?}"
    );
}
