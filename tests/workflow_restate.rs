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
// `MiryadDurableStep` et `StepContext` ne sont pas encore ré-exportés à plat (tâche ./mod.sdd
// #26, dernier fichier du lot) : chemin du module `durable`, public d'office par
// ./workflow/mod.rs — même précédent que `RunInfo` ci-dessous.
use miryad_core::workflow::MiryadWorkflowStep;
use miryad_core::workflow::StepDefinition;
use miryad_core::workflow::StepDispatcher;
use miryad_core::workflow::StepError;
use miryad_core::workflow::StepRegistry;
// `RunInfo` n'est pas encore ré-exporté à plat (tâche ./mod.sdd #26, lot B) : chemin du module
// `step`, public d'office par ./workflow/mod.rs.
use miryad_core::workflow::WorkflowConfig;
use miryad_core::workflow::durable::{MiryadDurableStep, StepContext};
use miryad_core::workflow::recommended_options;
use miryad_core::workflow::register_deployment;
use miryad_core::workflow::step::RunInfo;
// `SubWorkflowStep` est ré-exporté à plat depuis ./workflow/mod.rs (tranche minimale du lot #26) ;
// `DEFAULT_MAX_DEPTH`, item `pub` du module sans ré-export à plat (tâche ./mod.sdd #26 en cours),
// se lit par son chemin de module.
use miryad_core::workflow::SubWorkflowStep;
use miryad_core::workflow::subworkflow::DEFAULT_MAX_DEPTH;
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
/// Kind durable de fixture (tâche ./durable.sdd) : `sleep` brièvement via `StepContext` —
/// impossible dans une fermeture `ctx.run()`, donc preuve que le dispatcher n'enveloppe pas les
/// kinds durables — puis rend son `RunInfo`, preuve de la transmission de l'identité du run.
struct DurableSleeper;

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

