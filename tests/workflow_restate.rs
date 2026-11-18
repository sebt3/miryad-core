//! Crate d'intégration rattachée à `src/workflow/interpreter.sdd` (`Tasks` : « valider par un test
//! d'intégration le protocole de fan-out avec l'implémentation réelle »). Contrairement à la
//! batterie unitaire, ces tests parlent à un vrai `restate-server` (conteneur `podman`, image
//! `docker.restate.dev/restatedev/restate:latest`, réseau hôte, ports libres) et servent le vrai
//! `DagInterpreter` + le vrai `StepDispatcher` (registre de kinds de fixture) depuis le processus
//! de test. Y sont aussi rattachés, depuis l'issue #80 : la preuve des noms de `ctx.run` dans
//! `sys_journal` (`src/workflow/dispatcher.sdd`, tâche « Issue #80 (1) ») et les mesures des
//! formes de l'admin API — `POST /query`, `sys_invocation`, `sys_journal`,
//! `PATCH …/cancel` — par le client d'administration (`src/workflow/client.sdd`, tâche
//! « Issue #80 (2) ») ; ces tests lisent et pilotent Restate par les fonctions mêmes de
//! `workflow::client` qu'ils valident (au-delà des seuls appels de `trigger_run`, mention étendue
//! le 2026-11-15 d'après le `Can modify` de ./client.sdd).
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
use miryad_core::workflow::InvocationFilter;
use miryad_core::workflow::InvocationStatus;
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
use miryad_core::workflow::cancel_invocation;
use miryad_core::workflow::invocation_journal;
use miryad_core::workflow::invocation_statuses;
use miryad_core::workflow::list_invocations;
use miryad_core::workflow::recommended_options;
use miryad_core::workflow::register_deployment;
use miryad_core::workflow::reject_awakeable;
use miryad_core::workflow::resolve_awakeable;
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

/// Kind `"rendu"` de fixture (issue #85, intégration AC-17) : rend `config["output"]` verbatim —
/// une sortie dont la forme est posée par le DAG, seule façon de viser un pointeur d'export
/// (`/v`) depuis un step — après avoir journalisé `start:<id>`/`end:<id>` comme [`Record`].
struct Rendu(Log);

#[async_trait::async_trait]
impl MiryadWorkflowStep for Rendu {
    fn kind(&self) -> &'static str {
        "rendu"
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
        Ok(config.get("output").cloned().unwrap_or(Value::Null))
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

/// Kind durable de fixture `"attend_awakeable"` (issue #26, ./durable.sdd) : crée un awakeable,
/// remet l'identifiant par un effet journalisé (`run_effect` — écriture `awakeable:<id>` dans le
/// journal-fichier partagé, comme `mode_service_restate` — la remise n'est donc jamais rejouée au
/// rejeu), attend, et rend la valeur résolue. L'ordre `awakeable` → `run_effect` → attente est
/// celui prescrit par ./durable.sdd.
struct AttendAwakeable(std::path::PathBuf);

#[async_trait::async_trait]
impl MiryadDurableStep for AttendAwakeable {
    fn kind(&self) -> &'static str {
        "attend_awakeable"
    }
    async fn run(
        &self,
        ctx: &StepContext<'_>,
        _config: Value,
        _inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        let (identifiant, attente) = ctx.awakeable::<Value>();
        remettre_identifiant_awakeable(ctx, &self.0, identifiant).await?;
        attente.await
    }
}

/// Kind durable de fixture `"attend_awakeable_delai"` (issue #26) : comme [`AttendAwakeable`],
/// avec le délai durable lu dans `config` sous la clé `delai_s` (secondes, défaut 2).
struct AttendAwakeableDelai(std::path::PathBuf);

#[async_trait::async_trait]
impl MiryadDurableStep for AttendAwakeableDelai {
    fn kind(&self) -> &'static str {
        "attend_awakeable_delai"
    }
    async fn run(
        &self,
        ctx: &StepContext<'_>,
        config: Value,
        _inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        let delai = Duration::from_secs(config["delai_s"].as_u64().unwrap_or(2));
        let (identifiant, attente) = ctx.awakeable_timeout::<Value>(delai);
        remettre_identifiant_awakeable(ctx, &self.0, identifiant).await?;
        attente.await
    }
}

/// Remise de l'identifiant d'awakeable par effet journalisé (patron prescrit par ./durable.sdd :
/// `awakeable` → `run_effect` → attente). L'écriture `awakeable:<id>` dans le journal-fichier est
/// l'effet observable ; le résultat mémoïse garantit qu'au rejeu elle n'a plus lieu.
async fn remettre_identifiant_awakeable(
    ctx: &StepContext<'_>,
    journal: &std::path::Path,
    identifiant: String,
) -> Result<(), StepError> {
    let journal = journal.to_path_buf();
    ctx.run_effect(move || async move {
        journal_fichier(&journal, &format!("awakeable:{identifiant}"));
        Ok(identifiant)
    })
    .await
    .map(|_| ())
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
    /// Journal-fichier des kinds d'awakeable (issue #26) — partagé entre le test et les fixtures
    /// durables, supprimé au `Drop`.
    journal_awakeable: std::path::PathBuf,
}

