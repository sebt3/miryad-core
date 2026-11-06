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
//! Reprise sur crash du *processus service* : exercée depuis le 2026-11-06 (preuve de reprise de
//! `src/workflow/subagent_step.sdd`, DSD `lymaille#AC-10`) par la méthode du spike de
//! `workflow#AC-8` : `reprise_apres_kill_du_service_sur_step_subagent` ne sert plus dans le
//! processus du test mais relance le binaire lui-même en *mode service* (`mode_service_restate`,
//! activé par la variable d'environnement `MIRYAD_RESTATE_SERVE_PORT`), le tue d'un SIGKILL en
//! cours de step, puis le recharge sur le même port — seul le journal Restate et le fichier de
//! journal de la fixture (la « session ») survivent. Les autres tests servent toujours dans le
//! processus, qui ne peut pas y être tué sans tuer le test ; le rejeu d'un step transitoire, lui,
//! est exercé dans tous les cas ici.

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
use miryad_core::workflow::MiryadDurableStep;
use miryad_core::workflow::MiryadWorkflowStep;
use miryad_core::workflow::RunInfo;
use miryad_core::workflow::StepContext;
use miryad_core::workflow::StepDefinition;
use miryad_core::workflow::StepDispatcher;
use miryad_core::workflow::StepError;
use miryad_core::workflow::StepRegistry;
use miryad_core::workflow::SubWorkflowStep;
use miryad_core::workflow::WorkflowConfig;
// `DEFAULT_MAX_DEPTH`, item `pub` du module `subworkflow`, n'a pas de ré-export à plat dans
// ./workflow/mod.rs : l'`Exposes` de ./mod.sdd compte vingt-quatre chemins plats et le seul
// concernant `subworkflow` est `SubWorkflowStep`. Il se lit par son chemin de module, public
// d'office par la déclaration `pub mod subworkflow` de ./workflow/mod.rs.
use miryad_core::workflow::recommended_options;
use miryad_core::workflow::register_deployment;
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

/// Conteneur `restate-server` jetable : trois ports libres sur le réseau hôte, nom dérivé du
/// port applicatif rendu. Le port `app` retourné n'est **pas** binder par cette fonction : c'est
/// le destinataire qui le lie — l'endpoint en processus pour `Stack`, le sous-processus service
/// pour la preuve de reprise (`mode_service_restate`). Extraite de `Stack::start_with_options`
/// le 2026-11-06 pour que le test de reprise (tâche `subagent_step.sdd`, DSD `lymaille#AC-10`)
/// partage le conteneur sans partager l'endpoint.
fn lancer_conteneur() -> (RestateContainer, u16, u16, u16) {
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
    (RestateContainer { name }, ingress, admin, app)
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

/// Kind durable de fixture `"durable_config"` (tâche ./dispatcher.sdd, rendu de la branche
/// durable) : rend sa `config` telle que reçue — déjà rendue par le dispatcher dans un `ctx.run()`
/// dédié — preuve que la branche durable applique le rendu de template.
struct DurableConfig;

#[async_trait::async_trait]
impl MiryadDurableStep for DurableConfig {
    fn kind(&self) -> &'static str {
        "durable_config"
    }
    async fn run(
        &self,
        _ctx: &StepContext<'_>,
        config: Value,
        _inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        Ok(config)
    }
}

/// Kind ordinaire de fixture `"echo_trigger"` (tâche ./subworkflow.sdd T6b, 2026-11-02, lot 0
/// lymaille) : rend son `RunInfo.trigger` (`json!(run.trigger)`), écho nu de l'identité du
/// déclencheur du run qui l'exécute. Kind **ordinaire** : dans le DAG enfant d'un subworkflow,
/// c'est le `DagInterpreter` enfant qui le dispatche, donc l'écho prouve la traversée complète
/// `run_child_dag` → `RunInput.trigger` → `StepInvocation` → `RunInfo` (./durable.rs T6a).
struct EchoTrigger;

#[async_trait::async_trait]
impl MiryadWorkflowStep for EchoTrigger {
    fn kind(&self) -> &'static str {
        "echo_trigger"
    }
    async fn run(
        &self,
        run: &RunInfo,
        _config: Value,
        _inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        Ok(json!(run.trigger))
    }
}

/// Kind ordinaire de fixture `"subagent"` (Scenario DEC-29 de ./dispatcher.sdd) : journalise
/// `start:<id>`/`end:<id>` comme `record` et rend son `id`. Le **nom** du kind est imposé par
/// `LONG_RUNNING_KINDS` (./dispatcher.rs) — c'est lui que le routage `handler_name` envoie vers
/// le handler `execute_long`. Simple step à journal, sans dépendance : la preuve ici porte sur
/// le handler servi, pas sur la logique du futur `SubagentStep` (`./subagent_step.sdd`, R15).
struct Subagent(Log);