#[async_trait::async_trait]
impl MiryadDurableStep for DurableSleeper {
    fn kind(&self) -> &'static str {
        "durable_sleep"
    }
    async fn run(
        &self,
        ctx: &StepContext<'_>,
        _config: Value,
        _inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        ctx.sleep(Duration::from_millis(1500)).await?;
        let run = ctx.run_info();
        Ok(json!({ "run_key": run.run_key, "step_id": run.step_id, "depth": run.depth }))
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
        Self::start_with_max_depth(DEFAULT_MAX_DEPTH).await
    }

    /// Pile complète avec le plafond de profondeur du kind `"subworkflow"` enregistré posé à
    /// `max_depth` (`DEFAULT_MAX_DEPTH` par [`Self::start`] ; `1` pour le Scenario de garde de
    /// profondeur).
    async fn start_with_max_depth(max_depth: u32) -> Self {
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
            .and_then(|r| r.register_durable(DurableSleeper))
            .and_then(|r| r.register_durable(SubWorkflowStep::new(max_depth)))
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

/// Scenario (./durable.sdd `Tasks`) « un kind durable dort durablement puis rend son `RunInfo` » :
/// prouve la branche durable de `StepDispatcher::execute` — le `StepContext::sleep` du kind de
/// fixture échouerait si le dispatcher l'enveloppait dans un `ctx.run()` (le SDK interdit tout
/// appel de contexte dans une fermeture `ctx.run`, et le `StepContext` n'y serait même pas
/// constructible) — et la transmission de `RunInfo` : `run_key` est la clé du run déclenché,
/// `step_id` celui du step, `depth` vaut `0` pour un run racine. Sans la branche durable, le kind
/// ne se résoudrait pas (le `dispatch` ordinaire rend « kind inconnu ») et l'attache échouerait.
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn un_kind_durable_dort_durablement_et_reçoit_son_run_info() {
    let stack = Stack::start().await;
    let dag = DagSteps(vec![step("D", &[], "durable_sleep", json!({}))]);
    let handle = trigger_run(&stack.config, &stack.http, &dag)
        .await
        .expect("le déclenchement doit être accepté");
    let response = stack
        .http
        .get(format!(
            "{}/restate/workflow/DagInterpreter/{}/attach",
            stack.config.ingress_url, handle.run_key
        ))
        .timeout(Duration::from_mins(2))
        .send()
        .await
        .expect("l'attache au workflow doit répondre");
    let status = response.status().as_u16();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    assert_eq!(
        status, 200,
        "le kind durable doit mener le run au bout : {status} {body}"
    );
    assert_eq!(
        body["D"]["run_key"], handle.run_key,
        "le run_key vu par le kind doit être la clé du run déclenché : {body}"
    );
    assert_eq!(body["D"]["step_id"], "D", "le step_id vu par le kind : {body}");
    assert_eq!(
        body["D"]["depth"], 0,
        "un run racine est à la profondeur 0 : {body}"
    );
}

/// Scenario (./subworkflow.sdd `Tasks` (1)) « nominal — la sortie de S contient les deux sorties
/// enfant » : parent `A` (kind `record`, l'écho de la fixture) puis `S` (`subworkflow`,
/// `depends_on: ["A"]`) dont le DAG enfant est deux steps `record` en chaîne (`c1` → `c2`). La
/// sortie de `S` est la table `{c1: sortie, c2: sortie}` de l'enfant, sans enveloppe ; la chaîne
/// interne est honorée (`c2` attend `c1`) ; les `inputs` du parent ne traversent pas (`c1`, step
/// racine de l'enfant, reçoit un `inputs` vide — ./subworkflow.sdd `Must`).
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn subworkflow_nominal_rend_les_sorties_enfant() {
    let stack = Stack::start().await;
    let dag = DagSteps(vec![
        step("A", &[], "record", json!({ "id": "A" })),
        step(
            "S",
            &["A"],
            "subworkflow",
            json!({
                "dag": [
                    step("c1", &[], "record", json!({ "id": "c1" })),
                    step("c2", &["c1"], "record", json!({ "id": "c2" })),
                ]
            }),
        ),
    ]);
    let (status, body) = stack.run_to_completion(&dag).await;
    assert_eq!(status, 200, "corps : {body}");
    let results = body.as_object().expect("table id → sortie");
    assert_eq!(results.len(), 2, "deux sorties parent : A et S : {body}");
    let sorties_enfant = body["S"].as_object().expect("S rend la table des sorties enfant");
    assert_eq!(
        sorties_enfant.len(),
        2,
        "les deux sorties enfant, sans enveloppe : {body}"
    );
    assert_eq!(body["S"]["c1"]["id"], "c1", "sortie de c1 : {body}");
    assert_eq!(body["S"]["c2"]["id"], "c2", "sortie de c2 : {body}");
    assert_eq!(
        body["S"]["c1"]["inputs"],
        json!({}),
        "les inputs du parent ne sont pas transmis à l'enfant : {body}"
    );
    let journal = stack.journal();
    assert!(
        position(&journal, "end:A") < position(&journal, "start:c1"),
        "S attend A : {journal:?}"
    );
    assert!(
        position(&journal, "end:c1") < position(&journal, "start:c2"),
        "la chaîne interne de l'enfant est honorée : {journal:?}"
    );
}

/// Scenario (./subworkflow.sdd `Tasks` (2)) « échec — un step enfant non retryable fait échouer
/// S puis le run parent, message du fils conservé » : l'enfant de `S` est un unique step `fail`
/// (`boom:F`, `retryable: false`). Pas de branche de repli : l'échec de l'enfant fait échouer le
/// parent, et le message du fils traverse `run_child_dag` verbatim jusqu'à l'attache du parent.
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn subworkflow_echec_enfant_fait_échouer_le_parent_message_conservé() {
    let stack = Stack::start().await;
    let dag = DagSteps(vec![step(
        "S",
        &[],
        "subworkflow",
        json!({ "dag": [step("F", &[], "fail", json!({ "id": "F" }))] }),
    )]);
    let (status, body) = stack.run_to_completion(&dag).await;
    assert!(
        status >= 400,
        "l'échec de l'enfant doit échouer le run parent : {status} {body}"
    );
    assert!(
        body.to_string().contains("boom:F"),
        "le message du fils doit être conservé verbatim : {body}"
    );
}

/// Scenario (./subworkflow.sdd `Tasks` (3)) « profondeur — échec MRD-WORKFLOW-008 en nombre borné
/// de niveaux, sans emballement » : `SubWorkflowStep::new(1)` et un DAG d'un seul step `S`
/// (`subworkflow`) dont l'enfant est le même DAG — un cycle traversant les définitions, invisible
/// à `validate_dag` qui ne voit que le DAG courant. Garde à l'exécution : le run racine (profondeur
/// 0) lance l'enfant (profondeur 1, accepté car `1 ≤ 1`) ; le `S` de l'enfant refuse son propre
/// enfant (`2 > 1`) **avant** de le lancer — deux niveaux de run exactement, jamais d'emballement.
/// Le message borné `depth 2 exceeds the maximum of 1` est la preuve que la récursion s'est arrêtée
/// au premier refus ; le seuil de temps écarte tout rebond répété.
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn subworkflow_garde_de_profondeur_échoue_en_niveaux_bornés() {
    let stack = Stack::start_with_max_depth(1).await;
    let dag = DagSteps(vec![step(
        "S",
        &[],
        "subworkflow",
        // L'enfant se rappelle lui-même ; le `config` du step le plus profond est désérialisé puis
        // refusé par la garde de profondeur — jamais validé, jamais lancé comme petit-enfant.
        json!({
            "dag": [step("S", &[], "subworkflow", json!({ "dag": [] }))]
        }),
    )]);
    let started = Instant::now();
    let (status, body) = stack.run_to_completion(&dag).await;
    let elapsed = started.elapsed();
    assert!(
        status >= 400,
        "le refus de profondeur doit échouer le run : {status} {body}"
    );
    assert!(
        body.to_string()
            .contains("MRD-WORKFLOW-008: sub-workflow depth 2 exceeds the maximum of 1"),
        "refus borné à depth 2 attendu (un seul niveau enfant lancé) : {body}"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "deux niveaux de run exactement, sans emballement : {elapsed:?}"
    );
}