impl Drop for Stack {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.journal_awakeable);
    }
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
        // Journal-fichier des fixtures d'awakeable (issue #26) : la remise de l'identifiant y est
        // écrite par `run_effect` — lisible par le test, mémoïsée par le journal Restate.
        let journal_awakeable = std::env::temp_dir().join(format!("miryad-restate-awakeable-{app}.log"));
        let _ = std::fs::remove_file(&journal_awakeable);
        let mut registry = StepRegistry::new();
        registry
            .register(Record(Arc::clone(&log)))
            .and_then(|r| r.register(Fail))
            .and_then(|r| r.register(Flaky(Arc::clone(&flaky_calls))))
            .and_then(|r| r.register(Rendu(Arc::clone(&log))))
            .and_then(|r| r.register(EchoTrigger))
            .and_then(|r| r.register(Subagent(Arc::clone(&log))))
            .and_then(|r| r.register_durable(DurableSleeper))
            .and_then(|r| r.register_durable(DurableConfig))
            .and_then(|r| r.register_durable(SubWorkflowStep::new(max_depth)))
            .and_then(|r| r.register_durable(AttendAwakeable(journal_awakeable.clone())))
            .and_then(|r| r.register_durable(AttendAwakeableDelai(journal_awakeable.clone())))
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
            journal_awakeable,
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
        when: None,
        run_if_skipped: false,
        exports: std::collections::BTreeMap::new(),
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
        .and_then(|r| {
            r.register(FileSubagent {
                journal: journal.clone(),
                sommeil,
            })
        })
        .and_then(|r| r.register_durable(AttendAwakeable(journal.clone())))
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