#[async_trait::async_trait]
impl MiryadWorkflowStep for Subagent {
    fn kind(&self) -> &'static str {
        "subagent"
    }
    async fn run(
        &self,
        _run: &RunInfo,
        config: Value,
        _inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        let id = config["id"].as_str().unwrap_or_default().to_string();
        self.0.lock().expect("journal").push(format!("start:{id}"));
        self.0.lock().expect("journal").push(format!("end:{id}"));
        Ok(json!({ "id": id }))
    }
}

// ————— Mode service en sous-processus (preuve de reprise, 2026-11-06) —————————————
//
// Le spike `workflow#AC-8` tue le *processus* service, le relance, et vérifie le rejeu. Un
// endpoint servi par `HttpServer::serve` dans le processus du test ne peut pas être tué ainsi :
// boucler la tâche d'acceptation laisse les connexions en vol vivre dans leurs tâches spawnées
// par le SDK (`restate-sdk-0.12.1/src/http_server.rs`). La reprise est donc prouvée contre un
// vrai second processus : le binaire de test se relance lui-même (`mode_service_restate`,
// activé par `MIRYAD_RESTATE_SERVE_PORT`), le test parent le tue d'un SIGKILL et en relance un
// autre sur le même port. Le journal des fixtures s'écrit dans un **fichier** partagée entre
// les générations — c'est l'analogue de la session `lymaille` sauvegardée (`subagent_step.sdd`
// étape 8 : nom de session déterministe, reprise et non recommencement), que ni l'une ni l'autre
// génération ne porte dans sa mémoire.

/// Append une ligne au journal-fichier de la fixture (mode service).
fn journal_fichier(chemin: &std::path::Path, entree: &str) {
    use std::io::Write;
    let fichier = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(chemin)
        .unwrap_or_else(|erreur| panic!("le journal {} doit être ouvrable : {erreur}", chemin.display()));
    writeln!(&mut &fichier, "{entree}").expect("une ligne du journal doit être inscriptible");
}

/// Lignes du journal-fichier ; vide si le fichier n'existe pas encore.
fn lire_journal(chemin: &std::path::Path) -> Vec<String> {
    match std::fs::read_to_string(chemin) {
        Ok(contenu) => contenu.lines().map(str::to_string).collect(),
        Err(_) => Vec::new(),
    }
}

/// Kind `"record"` à journal-fichier (mode service) : comme `Record`, mais le journal survit au
/// kill du processus service — c'est ce qui rend la preuve de reprise lisible par le parent.
struct FileRecord(std::path::PathBuf);

#[async_trait::async_trait]
impl MiryadWorkflowStep for FileRecord {
    fn kind(&self) -> &'static str {
        "record"
    }
    async fn run(
        &self,
        _run: &RunInfo,
        config: Value,
        _inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        let id = config["id"].as_str().unwrap_or_default().to_string();
        journal_fichier(&self.0, &format!("start:{id}"));
        journal_fichier(&self.0, &format!("end:{id}"));
        Ok(json!({ "id": id }))
    }
}

/// Kind `"subagent"` à journal-fichier (mode service, fixture de la preuve `AC-10`) : journalise
/// `start:<id>`, dort `sommeil` (un tour de l'agent en cours, à la `run_batch` de `lymaille`),
/// puis `end:<id>`. Au rejeu après kill, le `ctx.run` du dispatcher n'ayant rien journalisé, la
/// fermeture est ré-exécutée par la nouvelle génération ; si le journal-fichier porte déjà un
/// `start:<id>` (la « session sauvegardée », nom déterministe transposé en `(kind, id)`), la
/// fixture **ne refait pas le tour** déjà commencé : elle journalise `reprise:<id>` au lieu de
/// dormir et rend le résultat attendu. Le `end` n'est journalisé qu'une fois.
struct FileSubagent {
    journal: std::path::PathBuf,
    sommeil: Duration,
}

#[async_trait::async_trait]
impl MiryadWorkflowStep for FileSubagent {
    fn kind(&self) -> &'static str {
        "subagent"
    }
    async fn run(
        &self,
        _run: &RunInfo,
        config: Value,
        _inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        let id = config["id"].as_str().unwrap_or_default().to_string();
        let session_deja_entamee = lire_journal(&self.journal)
            .iter()
            .any(|e| e == &format!("start:{id}"));
        journal_fichier(&self.journal, &format!("start:{id}"));
        if session_deja_entamee {
            journal_fichier(&self.journal, &format!("reprise:{id}"));
        } else {
            tokio::time::sleep(self.sommeil).await;
        }
        journal_fichier(&self.journal, &format!("end:{id}"));
        Ok(json!({ "id": id }))
    }
}

/// Enfant de test servant le mode service, tué au `Drop` (comme `RestateContainer`) pour
/// qu'aucun service orphelin ne survienne à un échec du parent.
struct ServiceChild(std::process::Child);

impl Drop for ServiceChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Relance le binaire de test lui-même en mode service sur `port` (méthode du spike de
/// `workflow#AC-8` : le service est un vrai OS processus, donc tuable et relançable). `sommeil`
/// est la durée du tour interrompu de la fixture `subagent`.
fn lancer_service(port: u16, journal: &std::path::Path, sommeil: Duration) -> ServiceChild {
    let exe = std::env::current_exe().expect("le binaire de test doit être relançable en mode service");
    let enfant = Command::new(exe)
        .args(["--ignored", "--exact", "mode_service_restate", "--test-threads=1"])
        .env("MIRYAD_RESTATE_SERVE_PORT", port.to_string())
        .env("MIRYAD_RESTATE_SERVE_JOURNAL", journal)
        .env("MIRYAD_RESTATE_SERVE_SLEEP_MS", sommeil.as_millis().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("le binaire de test doit pouvoir être relancé comme processus service");
    ServiceChild(enfant)
}

/// Attend qu'un port accepte les connexions TCP (le service — en processus ou en enfant — est
/// prêt ; l'endpoint Restate derrière répond alors sur son port applicatif).
async fn attendre_port(port: u16, quoi: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{quoi} n'a pas répondu sur le port {port} en 60 s"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Attend (60 s) que le journal-fichier partagé vérifie `attendu` ; message d'échec : le
/// contenu réellement observé.
async fn attendre_lignes(chemin: &std::path::Path, description: &str, attendu: impl Fn(&[String]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let lignes = lire_journal(chemin);
        if attendu(&lignes) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{description} : le journal {} n'a pas porté ce qui était attendu dans les 60 s : {lignes:?}",
            chemin.display()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Attend la disponibilité de l'admin API Restate (60 s) puis enregistre le déploiement déclaré
/// par `config` — même séquence que dans `Stack::start_with_options`, isolée pour le test de
/// reprise, qui n'a pas de `Stack` (le service est un sous-processus).
async fn attendre_admin_et_enregistrer(http: &reqwest::Client, config: &WorkflowConfig) {
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
    register_deployment(config, http)
        .await
        .expect("l'enregistrement du déploiement doit réussir");
}

/// Attache le workflow du run à l'ingress (bloque jusqu'à sa complétion, borne de 4 min : le
/// temps que Restate rejoue son journal et que le service rechargé reprenne) ; rend
/// `(statut HTTP, corps JSON)`.
async fn attache_run(http: &reqwest::Client, ingress_url: &str, run_key: &str) -> (u16, Value) {
    let response = http
        .get(format!(
            "{ingress_url}/restate/workflow/DagInterpreter/{run_key}/attach"
        ))
        .timeout(Duration::from_mins(4))
        .send()
        .await
        .expect("l'attache au workflow doit répondre");
    let statut = response.status().as_u16();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    (statut, body)
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
        Self::start_with_options(max_depth, recommended_options()).await
    }

    /// Pile complète avec les options de service `options` posées au `bind()` du
    /// `StepDispatcher` (spike `OI-11` : bornes de handler relevées).
    async fn start_with_options(max_depth: u32, options: restate_sdk::endpoint::ServiceOptions) -> Self {
        let (container, ingress, admin, app) = lancer_conteneur();

        let log: Log = Arc::default();
        let flaky_calls = Arc::new(AtomicUsize::new(0));
        let mut registry = StepRegistry::new();
        registry
            .register(Record(Arc::clone(&log)))
            .and_then(|r| r.register(Fail))
            .and_then(|r| r.register(Flaky(Arc::clone(&flaky_calls))))
            .and_then(|r| r.register(EchoTrigger))
            .and_then(|r| r.register(Subagent(Arc::clone(&log))))
            .and_then(|r| r.register_durable(DurableSleeper))
            .and_then(|r| r.register_durable(DurableConfig))
            .and_then(|r| r.register_durable(SubWorkflowStep::new(max_depth)))
            .expect("kinds distincts");
        let endpoint = Endpoint::builder()
            .bind(DagInterpreter)
            .bind(
                StepDispatcher::new(registry)
                    .into_service_definition()
                    .options(options),
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

    /// Déclenche `dag` par `trigger_run` (./client.rs — clé de run neuve, `POST
    /// {ingress}/DagInterpreter/{run_key}/run/send`) avec `vars` vides, l'identité du déclencheur
    /// `trigger` transmise telle quelle, et rend sa clé de run. Depuis la tâche B5 (amendement
    /// 2026-10-30, lot B), `trigger_run` porte le paramètre `vars` et c'est lui qui construit le
    /// corps `RunInput` (`{"dag": [..steps..], "vars": {}}`) — l'appel direct à l'ingress que
    /// cette fonction postait jusqu'ici n'a plus de raison d'être. Depuis la tâche T5 (amendement
    /// 2026-11-02, lot 0 lymaille), `trigger_run` porte aussi le paramètre `trigger` : les tests
    /// de fan-out passent `None` (ils n'exercent pas l'identité du déclencheur — verrouillée en
    /// unitaire dans ./client.rs et ./interpreter.rs) ; seul le Scenario T6b de
    /// `subworkflow_lenfant_hérite_du_trigger_du_parent` en pose une, pour prouver l'héritage
    /// parent → enfant. Les tests n'exercent ici que des `vars` vides ; la recopie des `vars` non
    /// vides est verrouillée en unitaire dans ./interpreter.rs.
    async fn send_run(&self, dag: &DagSteps, trigger: Option<&str>) -> String {
        let handle = trigger_run(&self.config, &self.http, dag, &serde_json::Map::new(), trigger)
            .await
            .unwrap_or_else(|erreur| panic!("le déclenchement doit être accepté : {erreur}"));
        handle.run_key
    }

    /// Déclenche `dag` (avec l'identité `trigger`, voir [`Self::send_run`]) puis attend sa
    /// complétion (`attach`) : `(statut HTTP, corps JSON)`.
    async fn run_to_completion(&self, dag: &DagSteps, trigger: Option<&str>) -> (u16, Value) {
        let run_key = self.send_run(dag, trigger).await;
        let response = self
            .http
            .get(format!(
                "{}/restate/workflow/DagInterpreter/{}/attach",
                self.config.ingress_url, run_key
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

    /// Attend (bornes de 30 s : la table est eventually consistent après l'achèvement du run)
    /// qu'une invocation `StepDispatcher` dont la cible satisfait `attendu` apparaisse dans
    /// `sys_invocation`. Voir [`attendre_cible_pour`] — l'extraction du 2026-11-06 qui rend la
    /// requête `sys_invocation` utilisable sans `Stack` (preuve de reprise). La lecture brute
    /// des cibles, avant extraction portée par cette méthode, est désormais [`cibles_dispatchers`].
    async fn attendre_cible(&self, attendu: impl Fn(&str) -> bool, description: &str) {
        attendre_cible_pour(&self.http, &self.config.admin_url, attendu, description).await;
    }
}

/// Cibles des invocations `StepDispatcher` lues dans `sys_invocation` de l'admin API
/// (`POST /query`, corps `{"query": …}`, réponse JSON `{"rows": […]}` sous
/// `Accept: application/json` — sans cet en-tête le serveur rend du Arrow IPC binaire, vérifié
/// sur `restate:latest` = 1.7.12). Le champ `target` porte le couple service/handler
/// (`StepDispatcher/…`). Extraite de `Stack::dispatcher_targets` le 2026-11-06 : le test de
/// reprise n'a pas d'endpoint en processus, donc pas de `Stack`, mais lit la même table.
async fn cibles_dispatchers(http: &reqwest::Client, admin_url: &str) -> Vec<String> {
    let response = http
        .post(format!("{admin_url}/query"))
        .header("Accept", "application/json")
        .json(&json!({ "query": "SELECT target FROM sys_invocation" }))
        .send()
        .await
        .expect("la requête à l'admin API doit répondre");
    let corps = response.json::<Value>().await.unwrap_or(Value::Null);
    corps
        .get("rows")
        .and_then(Value::as_array)
        .map(|lignes| {
            lignes
                .iter()
                .filter_map(|ligne| ligne.get("target")?.as_str().map(str::to_string))
                .filter(|t| t.contains("StepDispatcher"))
                .collect()
        })
        .unwrap_or_default()
}

/// Attend (bornes de 30 s : la table est eventually consistent après l'achèvement du run)
/// qu'une invocation `StepDispatcher` dont la cible satisfait `attendu` apparaisse dans
/// `sys_invocation`. Extraite de `Stack::attendre_cible` le 2026-11-06, même rationale que
/// [`cibles_dispatchers`].
async fn attendre_cible_pour(
    http: &reqwest::Client,
    admin_url: &str,
    attendu: impl Fn(&str) -> bool,
    description: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let cibles = cibles_dispatchers(http, admin_url).await;
        if cibles.iter().any(|t| attendu(t)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "aucune invocation StepDispatcher visant {description} dans les cibles : {cibles:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
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
    let (status, body) = stack.run_to_completion(&dag, None).await;
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
    let (status, body) = stack.run_to_completion(&dag, None).await;
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
    let (status, body) = stack.run_to_completion(&dag, None).await;
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
    let (status, body) = stack.run_to_completion(&dag, None).await;
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
    let run_key = stack.send_run(&dag, None).await;
    let response = stack
        .http
        .get(format!(
            "{}/restate/workflow/DagInterpreter/{}/attach",
            stack.config.ingress_url, run_key
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
        body["D"]["run_key"], run_key,
        "le run_key vu par le kind doit être la clé du run déclenché : {body}"
    );
    assert_eq!(body["D"]["step_id"], "D", "le step_id vu par le kind : {body}");
    assert_eq!(
        body["D"]["depth"], 0,
        "un run racine est à la profondeur 0 : {body}"
    );
}

/// Scenario « la config d'un kind durable est rendue avant `run` » (./dispatcher.sdd étape 1b,
/// tâche templating 2026-10-06) : le kind durable `durable_config` reçoit `{{run.run_key}}`
/// rendu avec la clé du run déclenché ; le rendu passe par le `ctx.run()` dédié de la branche
/// durable, sans enveloppe autour de `run`.
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn un_kind_durable_recoit_sa_config_rendue() {
    let stack = Stack::start().await;
    let dag = DagSteps(vec![step(
        "D",
        &[],
        "durable_config",
        json!({ "m": "{{run.run_key}}" }),
    )]);
    let run_key = stack.send_run(&dag, None).await;
    let response = stack
        .http
        .get(format!(
            "{}/restate/workflow/DagInterpreter/{}/attach",
            stack.config.ingress_url, run_key
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
        body["D"]["m"], run_key,
        "le kind durable doit recevoir la config rendue (run_key) : {body}"
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
    let (status, body) = stack.run_to_completion(&dag, None).await;
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
    let (status, body) = stack.run_to_completion(&dag, None).await;
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
    let (status, body) = stack.run_to_completion(&dag, None).await;
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

/// Scenario (./subworkflow.sdd `Tasks` T6b, 2026-11-02, lot 0 lymaille, `FR-10`/`DEC-12`)
/// « un parent déclenché avec `trigger = Some("sub-42")` fait rendre `sub-42` à un kind de
/// fixture du run enfant » : parent d'un unique step `S` (`subworkflow`) dont le DAG enfant est
/// un step `E` du kind ordinaire `echo_trigger`, qui rend son `RunInfo.trigger`. L'identité ne
/// transite que par le canal autorisé — `run_child_dag` poste un `RunInput` dont `trigger` est
/// `self.run.trigger` du parent (./durable.rs, T6a) ; le `config` du step n'en porte aucune et
/// l'enfant ne peut pas la tenir de là (verrou unitaire dans ./subworkflow.rs). Avant T6a,
/// l'écho était `null` (`trigger: None` posté) ; ce rouge n'est observable qu'en jeu contre
/// `restate-server` — aucun `Context` virtualisable en unitaire (borne ./durable.sdd, intégration
/// exigée par la spec).
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn subworkflow_lenfant_hérite_du_trigger_du_parent() {
    let stack = Stack::start().await;
    let dag = DagSteps(vec![step(
        "S",
        &[],
        "subworkflow",
        // Le config du step ne porte AUCUNE identité : l'enfant ne peut tenir `sub-42` que du
        // parent, par `run_child_dag`.
        json!({ "dag": [step("E", &[], "echo_trigger", json!({}))] }),
    )]);
    let (status, body) = stack.run_to_completion(&dag, Some("sub-42")).await;
    assert_eq!(status, 200, "corps : {body}");
    let sorties_enfant = body["S"].as_object().expect("S rend la table des sorties enfant");
    assert_eq!(
        sorties_enfant["E"],
        json!("sub-42"),
        "l'enfant doit rendre le trigger hérité du parent, pas null : {body}"
    );
}

/// Spike `OI-11` (DSD `lymaille#OI-11`, `DEC-29`) : un step ordinaire d'environ 40 minutes, dans l'unique
/// `ctx.run` de `execute`, survit quand `inactivity_timeout` et `abort_timeout` sont relevés **sur le handler**
/// `execute` (`HandlerOptions`, le mécanisme d'un handler dédié) : un seul `start`, aucun rejeu, résultat rendu.
///
/// Durée : environ 40 minutes. Lancement explicite et isolé :
/// `cargo test --no-default-features --features workflow --test workflow_restate un_step_de_40_minutes -- --ignored --nocapture`.
/// Hors preuve (le serveur de test vit dans le processus du test, voir l'en-tête) : la reprise après kill du
/// processus service reste couverte par `AC-10` de `lymaille`, via la session déterministe.
#[tokio::test]
#[ignore = "exige podman et l'image restate ; dure environ 40 minutes"]
async fn un_step_de_40_minutes_survit_aux_bornes_relevees_du_handler() {
    use restate_sdk::endpoint::HandlerOptions;
    use restate_sdk::endpoint::ServiceOptions;
    let options = recommended_options_pour_spike(
        ServiceOptions::new().handler(
            "execute",
            HandlerOptions::new()
                .inactivity_timeout(Duration::from_mins(50))
                .abort_timeout(Duration::from_mins(55)),
        ),
    );
    let stack = Stack::start_with_options(DEFAULT_MAX_DEPTH, options).await;
    let debut = Instant::now();
    let dag = DagSteps(vec![step(
        "L",
        &[],
        "record",
        json!({ "id": "L", "sleep_ms": 40 * 60 * 1000 }),
    )]);
    let run_key = stack.send_run(&dag, None).await;
    let response = stack
        .http
        .get(format!(
            "{}/restate/workflow/DagInterpreter/{}/attach",
            stack.config.ingress_url, run_key
        ))
        .timeout(Duration::from_mins(60))
        .send()
        .await
        .expect("l'attache au workflow doit répondre");
    let status = response.status().as_u16();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    let duree = debut.elapsed();
    println!(
        "OI-11 : statut {status}, durée mesurée {duree:?}, journal {:?}",
        stack.journal()
    );
    assert_eq!(status, 200, "corps : {body}");
    assert!(duree >= Duration::from_mins(40), "le step a dormi {duree:?}");
    let journal = stack.journal();
    assert_eq!(
        journal.iter().filter(|e| e.as_str() == "start:L").count(),
        1,
        "aucun rejeu : {journal:?}"
    );
    assert_eq!(journal.iter().filter(|e| e.as_str() == "end:L").count(), 1);
}

/// Scenario « un step subagent est exécuté par `execute_long` » (./dispatcher.sdd, amendement
/// DEC-29 du 2026-11-03, preuve d'intégration) : la pile `Stack::start` lie le `StepDispatcher`
/// avec `recommended_options()` — dont l'entrée de handler `execute_long` (le `bind` panique si le
/// service ne porte pas ce second handler). Un DAG d'un unique step du kind `"subagent"` (seul
/// membre de `LONG_RUNNING_KINDS`) réussit et son invocation vise `execute_long`, lu dans
/// `sys_invocation.target` de l'admin API ; un second run, DAG d'un unique step `record`, vise
/// `execute` — les bornes relevées ne sortent jamais du handler dédié.
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn un_step_subagent_est_exécuté_par_execute_long() {
    let stack = Stack::start().await;
    let dag_long = DagSteps(vec![step("S", &[], "subagent", json!({ "id": "S" }))]);
    let (status, body) = stack.run_to_completion(&dag_long, None).await;
    assert_eq!(status, 200, "le step subagent doit mener le run au bout : {body}");
    let journal = stack.journal();
    assert!(
        journal.iter().any(|e| e == "start:S") && journal.iter().any(|e| e == "end:S"),
        "le step subagent doit s'exécuter : {journal:?}"
    );
    stack
        .attendre_cible(|t| t.contains("execute_long"), "execute_long")
        .await;
    let dag_ord = DagSteps(vec![step("R", &[], "record", json!({ "id": "R" }))]);
    let (status, body) = stack.run_to_completion(&dag_ord, None).await;
    assert_eq!(status, 200, "le step record doit mener le run au bout : {body}");
    stack
        .attendre_cible(
            |t| t.contains("StepDispatcher") && t.contains("execute") && !t.contains("execute_long"),
            "execute (jamais execute_long pour un kind ordinaire)",
        )
        .await;
}

/// Mode service du binaire de test (sous-processus de
/// [`reprise_apres_kill_du_service_sur_step_subagent`]) : sert le `DagInterpreter` +
/// `StepDispatcher` (kinds de fixture à journal-fichier, options `recommended_options()` — le
/// kind `"subagent"` passe donc par `execute_long`, bornes relevées) sur le port donné, sans
/// jamais rien valider : c'est le parent qui observe. Tant que
/// `MIRYAD_RESTATE_SERVE_PORT` est posé, ce « test » ne rend jamais la main — le parent le tue
/// (SIGKILL, méthode du spike `workflow#AC-8`) et le relance. Sans la variable d'environnement
/// (lancement `-- --ignored` de la batterie), il ne fait rien : il est là pour être relancé, pas
/// pour tourner seul.
#[tokio::test]
#[ignore = "mode service : ne sert que relancé avec MIRYAD_RESTATE_SERVE_PORT par le test de reprise"]
async fn mode_service_restate() {
    let Ok(port) = std::env::var("MIRYAD_RESTATE_SERVE_PORT") else {
        return;
    };
    let port: u16 = port
        .parse()
        .expect("MIRYAD_RESTATE_SERVE_PORT doit être un numéro de port");
    let journal = std::path::PathBuf::from(
        std::env::var("MIRYAD_RESTATE_SERVE_JOURNAL")
            .expect("le chemin du journal est requis en mode service"),
    );
    let sommeil_ms = match std::env::var("MIRYAD_RESTATE_SERVE_SLEEP_MS") {
        Ok(v) => v
            .parse::<u64>()
            .expect("la durée du tour interrompu doit être un nombre de millisecondes"),
        Err(_) => 90_000,
    };
    let sommeil = Duration::from_millis(sommeil_ms);
    let mut registry = StepRegistry::new();
    registry
        .register(FileRecord(journal.clone()))
        .and_then(|r| r.register(FileSubagent { journal, sommeil }))
        .expect("kinds distincts");
    let endpoint = Endpoint::builder()
        .bind(DagInterpreter)
        .bind(
            StepDispatcher::new(registry)
                .into_service_definition()
                .options(recommended_options()),
        )
        .build();
    // Le port peut porter encore la trace du processus tué d'un SIGKILL : retenter le bind 10 s.
    let deadline = Instant::now() + Duration::from_secs(10);
    let listener = loop {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => break l,
            Err(erreur) => {
                assert!(
                    Instant::now() < deadline,
                    "le port {port} ne s'est pas libéré pour le service : {erreur}"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    // Ne rend jamais la main (le `serve` de `HttpServer` attend ctrl_c, que le parent ne
    // transmet pas : il tue) ; si le parent tue par SIGINT, la fermeture gracieuse s'arrête ici.
    HttpServer::new(endpoint).serve(listener).await;
}

/// Preuve de reprise (tâche `subagent_step.sdd`, DSD `lymaille#AC-10`, méthode du spike
/// `workflow#AC-8`, 2026-11-06) : le service est un sous-processus relancé par le binaire
/// lui-même ([`mode_service_restate`]), pas un endpoint en processus. Un DAG `A` (kind `record`)
/// puis `S` (kind `"subagent"`, routé `execute_long`) est lancé ; dès que `A` a **commité** son
/// `ctx.run` (le `end:A` est au journal-fichier, la réponse est dans le journal Restate) et que le
/// step subagent a entamé son tour (`start:S`, pas encore `end:S`), le service est tué d'un
/// **SIGKILL** et relancé sur le même port — mémoire neuve, seul le journal Restate (côté
/// serveur) et le journal-fichier (la « session » de la fixture) survivent.
///
/// Ce que la reprise doit rendre, journal en main :
/// - `A` : exécuté **une seule fois** au total — son `ctx.run` journalisé avant le kill est
///   rejoué par Restate, la fermeture n'est pas ré-exécutée (preuve par le `sys_invocation` :
///   une seule invocation `StepDispatcher/execute`) ;
/// - `S` : l'effet interrompu est **ré-exécuté** (le `ctx.run` n'avait rien journalisé —
///   « au moins une fois » sur le seul tour interrompu, DSD `DEC-11`) : deux `start:S` au total,
///   mais la nouvelle génération retrouve la session déjà entamée (`reprise:S`, le tour n'est pas
///   refait à l'identique), le résultat est commité **une seule fois** (`end:S` unique), et
///   l'attache du run rend `{"id": "S"}` attendu, sans doublon.
///
/// Durée : ~30 s hors démarrage de l'image (le sommeil de 90 s du premier `S` est coupé par le
/// kill, la reprise ne le refait pas). Non verrouillé par la CI, comme tout le fichier.
#[tokio::test]
#[ignore = "exige podman et l'image restate ; relance le binaire de test en sous-processus"]
async fn reprise_apres_kill_du_service_sur_step_subagent() {
    let (_container, ingress, admin, app) = lancer_conteneur();
    let http = reqwest::Client::new();
    let config = WorkflowConfig {
        admin_url: format!("http://127.0.0.1:{admin}"),
        ingress_url: format!("http://127.0.0.1:{ingress}"),
        deployment_url: format!("http://127.0.0.1:{app}"),
    };
    let journal = std::env::temp_dir().join(format!("miryad-restate-kill-{app}.log"));
    let _ = std::fs::remove_file(&journal);

    // Génération 1 du service : sous-processus, donc tuable (méthode du spike).
    let mut service = lancer_service(app, &journal, Duration::from_secs(90));
    attendre_port(app, "le service (génération 1)").await;
    attendre_admin_et_enregistrer(&http, &config).await;

    let dag = DagSteps(vec![
        step("A", &[], "record", json!({ "id": "A" })),
        step("S", &["A"], "subagent", json!({ "id": "S" })),
    ]);
    let handle = trigger_run(&config, &http, &dag, &serde_json::Map::new(), None)
        .await
        .unwrap_or_else(|erreur| panic!("le déclenchement doit être accepté : {erreur}"));
    let run_key = handle.run_key;

    // Fenêtre du kill : `A` terminé et journalisé, `S` en cours de tour sans rien de commité.
    attendre_lignes(
        &journal,
        "`end:A` (A commité) et `start:S` (S interrompu)",
        |lignes| lignes.iter().any(|e| e == "end:A") && lignes.iter().any(|e| e == "start:S"),
    )
    .await;

    // ————— Le kill : SIGKILL sur le processus service, rien d'autre ne survit côté app.
    service.0.kill().expect("le service doit être tuable");
    let statut = service.0.wait().expect("le service tué doit être récupéré");
    assert!(
        statut.code().is_none(),
        "le service doit avoir été tué par signal, pas sorti proprement : {statut:?}"
    );
    // ————— Le rechargement : second processus, mémoire neuve, même port et même journal.
    let _service_rechargé = lancer_service(app, &journal, Duration::from_secs(90));
    attendre_port(app, "le service rechargé (génération 2)").await;

    // L'attache bloque pendant que Restate rejoue son journal et retente l'invocation
    // interrompue : cette attente EST la reprise.
    let (statut_attache, body) = attache_run(&http, &config.ingress_url, &run_key).await;
    assert_eq!(
        statut_attache, 200,
        "le run interrompu doit être mené au bout par la reprise : {statut_attache} {body}"
    );
    assert_eq!(body["A"]["id"], "A", "la sortie commitée de A : {body}");
    assert_eq!(
        body["S"]["id"], "S",
        "la reprise doit rendre le résultat attendu de S, sans doublon : {body}"
    );

    let lignes = lire_journal(&journal);
    let compte = |cible: &str| lignes.iter().filter(|e| e.as_str() == cible).count();
    assert_eq!(
        compte("start:A"),
        1,
        "A, journalisé avant le kill, n'est jamais ré-exécuté par le service rechargé : {lignes:?}"
    );
    assert_eq!(compte("end:A"), 1, "A n'a commité qu'une fois : {lignes:?}");
    assert_eq!(
        compte("start:S"),
        2,
        "l'effet interrompu est ré-exécuté une fois (« au moins une fois », DEC-11), pas davantage : {lignes:?}"
    );
    assert_eq!(
        compte("reprise:S"),
        1,
        "la génération 2 doit retrouver la session déjà entamée et ne pas refaire le tour : {lignes:?}"
    );
    assert_eq!(
        compte("end:S"),
        1,
        "le résultat est commité une seule fois : {lignes:?}"
    );

    // Preuve par le journal Restate : le kill n'a pas dédoublé les invocations — une invocation
    // par step (`A` sous `execute`, `S` sous le handler dédié `execute_long`), et le rejeu de la
    // reprise n'en a pas créé de nouvelle.
    attendre_cible_pour(
        &http,
        &config.admin_url,
        |t| t.ends_with("/execute_long"),
        "execute_long (step subagent)",
    )
    .await;
    attendre_cible_pour(
        &http,
        &config.admin_url,
        |t| t.ends_with("/execute"),
        "execute (step record)",
    )
    .await;
    let cibles = cibles_dispatchers(&http, &config.admin_url).await;
    assert_eq!(
        cibles.iter().filter(|t| t.ends_with("/execute")).count(),
        1,
        "le kill n'a pas dupliqué l'invocation de A, journalisée et terminée avant lui : {cibles:?}"
    );
    assert_eq!(
        cibles.iter().filter(|t| t.ends_with("/execute_long")).count(),
        1,
        "l'invocation d'un step subagent survit au kill et se rejoue sous la même entrée : {cibles:?}"
    );
    let _ = std::fs::remove_file(&journal);
}

/// Ajoute les réglages du spike `OI-11` au `ServiceOptions` de base (retry plafonné) : les options de
/// handler portent les bornes de durée, celles de service la politique de retry.
fn recommended_options_pour_spike(
    base: restate_sdk::endpoint::ServiceOptions,
) -> restate_sdk::endpoint::ServiceOptions {
    base.retry_policy_max_attempts(5)
        .retry_policy_pause_on_max_attempts()
}