/// Scenario « le journal d'une invocation nomme ses runs » (issue #80 (1), ./dispatcher.sdd,
/// amendement du 2026-11-13) : un DAG de deux steps indépendants — `A` (kind ordinaire `record`)
/// et `S` (kind durable `durable_sleep`) — joué contre un vrai `restate-server`. Lecture par le
/// client d'administration lui-même (`list_invocations`/`invocation_journal`, ./client.sdd),
/// dogfooding du contrat.
///
/// **[!] mesuré le 2026-11-15 sur restate 1.8.1** : l'invocation du step ordinaire `A` ne porte
/// qu'un seul run nommé, `step:A` — le rendu s'exécute **dans** ce run (`run_invocation` est le
/// corps de l'unique `ctx.run` de l'invocation, « un seul `ctx.run()` par invocation » du même
/// fichier). Le membre du Scenario « l'invocation de A contient render:A puis step:A » est
/// impossible sous ce `Must` : le présent test verrouille la forme mesurée en attendant
/// l'arbitrage du Scenario par spec-dd. La branche durable, elle, conforme : `render:S` seul.
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn le_journal_d_une_invocation_nomme_ses_runs() {
    let stack = Stack::start().await;
    let dag = DagSteps(vec![
        step("A", &[], "record", json!({ "id": "A" })),
        step("S", &[], "durable_sleep", json!({})),
    ]);
    let (status, body) = stack.run_to_completion(&dag, None).await;
    assert_eq!(status, 200, "corps : {body}");

    let deadline = Instant::now() + Duration::from_mins(1);
    let invocations = loop {
        let filtre = InvocationFilter {
            service: Some("StepDispatcher".to_string()),
            handler: None,
            status: None,
            limit: Some(100),
        };
        let invocations = list_invocations(&stack.config, &stack.http, &filtre)
            .await
            .expect("la lecture de sys_invocation doit réussir");
        if invocations.len() >= 2 || Instant::now() >= deadline {
            break invocations;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert_eq!(
        invocations.len(),
        2,
        "une invocation StepDispatcher par step du DAG : {invocations:?}"
    );

    let mut vue_a: Option<(String, Vec<String>)> = None;
    let mut vue_s: Option<(String, Vec<String>)> = None;
    for invocation in &invocations {
        let journal = invocation_journal(&stack.config, &stack.http, &invocation.id)
            .await
            .expect("la lecture de sys_journal doit réussir");
        for paire in journal.windows(2) {
            assert!(
                paire[0].index < paire[1].index,
                "le journal doit remonter ordonné par index : {journal:?}"
            );
        }
        let noms: Vec<String> = journal.iter().filter_map(|e| e.name.clone()).collect();
        println!(
            "mesure #80 : invocation {} (status {}, handler {}) — entrées {} dont noms {noms:?}",
            invocation.id,
            invocation.status,
            invocation.handler,
            journal.len()
        );
        if noms.iter().any(|n| n == "step:A") {
            vue_a = Some((invocation.id.clone(), noms));
        } else if noms.iter().any(|n| n == "render:S") {
            vue_s = Some((invocation.id.clone(), noms));
        }
    }

    let (id_a, noms_a) = vue_a.expect("l'invocation du step ordinaire A, reconnue à step:A");
    assert_eq!(
        noms_a.iter().filter(|n| n.ends_with(":A")).count(),
        1,
        "forme mesurée : un step ordinaire a un seul run nommé, step:A (le rendu vit dans ce \
         run — [!] en attente d'arbitrage du Scenario « le journal d'une invocation nomme ses \
         runs ») : {noms_a:?}"
    );
    assert!(
        !noms_a.iter().any(|n| n == "render:A"),
        "mesuré : aucun run de rendu séparé pour un kind ordinaire sous « un seul ctx.run() \
         par invocation » : {noms_a:?}"
    );
    let _ = id_a;

    let (id_s, noms_s) = vue_s.expect("l'invocation du kind durable S, reconnue à render:S");
    assert!(
        !noms_s.iter().any(|n| n == "step:S"),
        "aucun run englobant un kind durable : {noms_s:?}"
    );
    assert_eq!(
        noms_s.iter().filter(|n| n.ends_with(":S")).count(),
        1,
        "le kind durable n'a que render:S dans le journal du dispatcher : {noms_s:?}"
    );
    let _ = id_s;
}

/// Attend (mesure live #80) que l'invocation soit vue **en cours d'exécution** dans
/// `sys_invocation` — la table est progressivement cohérente après le `202` de l'ingress — et
/// rend la ligne lue.
async fn ligne_en_cours(stack: &Stack, id: &str) -> InvocationStatus {
    let id = id.to_owned();
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        let statuts = invocation_statuses(&stack.config, &stack.http, std::slice::from_ref(&id))
            .await
            .expect("la lecture du statut doit réussir");
        if let Some(premier) = statuts.first()
            && premier.status.to_ascii_lowercase().contains("running")
        {
            println!("mesure #80 : sys_invocation en cours de run : {premier:?}");
            return premier.clone();
        }
        assert!(
            Instant::now() < deadline,
            "l'invocation n'a jamais été vue en cours d'exécution"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Attend que l'invocation soit sortie des états actifs (`pending`/`scheduled`/`ready`/
/// `running`/`backoff`) après une annulation, et rend sa ligne finale — celle que la mesure #80
/// consigne verbatim (`completed`, `cancelled` ou autre, orthographe du serveur).
async fn ligne_terminée(stack: &Stack, id: &str) -> InvocationStatus {
    let id = id.to_owned();
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        let statuts = invocation_statuses(&stack.config, &stack.http, std::slice::from_ref(&id))
            .await
            .expect("la relecture du statut doit réussir");
        if let Some(premier) = statuts.first() {
            let etat = premier.status.to_ascii_lowercase();
            let actif = ["running", "pending", "scheduled", "ready", "backoff"]
                .iter()
                .any(|nom| etat.contains(nom));
            if !actif {
                return premier.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "l'invocation annulée n'est jamais sortie de l'état en cours"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Rend les `name` du journal de la première invocation du `StepDispatcher` dont le journal
/// nomme `nom_attendu` (mesure live #80 : `render:<id>` puis `step:<id>`), `index` croissants.
async fn noms_d_un_journal_de_step(stack: &Stack, nom_attendu: &str) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_mins(1);
    let filtre = InvocationFilter {
        service: Some("StepDispatcher".to_string()),
        handler: None,
        status: None,
        limit: Some(100),
    };
    loop {
        let invocations = list_invocations(&stack.config, &stack.http, &filtre)
            .await
            .expect("la lecture de sys_invocation doit réussir");
        for invocation in &invocations {
            let journal = invocation_journal(&stack.config, &stack.http, &invocation.id)
                .await
                .expect("la lecture de sys_journal doit réussir");
            let noms: Vec<String> = journal.iter().filter_map(|entree| entree.name.clone()).collect();
            if noms.iter().any(|nom| nom == nom_attendu) {
                return noms;
            }
        }
        assert!(
            Instant::now() < deadline,
            "aucun journal de step ordinaire nommant {nom_attendu} en 60 s"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Scenarios « annulation d'un run en cours puis statut » et « le journal nomme les étapes »
/// (issue #80 (2), ./client.sdd, amendement du 2026-11-13), contre un vrai `restate-server` :
/// un run lent déclenché par `trigger_run` est lu par `invocation_statuses` (une requête, par
/// l'`invocation_id` du corps de réponse), annulé par `cancel_invocation` (`PATCH …/cancel`),
/// puis relu — il sort de l'état en cours. Idempotence mesurée : re-annuler une invocation
/// terminée rend `Ok(())` ; un identifiant bien formé mais inconnu est **absent** du résultat.
/// La liste filtrée par service contient l'invocation. Second run court : le journal de
/// l'invocation du step ordinaire `L` nomme son run d'exécution `step:L` (forme mesurée, [!]
/// doc au test `le_journal_d_une_invocation_nomme_ses_runs`), ordonné par `index`.
/// Toutes les formes observées (statuts, dates, `key`, codes de `cancel`) sont consignées par la
/// sortie `--nocapture` et le rapport de tâche, avec la version de Restate.
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn administration_annulation_statut_et_journal_mesurés() {
    let stack = Stack::start().await;
    let dag_lent = DagSteps(vec![step(
        "D0",
        &[],
        "record",
        json!({ "id": "D0", "sleep_ms": 30000 }),
    )]);
    let handle = trigger_run(
        &stack.config,
        &stack.http,
        &dag_lent,
        &serde_json::Map::new(),
        None,
    )
    .await
    .expect("le déclenchement doit être accepté");
    println!(
        "mesure #80 : invocation_id lu du corps 202 = {}",
        handle.invocation_id
    );

    // 1) Le statut devient joignable puis passe en cours (`sys_invocation` eventually consistent).
    let premier = ligne_en_cours(&stack, &handle.invocation_id).await;
    assert_eq!(premier.id, handle.invocation_id);
    assert_eq!(
        premier.service, "DagInterpreter",
        "colonne target_service_name mesurée"
    );
    assert_eq!(premier.handler, "run", "colonne target_handler_name mesurée");

    // 2) Annulation propre (`cancel`, pas `kill`).
    cancel_invocation(&stack.config, &stack.http, &handle.invocation_id)
        .await
        .expect("l'annulation doit être acceptée");

    // 3) Relecture : l'invocation sort de l'état en cours.
    let statut_final = ligne_terminée(&stack, &handle.invocation_id).await;
    println!("mesure #80 : statut après annulation : {statut_final:?}");
    assert!(
        !statut_final.status.to_ascii_lowercase().contains("running"),
        "l'annulation doit faire sortir du running : {statut_final:?}"
    );

    // 4) Idempotence mesurée : re-annuler une invocation terminée rend `Ok(())`.
    cancel_invocation(&stack.config, &stack.http, &handle.invocation_id)
        .await
        .expect("mesure #80 : re-annuler une invocation terminée doit être idempotent (Ok)");

    // 5) Identifiant bien formé mais inconnu (dernier caractère muté, grammaire et codec
    // préservés) → absent du résultat, jamais une erreur ni une ligne inventée.
    let mut caracteres: Vec<char> = handle.invocation_id.chars().collect();
    let derniere = caracteres
        .last_mut()
        .expect("un identifiant d'invocation n'est pas vide");
    *derniere = if *derniere == 'q' { 'z' } else { 'q' };
    let inconnu: String = caracteres.into_iter().collect();
    let rendu = invocation_statuses(&stack.config, &stack.http, std::slice::from_ref(&inconnu))
        .await
        .expect("mesure #80 : un identifiant inconnu bien formé doit être une absence, pas une erreur");
    assert!(
        rendu.iter().all(|statut| statut.id != inconnu),
        "l'identifiant inconnu {inconnu} doit être absent du résultat : {rendu:?}"
    );

    // 6) Liste filtrée par service : l'invocation du run y figure.
    let filtre = InvocationFilter {
        service: Some("DagInterpreter".to_string()),
        handler: None,
        status: None,
        limit: Some(100),
    };
    let liste = list_invocations(&stack.config, &stack.http, &filtre)
        .await
        .expect("la liste filtrée doit réussir");
    assert!(
        liste.iter().any(|statut| statut.id == handle.invocation_id),
        "l'invocation doit figurer dans la liste filtrée par service : {}",
        liste.len()
    );
    println!(
        "mesure #80 : liste service=DagInterpreter → {} invocations, première {:?}",
        liste.len(),
        liste.first()
    );

    // 7) Journal d'un step ordinaire (run nommé `step:L`, ordonné par `index`) — scenario
    // « le journal nomme les étapes » lu par `invocation_journal` ([!] doc, test précédent).
    let dag_court = DagSteps(vec![step(
        "L",
        &[],
        "record",
        json!({ "id": "L", "sleep_ms": 200 }),
    )]);
    let (status, body) = stack.run_to_completion(&dag_court, None).await;
    assert_eq!(status, 200, "corps : {body}");
    // [!] mesuré 2026-11-15 (restate 1.8.1) : le step ordinaire `L` n'a qu'un run nommé,
    // `step:L` (rendu à l'intérieur, « un seul ctx.run() par invocation » de ./dispatcher.sdd) ;
    // le membre « render:L puis step:L » du Scenario est en attente d'arbitrage spec-dd.
    let noms = noms_d_un_journal_de_step(&stack, "step:L").await;
    println!("mesure #80 : journal du step L — noms {noms:?}");
    assert!(
        noms.iter().any(|n| n == "step:L"),
        "le run d'exécution du step L doit être nommé step:L : {noms:?}"
    );
}

// ─────────────── Awakeables (issue #26, `src/workflow/durable.sdd`, 2026-11-14) ───────────────
//
// Formes de l'ingress issues de la doc `restate-sdk` `0.12.1` et **confirmées mesurées contre le
// `restate-server` de la DevStation par les tests ci-dessous** (rapport de tâche #26) :
// - résolution : `POST {ingress}/restate/awakeables/{id}/resolve`, `content-type:
//   application/json`, corps = la valeur JSON nue ;
// - rejet : `POST {ingress}/restate/awakeables/{id}/reject`, `content-type: text/plain`, corps =
//   le message nu ;
// - l'identifiant a la forme `prom_1` + base64url (URL-safe sans padding).
// Les statuts des chemins nominal, tardif et doublon sont mesurés (lignes `mesure #26 :`) et
// consignés dans le rapport de tâche avec la version de Restate.

/// Identifiant de la première remise `awakeable:<id>` du journal-fichier (attendu 60 s).
async fn premier_identifiant_awakeable(chemin: &std::path::Path) -> String {
    attendre_lignes(chemin, "une remise d'awakeable", |lignes| {
        lignes.iter().any(|ligne| ligne.starts_with("awakeable:"))
    })
    .await;
    lire_journal(chemin)
        .into_iter()
        .find_map(|ligne| ligne.strip_prefix("awakeable:").map(str::to_string))
        .expect("au moins une remise awakeable:<id> est présente")
}

/// Nombre de remises d'awakeable (`awakeable:<id>`) déjà écrites dans le journal-fichier — la
/// mémoïsation de `run_effect` impose qu'un step n'en produise qu'une, même après rejeu.
fn remises_awakeable(chemin: &std::path::Path) -> usize {
    lire_journal(chemin)
        .iter()
        .filter(|ligne| ligne.starts_with("awakeable:"))
        .count()
}

/// Résolution nue par l'ingress — sonde de MESURE (statut et corps texte rendus pour consignation,
/// jamais consommés par du code de production) : `POST {ingress}/restate/awakeables/{id}/resolve`,
/// `content-type: application/json`, corps = valeur JSON nue.
async fn résoudre_par_ingress_brut(
    http: &reqwest::Client,
    ingress_url: &str,
    identifiant: &str,
    valeur: &Value,
) -> (u16, String) {
    let réponse = http
        .post(format!("{ingress_url}/restate/awakeables/{identifiant}/resolve"))
        .header("content-type", "application/json")
        .body(valeur.to_string())
        .send()
        .await
        .expect("la résolution doit atteindre l'ingress");
    let statut = réponse.status().as_u16();
    let corps = réponse.text().await.unwrap_or_default();
    (statut, corps)
}

/// Rejet nu par l'ingress — sonde de MESURE, même forme que [`résoudre_par_ingress_brut`] avec
/// `content-type: text/plain` et le message en corps.
async fn rejeter_par_ingress_brut(
    http: &reqwest::Client,
    ingress_url: &str,
    identifiant: &str,
    message: &str,
) -> (u16, String) {
    let réponse = http
        .post(format!("{ingress_url}/restate/awakeables/{identifiant}/reject"))
        .header("content-type", "text/plain")
        .body(message.to_string())
        .send()
        .await
        .expect("le rejet doit atteindre l'ingress");
    let statut = réponse.status().as_u16();
    let corps = réponse.text().await.unwrap_or_default();
    (statut, corps)
}

/// Résolution contractuelle par le client livré par la tâche « Issue #26 (awakeable) » de
/// `client.sdd` (`workflow::client::resolve_awakeable`, ré-exporté à plat) — les tests l'emploient
/// depuis sa livraison ; les sondes brutes ci-dessus restent la sonde de MESURE des statuts.
async fn résoudre_awakeable(config: &WorkflowConfig, http: &reqwest::Client, id: &str, valeur: &Value) {
    resolve_awakeable(config, http, id, valeur)
        .await
        .expect("la résolution par le client doit être acceptée");
}

/// Rejet contractuel par `workflow::reject_awakeable` (voir [`résoudre_awakeable`]).
async fn rejeter_awakeable(config: &WorkflowConfig, http: &reqwest::Client, id: &str, message: &str) {
    reject_awakeable(config, http, id, message)
        .await
        .expect("le rejet par le client doit être accepté");
}

// Formes MESURÉES live (restate-server 1.8.1, podman, 2026-11-15) :
// - identifiants émis par le serveur : préfixe `sign_1` + base64url non vide (observé
//   `sign_1B6o5lXb2fmcBoeB4-9z9AB6TVbCXtyWQAAAAEQ`), pas le `prom_1` de la documentation du SDK.
//   La grammaire anti-injection du client accepte les deux préfixes ; écart `[!]` à la spec
//   `client.sdd`, remonté à la synthèse.
// - resolve nominal : `POST {ingress}/restate/awakeables/{id}/resolve`,
//   `content-type: application/json`, valeur JSON nue → 202 Accepted, corps vide.
// - reject : `POST {ingress}/restate/awakeables/{id}/reject`, `content-type: text/plain`,
//   message nu → 202 Accepted ; le message transite verbatim jusqu'en erreur de step (l'attache
//   le rend tel quel, validé par le test du rejet).
// - résolution tardive (après expiration du délai) → 202 Accepted, silencieux ; le step a déjà
//   échoué côté moteur. Double résolution → 202 Accepted, la seconde ne change rien. Rejet sur un
//   awakeable déjà résolu → 202 Accepted, silencieux. `restate-server` 1.8.1 ne distingue
//   aucune de ces formes au statut : la résolution tardive n'est pas une erreur au sens HTTP.

/// Scenario « un awakeable résolu reprend le step avec la valeur » (`durable.sdd`, 2026-11-14) :
/// le kind de fixture `attend_awakeable` crée l'awakeable, remet l'identifiant par `run_effect`
/// (journal-fichier), attend ; le test lit l'identifiant puis résout `{"ok": 42}` — le run se
/// termine, la sortie du step vaut la valeur résolue, la remise n'a eu lieu qu'une fois.
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn un_awakeable_résolu_reprend_le_step_avec_la_valeur() {
    let stack = Stack::start().await;
    let dag = DagSteps(vec![step("W", &[], "attend_awakeable", json!({}))]);
    let run_key = stack.send_run(&dag, None).await;
    let identifiant = premier_identifiant_awakeable(&stack.journal_awakeable).await;
    résoudre_awakeable(&stack.config, &stack.http, &identifiant, &json!({"ok": 42})).await;
    let (statut, corps) = attache_run(&stack.http, &stack.config.ingress_url, &run_key).await;
    assert_eq!(
        statut, 200,
        "le run de l'awakeable résolu doit mener l'attache au bout : {statut} {corps}"
    );
    assert_eq!(
        corps["W"],
        json!({"ok": 42}),
        "le step rend la valeur résolue : {corps}"
    );
    assert_eq!(
        remises_awakeable(&stack.journal_awakeable),
        1,
        "la remise de l'identifiant, journalisée par run_effect, n'a eu lieu qu'une fois"
    );
}

/// Scenario « un awakeable rejeté fait échouer le step sans retry » (`durable.sdd`) : le rejet
/// `refus externe` fait échouer le run, le message du rejet transite verbatim dans le message
/// d'échec du step, et le step n'est pas rejoué — une seule remise d'identifiant au journal.
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn un_awakeable_rejeté_fait_échouer_le_step_sans_retry() {
    let stack = Stack::start().await;
    let dag = DagSteps(vec![step("W", &[], "attend_awakeable", json!({}))]);
    let run_key = stack.send_run(&dag, None).await;
    let identifiant = premier_identifiant_awakeable(&stack.journal_awakeable).await;
    rejeter_awakeable(&stack.config, &stack.http, &identifiant, "refus externe").await;
    let (statut, corps) = attache_run(&stack.http, &stack.config.ingress_url, &run_key).await;
    assert!(
        statut >= 400,
        "le rejet de l'awakeable doit échouer le run : {statut} {corps}"
    );
    assert!(
        corps.to_string().contains("refus externe"),
        "le message du rejet doit transiter verbatim : {corps}"
    );
    assert_eq!(
        remises_awakeable(&stack.journal_awakeable),
        1,
        "le step en échec non retryable n'est pas rejoué : une seule remise d'identifiant"
    );
}

/// Scenario « l'identifiant d'awakeable survit au rejeu du parent » (`durable.sdd`, reprise) :
/// méthode du spike `workflow#AC-8` — le service est le binaire relancé en sous-processus
/// ([`mode_service_restate`], qui enregistre le kind durable avec journal-fichier), tué d'un
/// SIGKILL **pendant l'attente**, rechargé sur le même port. L'identifiant lu **avant** le crash
/// est celui qui résout après reprise (entrée du journal, jamais recalculé) ; la remise par
/// `run_effect` n'est pas rejouée — le journal-fichier n'en porte qu'une.
#[tokio::test]
#[ignore = "exige podman et l'image restate ; relance le binaire de test en sous-processus"]
async fn l_identifiant_d_awakeable_survit_au_rejeu_du_parent() {
    let (_container, ingress, admin, app) = lancer_conteneur();
    let http = reqwest::Client::new();
    let config = WorkflowConfig {
        admin_url: format!("http://127.0.0.1:{admin}"),
        ingress_url: format!("http://127.0.0.1:{ingress}"),
        deployment_url: format!("http://127.0.0.1:{app}"),
    };
    let journal = std::env::temp_dir().join(format!("miryad-restate-awakeable-kill-{app}.log"));
    let _ = std::fs::remove_file(&journal);

    let mut service = lancer_service(app, &journal, Duration::from_secs(90));
    attendre_port(app, "le service (génération 1)").await;
    attendre_admin_et_enregistrer(&http, &config).await;

    let dag = DagSteps(vec![step("W", &[], "attend_awakeable", json!({}))]);
    let handle = trigger_run(&config, &http, &dag, &serde_json::Map::new(), None)
        .await
        .expect("le déclenchement doit être accepté");

    // Identifiant lu AVANT le crash : c'est lui, pas un identifiant recalculé, qui doit résoudre.
    let identifiant = premier_identifiant_awakeable(&journal).await;

    service.0.kill().expect("le service doit être tuable");
    let statut_kill = service.0.wait().expect("le service tué doit être récupéré");
    assert!(
        statut_kill.code().is_none(),
        "le service doit avoir été tué par signal, pas sorti proprement : {statut_kill:?}"
    );
    let _service_rechargé = lancer_service(app, &journal, Duration::from_secs(90));
    attendre_port(app, "le service rechargé (génération 2)").await;

    résoudre_awakeable(&config, &http, &identifiant, &json!({"reprise": true})).await;
    let (statut_attache, corps) = attache_run(&http, &config.ingress_url, &handle.run_key).await;
    assert_eq!(
        statut_attache, 200,
        "la reprise doit mener le run au bout avec la valeur résolue : {statut_attache} {corps}"
    );
    assert_eq!(corps["W"], json!({"reprise": true}), "sortie du step W : {corps}");

    let lignes = lire_journal(&journal);
    assert_eq!(
        lignes
            .iter()
            .filter(|ligne| ligne.starts_with("awakeable:"))
            .count(),
        1,
        "la remise journalisée avant le crash n'est pas rejouée par la génération 2 : {lignes:?}"
    );
    let relu = lignes
        .iter()
        .find_map(|ligne| ligne.strip_prefix("awakeable:"))
        .expect("la remise est présente au journal-fichier");
    assert_eq!(
        relu, identifiant,
        "l'identifiant relu après reprise est identique"
    );
    let _ = std::fs::remove_file(&journal);
}

/// Scenario « un awakeable à délai échoit sans résolution » (`durable.sdd`) :
/// `attend_awakeable_delai` avec 2 s, jamais résolu — le step échoue en `StepError` non retryable
/// dont le message porte `AWAKEABLE_TIMEOUT_MESSAGE` (« awakeable timed out », verbatim — le kind
/// distingue le délai en comparant la constante, ./durable.sdd) ; le run n'est pas rejoué.
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn un_awakeable_à_délai_échoit_sans_résolution() {
    let stack = Stack::start().await;
    let dag = DagSteps(vec![step(
        "W",
        &[],
        "attend_awakeable_delai",
        json!({ "delai_s": 2 }),
    )]);
    let (statut, corps) = stack.run_to_completion(&dag, None).await;
    assert!(
        statut >= 400,
        "le délai échu sans résolution doit échouer le run : {statut} {corps}"
    );
    assert!(
        corps.to_string().contains("awakeable timed out"),
        "le message de l'échec porte AWAKEABLE_TIMEOUT_MESSAGE : {corps}"
    );
    assert_eq!(
        remises_awakeable(&stack.journal_awakeable),
        1,
        "le step en échec non retryable n'est pas rejoué"
    );
}

/// Scenario « un awakeable à délai résolu à temps rend sa valeur » (`durable.sdd`) : délai de
/// 30 s, résolution aussitôt — le step rend la valeur sans attendre l'échéance (durée mesurée,
/// seuil 20 s largement sous les 30 s du délai).
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn un_awakeable_à_délai_résolu_à_temps_rend_sa_valeur() {
    let stack = Stack::start().await;
    let debut = Instant::now();
    let dag = DagSteps(vec![step(
        "W",
        &[],
        "attend_awakeable_delai",
        json!({ "delai_s": 30 }),
    )]);
    let run_key = stack.send_run(&dag, None).await;
    let identifiant = premier_identifiant_awakeable(&stack.journal_awakeable).await;
    résoudre_awakeable(&stack.config, &stack.http, &identifiant, &json!({"gagné": true})).await;
    let (statut, corps) = attache_run(&stack.http, &stack.config.ingress_url, &run_key).await;
    assert_eq!(
        statut, 200,
        "l'attente gagnante doit mener le run au bout : {statut} {corps}"
    );
    assert_eq!(corps["W"], json!({"gagné": true}), "sortie du step W : {corps}");
    assert!(
        debut.elapsed() < Duration::from_secs(20),
        "la résolution à temps rend la valeur sans attendre l'échéance de 30 s : {:?}",
        debut.elapsed()
    );
}

/// Scenario « une résolution tardive ou double est mesurée et consignée » (`durable.sdd`) : les
/// statuts Rendus par les deux sondes `résoudre_par_ingress_brut` sont imprimés (`mesure #26 :`),
/// mesurés contre le `restate-server` de la `DevStation` et consignés dans le rapport de tâche avec
/// la version de Restate ; le Then du Scenario — aucun effet observable sur le run terminé — est
/// verrouillé ici comme contrat de régression, quel que soit le code mesuré.
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn une_résolution_tardive_ou_double_est_mesurée_et_consignée() {
    let stack = Stack::start().await;

    // ————— volet 1 : awakeable échu par son délai, résolu après échéance.
    let dag_délai = DagSteps(vec![step(
        "T",
        &[],
        "attend_awakeable_delai",
        json!({ "delai_s": 2 }),
    )]);
    let run_key_délai = stack.send_run(&dag_délai, None).await;
    let identifiant_délai = premier_identifiant_awakeable(&stack.journal_awakeable).await;
    let (statut_échéance, corps_échéance) =
        attache_run(&stack.http, &stack.config.ingress_url, &run_key_délai).await;
    assert!(
        statut_échéance >= 400 && corps_échéance.to_string().contains("awakeable timed out"),
        "l'échéance doit d'abord être observée : {statut_échéance} {corps_échéance}"
    );
    let (statut_tardif, corps_tardif) = résoudre_par_ingress_brut(
        &stack.http,
        &stack.config.ingress_url,
        &identifiant_délai,
        &json!({"trop": "tard"}),
    )
    .await;
    println!(
        "mesure #26 : résolution tardive (awakeable échu) → statut {statut_tardif}, corps {corps_tardif:?}"
    );
    let (re_vu_délai, _) = attache_run(&stack.http, &stack.config.ingress_url, &run_key_délai).await;
    assert_eq!(
        re_vu_délai, statut_échéance,
        "aucun effet observable de la résolution tardive sur le run terminé par l'échéance"
    );

    // ————— volet 2 : awakeable résolu, puis résolu une seconde fois.
    // Journal remis à zéro pour que `premier_identifiant_awakeable` vise la remise du volet 2.
    std::fs::write(&stack.journal_awakeable, b"").expect("le journal-fichier doit être remis à vide");
    let dag_simple = DagSteps(vec![step("R", &[], "attend_awakeable", json!({}))]);
    let run_key_simple = stack.send_run(&dag_simple, None).await;
    let identifiant_simple = premier_identifiant_awakeable(&stack.journal_awakeable).await;
    résoudre_awakeable(&stack.config, &stack.http, &identifiant_simple, &json!({"x": 1})).await;
    let (statut_premier, corps_premier) =
        attache_run(&stack.http, &stack.config.ingress_url, &run_key_simple).await;
    assert_eq!(
        statut_premier, 200,
        "le run de l'awakeable résolu doit aboutir avant la double résolution : {statut_premier} {corps_premier}"
    );
    let (statut_double, corps_double) = résoudre_par_ingress_brut(
        &stack.http,
        &stack.config.ingress_url,
        &identifiant_simple,
        &json!({"bis": true}),
    )
    .await;
    println!(
        "mesure #26 : double résolution (awakeable résolu) → statut {statut_double}, corps {corps_double:?}"
    );
    let (statut_rejet_sur_résolu, corps_rejet) = rejeter_par_ingress_brut(
        &stack.http,
        &stack.config.ingress_url,
        &identifiant_simple,
        "refus après résolution",
    )
    .await;
    println!(
        "mesure #26 : rejet sur un awakeable déjà résolu → statut {statut_rejet_sur_résolu}, corps {corps_rejet:?}"
    );
    let (re_vu_simple, _) = attache_run(&stack.http, &stack.config.ingress_url, &run_key_simple).await;
    assert_eq!(
        re_vu_simple, 200,
        "aucun effet observable de la double résolution sur le run terminé en succès"
    );
}

/// Scenario « un step ignoré ne figure pas dans la table de sortie du run » et « `run_if_skipped`
/// fait s'exécuter le dépendant malgré l'ignorance » (`src/workflow/interpreter.sdd`, #84,
/// intégration AC-16 arbitrée le 2026-11-18), contre un vrai `restate-server` : DAG `A → B → C`,
/// le `when` de `B` lit `/statut_inexistant` dans sa sortie — faux, `B` est ignoré, `C` par
/// propagation. Table de sortie réduite à `A`, ni `B` ni `C` dispatchés (aucun `start:` au
/// journal). Second run, même pile, avec `run_if_skipped: true` sur `C` : `C` s'exécute et lit
/// `null` pour `B` (l'entrée existe, à `Value::Null` — `build_inputs` de ./interpreter.sdd),
/// `B` reste sans trace.
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn ac16_l_ignorance_de_b_propage_a_c_puis_run_if_skipped_sur_c_la_franchit() {
    let stack = Stack::start().await;
    let quand_faux = json!({ "step": "A", "path": "/statut_inexistant", "op": "eq", "value": "ok" });

    // ————— Run 1 : `B` ignoré par son `when`, `C` par propagation → sortie réduite à `A`.
    let mut b = step("B", &["A"], "record", json!({ "id": "B" }));
    b.when = Some(
        serde_json::from_value(quand_faux.clone()).expect("la fixture `when` est un prédicat désérialisable"),
    );
    let dag = DagSteps(vec![
        step("A", &[], "record", json!({ "id": "A" })),
        b,
        step("C", &["B"], "record", json!({ "id": "C" })),
    ]);
    let (status, body) = stack.run_to_completion(&dag, None).await;
    assert_eq!(
        status, 200,
        "un run dont toute l'aval est ignoré doit mener l'attache au bout : {body}"
    );
    let results = body.as_object().expect("table id → sortie");
    assert_eq!(
        results.len(),
        1,
        "table de sortie réduite à `A` — `B` et `C` n'y ont aucune clé : {body}"
    );
    assert_eq!(body["A"]["id"], "A", "l'unique step exécuté est `A` : {body}");
    let journal = stack.journal();
    assert!(
        !journal.iter().any(|e| e == "start:B") && !journal.iter().any(|e| e == "start:C"),
        "ni `B` (`when` faux) ni `C` (propagation) ne devaient être dispatchés : {journal:?}"
    );

    // ————— Run 2 : `run_if_skipped: true` sur `C` — `C` s'exécute et lit `null` pour `B`.
    let mut b_bis = step("B", &["A"], "record", json!({ "id": "B" }));
    b_bis.when =
        Some(serde_json::from_value(quand_faux).expect("la fixture `when` est un prédicat désérialisable"));
    let mut c = step("C", &["B"], "record", json!({ "id": "C" }));
    c.run_if_skipped = true;
    let dag_bis = DagSteps(vec![step("A", &[], "record", json!({ "id": "A" })), b_bis, c]);
    let (status, body) = stack.run_to_completion(&dag_bis, None).await;
    assert_eq!(
        status, 200,
        "le run avec `run_if_skipped` doit mener l'attache au bout : {body}"
    );
    let results = body.as_object().expect("table id → sortie");
    assert_eq!(
        results.len(),
        2,
        "`A` et `C` seulement — `B` reste ignoré : {body}"
    );
    assert_eq!(
        body["C"]["inputs"],
        json!({ "B": null }),
        "`C` doit lire `null` pour le step ignoré `B` — l'entrée existe : {body}"
    );
    let journal = stack.journal();
    assert_eq!(
        journal.iter().filter(|e| e.as_str() == "start:B").count(),
        0,
        "`B` n'a été dispatché dans aucun des deux runs : {journal:?}"
    );
    assert_eq!(
        journal.iter().filter(|e| e.as_str() == "start:C").count(),
        1,
        "`C` s'exécute une fois, dans le second run : {journal:?}"
    );
}

/// Scenario « un step lit l'export d'un ancêtre non direct », « l'export d'un producteur ignoré
/// est absent et ne propage rien » (`src/workflow/interpreter.sdd`, issue #85, intégration AC-17
/// arbitrée le 2026-11-18), contre un vrai `restate-server` : DAG `A → B → C` où `A` déclare
/// l'export `verdict` (`/v`) et `C`, qui ne dépend que de `B`, porte `when: {export: verdict,
/// op: eq, value: "ok"}` — `C` s'exécute quand `A` rend `{"v": "ok"}` (run 1) et est ignoré
/// quand `A` rend `{"v": "ko"}` (run 2), sans que `B` le sache. Run 3 : `A` est ignoré par son
/// propre `when` (une `var` absente), donc son export n'est jamais résolu ; `B` s'exécute
/// (`run_if_skipped`), `C` lit un export absent — `when` faux, ignorant — tandis que `D`, sans
/// `when`, qui ne dépend que de `B` non ignoré, s'exécute : un export n'est pas une `depends_on`,
/// il ne propage pas l'ignorance.
#[tokio::test]
#[ignore = "exige podman et l'image restate"]
async fn ac17_un_export_se_lit_en_aval_de_son_producteur_et_absent_s_il_est_ignore() {
    let stack = Stack::start().await;
    let quand_export = |value: &str| json!({ "export": "verdict", "op": "eq", "value": value });
    let producteur = |id: &str, sortie: Value| {
        let mut a = step("A", &[], "rendu", json!({ "id": id, "output": sortie }));
        a.exports = std::collections::BTreeMap::from([("verdict".to_string(), "/v".to_string())]);
        a
    };
    let etape_qui_lit = |value: &str| {
        let mut c = step("C", &["B"], "record", json!({ "id": "C" }));
        c.when = Some(
            serde_json::from_value(quand_export(value))
                .expect("la fixture `when` est un prédicat désérialisable"),
        );
        c
    };

    // ————— Run 1 : `A` rend `{"v": "ok"}` — l'export se lit en aval non direct, `C` s'exécute.
    let dag = DagSteps(vec![
        producteur("A", json!({ "v": "ok" })),
        step("B", &["A"], "record", json!({ "id": "B" })),
        etape_qui_lit("ok"),
    ]);
    let (status, body) = stack.run_to_completion(&dag, None).await;
    assert_eq!(
        status, 200,
        "un run dont l'export vérifie le `when` doit aboutir : {body}"
    );
    assert_eq!(
        body["C"]["id"], "C",
        "`C` s'exécute quand l'export vaut `\"ok\"` : {body}"
    );
    let journal = stack.journal();
    assert!(
        journal.iter().any(|e| e == "start:C"),
        "`C` doit avoir été dispatché au moins une fois : {journal:?}"
    );

    // ————— Run 2 : `A` rend `{"v": "ko"}` — `C` est ignoré par son `when`, `B` indemne.
    let avant_deux = stack.journal().len();
    let dag_deux = DagSteps(vec![
        producteur("A-deux", json!({ "v": "ko" })),
        step("B", &["A"], "record", json!({ "id": "B" })),
        etape_qui_lit("ok"),
    ]);
    let (status, body) = stack.run_to_completion(&dag_deux, None).await;
    assert_eq!(
        status, 200,
        "un run dont un step est ignoré doit aboutir : {body}"
    );
    let results = body.as_object().expect("table id → sortie");
    assert!(
        results.contains_key("A") && results.contains_key("B") && !results.contains_key("C"),
        "`A` et `B` seulement — `C` lit `\"ko\"` et est ignoré par son `when` : {body}"
    );
    let journal = stack.journal();
    assert_eq!(
        journal[avant_deux..]
            .iter()
            .filter(|e| e.as_str() == "start:C")
            .count(),
        0,
        "aucun dispatch de `C` dans le run 2 : {:?}",
        &journal[avant_deux..]
    );

    // ————— Run 3 : `A` ignoré par son `when` → export jamais résolu ; `B` (`run_if_skipped`)
    // s'exécute ; `C`, qui lit cet export absent, est ignoré par son `when` ; `D`, sans `when`
    // sur la seule dépendance `B` non ignorée, s'exécute — aucune propagation par l'export.
    let avant_trois = stack.journal().len();
    let mut a_ignore = producteur("A-trois", json!({ "v": "ok" }));
    a_ignore.when = Some(
        serde_json::from_value(json!({ "var": "absente-de-vars", "op": "exists" }))
            .expect("la fixture `when` est un prédicat désérialisable"),
    );
    let mut b = step("B", &["A"], "record", json!({ "id": "B" }));
    b.run_if_skipped = true;
    let dag_trois = DagSteps(vec![
        a_ignore,
        b,
        etape_qui_lit("ok"),
        step("D", &["B"], "record", json!({ "id": "D" })),
    ]);
    let (status, body) = stack.run_to_completion(&dag_trois, None).await;
    assert_eq!(status, 200, "le run du producteur ignoré doit aboutir : {body}");
    let results = body.as_object().expect("table id → sortie");
    assert_eq!(results.len(), 2, "exactement `B` et `D` en sortie : {body}");
    assert!(
        results.contains_key("B") && results.contains_key("D"),
        "`B` et `D` s'exécutent — l'export absent ne propage rien : {body}"
    );
    assert!(
        !results.contains_key("A") && !results.contains_key("C"),
        "`A` ignoré par son `when`, `C` par le sien (export absent) : {body}"
    );
    let journal = stack.journal();
    let nouveau: Vec<&str> = journal[avant_trois..].iter().map(String::as_str).collect();
    assert!(
        !nouveau.contains(&"start:A-trois") && !nouveau.contains(&"start:C"),
        "ni `A-trois` (ignoré) ni `C` (export absent) ne sont dispatchés dans le run 3 : {nouveau:?}"
    );
}
