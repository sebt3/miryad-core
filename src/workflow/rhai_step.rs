//! Kind natif `"rhai"` (feature `workflow`) : exécution d'un script Rhai via
//! [`vynil_core::engine::Script`], seul `impl` de [`MiryadWorkflowStep`] fourni par la crate.
//!
//! Le script reçoit trois variables — `inputs` (les sorties des steps amont déclarés, telles que
//! livrées par le trait), `config` (le `config` complet du step, `script` compris) et `run`
//! (l'identité du run) — ainsi que la fonction `render` (rendu Handlebars du même contexte que
//! `./template.sdd`). Le script doit se réduire à une `Map` Rhai, rendue telle quelle en JSON. Tout échec (config mal formé, erreur
//! d'exécution Rhai, panic interne) est un [`StepError`] `retryable: false` : rejouer un script
//! qui a échoué rendrait le même résultat.
//!
//! L'évaluation Rhai est synchrone et son moteur vit exclusivement dans une tâche
//! `tokio::task::spawn_blocking` : un script lent ne doit pas monopoliser le thread de travail
//! qui sert les autres invocations concurrentes. À l'intérieur de cette tâche, l'**évaluation**
//! est déléguée à un fil dédié à pile dimensionnée (`SCRIPT_STACK_SIZE`, arbitrage `OI-9`
//! rendu le 2026-11-01, `DEC-10`) : le plafond de 512 niveaux d'appels du moteur est ainsi
//! épuisé avant la pile native, et une récursion profonde rend l'erreur `Stack overflow` de rhai
//! au lieu d'avorter le processus. Chaque exécution est aussi bornée en durée
//! (`timeout_secs`, FR-18) : à l'expiration le moteur annule le script de façon coopérative et
//! rend `MRD-WORKFLOW-009` sans attendre la fin du fil. Aucun `Script` n'est porté par [`RhaiStep`] —
//! chaque `run` construit son moteur neuf, aucune variable d'une invocation ne fuit vers
//! l'autre.
//!
//! Ce fichier ne connaît ni accès DB, ni appel HTTP, ni client MCP. Son moteur Rhai est construit
//! par liste blanche explicite (amendement 2026-10-30, `DEC-8`) : six registrars purs de
//! `vynil-core`, quatre fonctions `log_*` propres au step, les shims d'`add_common` et `render` —
//! jamais `get_env`, les fichiers ou le shell, quelles que soient les features de `vynil-core`
//! activées ailleurs dans le graphe. Le registrar `sleep` de `vynil-core` n'est pas enregistré,
//! mais `sleep` reste appelable : substrat du `lang_core` de rhai accepté le 2026-11-01 (`DEC-8`).
//! Le point d'extension est
//! [`RhaiStep::with_setup`] (issue sebt3/miryad-core#27) : l'application empile des closures
//! qui, à chaque exécution, enregistrent leurs fonctions hôte sur le moteur neuf — jamais un
//! besoin additionnel codé en dur ici.
//!
//! [`MiryadWorkflowStep`]: super::step::MiryadWorkflowStep
//! [`StepError`]: super::step::StepError

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use vynil_core::cancel::{self, CancelToken};
use vynil_core::chrono::chrono_rhai_register;
use vynil_core::engine::{
    Dynamic, Engine, FileModuleResolver, ImmutableString, ModuleResolversCollection, Scope, Script,
    core_pure_rhai_register,
};
use vynil_core::glob::glob_rhai_register;
use vynil_core::hashes::hashes_rhai_register;
use vynil_core::semver::semver_rhai_register;
use vynil_core::yaml::yaml_rhai_register;
use vynil_core::{RhaiRes, rhai_err_str};

use super::error::WorkflowError;
use super::step::{MiryadWorkflowStep, RunInfo, StepError};
use super::template::render_str;

/// Plafond d'opérations Rhai par exécution : `spawn_blocking` isole le thread de travail mais ne
/// peut pas interrompre une tâche bloquante — sans ce plafond, un `loop {}` occuperait un thread
/// du pool bloquant indéfiniment (`./rhai_step.sdd` `Must`, étape 2).
const MAX_OPERATIONS: u64 = 10_000_000;

/// Durée d'exécution par défaut d'un script, en secondes, quand `timeout_secs` est absent
/// (`./rhai_step.sdd` « Durée d'exécution », `FR-18`, valeur confirmée par le développeur
/// principal le 2026-11-01).
const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Plafond de `timeout_secs`, en secondes (`./rhai_step.sdd` « Durée d'exécution », `FR-18`,
/// valeur confirmée par le développeur principal le 2026-11-01) : les valeurs valides sont
/// `1..=MAX_TIMEOUT_SECS`.
const MAX_TIMEOUT_SECS: u64 = 3600;

/// Taille de pile du fil dédié d'évaluation, en octets (`./rhai_step.sdd` « Fil d'exécution du
/// script », arbitrage `dsd/miryad-core/workflow.md` `OI-9` rendu le 2026-11-01, `DEC-10`) :
/// 256 Mio réservés (engagés à la touche), soit deux fois le pire cas de 125 Mio mesuré par
/// `lymaille` en release à 512 niveaux d'appels. Sur la pile par défaut (~2 Mio) du fil
/// `spawn_blocking`, la récursion avortait le processus dès 144 niveaux en debug et entre 256
/// et 384 en release (mesure A6 du 2026-11-01) ; cette pile garantit que
/// `set_max_call_levels(512)` — la limite logique — s'épuise avant la pile native.
const SCRIPT_STACK_SIZE: usize = 256 * 1024 * 1024;

/// Fermeture d'extension posée par [`RhaiStep::with_setup`] : le type exact des éléments de
/// `RhaiStep::setups`, dicté par `./rhai_step.sdd` `Must` — elle reçoit le `Script` neuf de
/// l'exécution, puis `config` et `inputs` en lecture seule. Alias privé de lisibilité — le
/// contrat est ce type, nommé ou non.
type SetupFn = dyn Fn(&mut Script, &Value, &HashMap<String, Value>) + Send + Sync + 'static;

/// Échec de la **mécanique** du fil dédié d'évaluation (`./rhai_step.sdd` « Fil d'exécution du
/// script », 2026-11-01), distinct d'une erreur Rhai : traduit à l'étape 4 sous le message
/// `rhai task panicked: {e}` où `{e}` est le `Display` **brut** de l'erreur sous-jacente — une
/// seule forme de message, aucune prose ajoutée, comme pour un `JoinError`.
enum EvalThreadError {
    /// Panique dans le fil d'évaluation : le sender est tombé sans rien émettre, le canal est
    /// fermé sans valeur (`Handles`, jointure par canal `mpsc`). Rendu : `Display` de
    /// `mpsc::RecvError`.
    ChannelClosed(mpsc::RecvError),
    /// Le fil d'évaluation n'a pas pu être créé (ressource système) : plié dans le même rendu
    /// d'étape 4, la spec ne lui donne pas de message propre et le script n'a pas été évalué.
    /// Rendu : `Display` de l'`io::Error` brut.
    Spawn(std::io::Error),
}

impl std::fmt::Display for EvalThreadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ChannelClosed(e) => write!(f, "{e}"),
            Self::Spawn(e) => write!(f, "{e}"),
        }
    }
}

/// Le `config` du kind `"rhai"`, désérialisé depuis le JSON opaque livré à
/// [`MiryadWorkflowStep::run`] — deux champs attendus (`script`, `timeout_secs`), les autres clés
/// restent accessibles au script via la variable `config` (le JSON complet lui est exposé,
/// `script` compris).
#[derive(Deserialize)]
struct RhaiConfig {
    /// Texte source Rhai évalué par [`Script::eval_map_json`].
    script: String,
    /// Durée d'exécution du script, en secondes (`./rhai_step.sdd` « Durée d'exécution »,
    /// `FR-18`) : absent ⇒ [`DEFAULT_TIMEOUT_SECS`] ; hors `1..=MAX_TIMEOUT_SECS` ⇒ `StepError`
    /// non retryable, vérifié avant toute évaluation.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// Kind de step `"rhai"` : exécute le script porté par le `config` du step via
/// [`vynil_core::engine::Script`].
///
/// L'application consommatrice enregistre elle-même cette instance dans son
/// `super::step::StepRegistry` (aucun auto-enregistrement). Deux champs, posés à la construction
/// et jamais réécrits : `resolver_path` (chemins recherchés par les `import` Rhai) et `setups`
/// (fermetures d'extension posées par [`Self::with_setup`], vides par défaut). Aucun `Script` n'y
/// survit entre deux appels — chaque `run` construit son moteur neuf.
pub struct RhaiStep {
    resolver_path: Vec<String>,
    setups: Vec<Arc<SetupFn>>,
}

impl RhaiStep {
    /// Construit un kind `"rhai"` autorisant l'`import` de modules Rhai depuis `resolver_path`
    /// (un `FileModuleResolver` par chemin, dans l'ordre, posés par `build_script`). Décision
    /// explicite de l'app : miryad-core n'ajoute lui-même aucun chemin (`Default` rend la liste
    /// vide, tout `import` échoue alors en erreur catégorisée, jamais en erreur brute). N'ajoute
    /// aucune closure de setup — le comportement est celui d'avant `with_setup`.
    #[must_use]
    pub fn new(resolver_path: Vec<String>) -> Self {
        Self {
            resolver_path,
            setups: Vec::new(),
        }
    }

    /// Empile une closure d'extension appelée à **chaque** exécution, dans le
    /// `spawn_blocking` de `run`, sur le `Script` neuf de l'exécution — après la construction du
    /// moteur par liste blanche (`build_script`), la pose du plafond d'opérations et les
    /// `set_dynamic` de `inputs`/`config`, avant `eval_map_json` : la closure voit les deux
    /// variables déjà posées et peut enregistrer des fonctions hôte sur `script.engine` (issue
    /// sebt3/miryad-core#27, `./rhai_step.sdd` `Must`).
    ///
    /// **Cumulatif, dans l'ordre d'enregistrement** : plusieurs `with_setup` n'écrasent jamais
    /// l'un l'autre ; une fonction Rhai enregistrée sous le même nom par deux closures suit la
    /// règle du moteur Rhai (la dernière enregistrée prévaut). `new` et `Default` n'ajoutent
    /// aucune closure.
    ///
    /// La closure reçoit `config` et `inputs` en lecture seule : à l'app d'en déduire le
    /// contexte des fonctions qu'elle enregistre (identité, projet…). Elle est rappelée sur un
    /// `Script` neuf à chaque `run` ; un état capturé (`Arc<Mutex<..>>`…) reste la
    /// responsabilité de l'app.
    ///
    /// # Code hôte `async` : `block_on` sur un runtime multi-thread
    ///
    /// Une fonction Rhai est **synchrone**. Une closure qui doit appeler du code hôte `async`
    /// capture un [`tokio::runtime::Handle`] (obtenu hors de la closure, dans le contexte du
    /// runtime) et y fait `block_on` dans la fonction enregistrée : c'est légitime ici car la
    /// closure s'exécute sur un thread du **pool bloquant** de `spawn_blocking`, jamais sur un
    /// thread de travail — à condition que le runtime soit **multi-thread** (un `block_on` sur
    /// un runtime current-thread depuis son propre thread de travail panique ou deadlocke).
    ///
    /// # Plafond d'opérations : surcharge à la charge de l'app
    ///
    /// `MAX_OPERATIONS` n'a pas de paramètre dédié : la closure reçoit `&mut Script` et peut
    /// appeler `script.engine.set_max_operations(..)` elle-même — une surcharge possible mais
    /// volontaire, à la charge de l'app, jamais un défaut de la crate (`./rhai_step.sdd`
    /// `Must`, arbitrage 2026-10-04).
    ///
    /// # Durée d'exécution : une closure non coopérative peut survivre au timeout
    ///
    /// Chaque exécution est bornée en durée par `timeout_secs` (`./rhai_step.sdd` « Durée
    /// d'exécution », `FR-18`) et s'exécute dans une portée d'annulation coopérative de
    /// `vynil-core`. À l'expiration, `run` annule le jeton et rend l'erreur `MRD-WORKFLOW-009`
    /// **sans attendre la fin du fil** : une closure non coopérative — qui bloque sans rendre la
    /// main au moteur — peut donc survivre à cette erreur dans son fil du pool bloquant jusqu'à
    /// ce qu'elle rende la main elle-même. C'est un risque d'app, pas une garantie de la crate.
    #[must_use]
    pub fn with_setup<F>(mut self, f: F) -> Self
    where
        F: Fn(&mut Script, &Value, &HashMap<String, Value>) + Send + Sync + 'static,
    {
        self.setups.push(Arc::new(f));
        self
    }
}

/// Aucun chemin de résolution par défaut, aucune closure de setup : les capacités d'`import` et
/// de fonctions hôte sont présentes, jamais activées par la crate (décision Sébastien,
/// 2026-09-23 et arbitrage #27 2026-10-04, `./rhai_step.sdd` `Must`).
impl Default for RhaiStep {
    fn default() -> Self {
        Self {
            resolver_path: Vec::new(),
            setups: Vec::new(),
        }
    }
}

/// Construit le `Script` neuf d'une exécution par la liste blanche explicite de
/// `./rhai_step.sdd` (« Construction du moteur », amendement 2026-10-30, `dsd/miryad-core/workflow.md`
/// `DEC-8`, `FR-16`) : `Script` à la main (`Engine::new()`, `Scope::new()`), résolveur de modules =
/// `ModuleResolversCollection` portant un `FileModuleResolver` par entrée de `resolver_path`, dans
/// l'ordre (vide ⇒ tout `import` échoue, fail-closed, comme `new_bare`) ; plafonds d'expression et
/// d'appels de `new_bare` (`256`/`128`, `512`) ; puis, **dans cet ordre exact**, les six registrars
/// purs de `vynil-core` (`core_pure`, `chrono`, `hashes`, `semver`, `yaml`, `glob`), les quatre
/// `log_*` du step ([`register_step_logs`]) et les shims de [`Script::add_common`] (`assert`,
/// `import_run`, `import_template` — leur chemin de saut appelle `log_debug`, enregistré avant).
/// Ne sont **jamais** enregistrés : le groupe hôte (`get_env`, les `log_*` du processus), le
/// registrar `core_sleep` de `vynil-core`, ni les registrars `fs`, `shell`, `password`, `crypto`,
/// `oci`, `http`, `s3`, `k8s` — quelles que soient les features de `vynil-core` activées ailleurs
/// dans le graphe. Un registrar ajouté plus tard par `vynil-core` n'atteint les scripts que par un
/// amendement de la spec ; la liste est relue à chaque montée de `vynil-core`. Substrat accepté
/// (`./rhai_step.sdd` « Liste blanche », arbitrage 2026-11-01) : `sleep(INT)`/`sleep(FLOAT)` est
/// enregistré par rhai lui-même dans son `lang_core` sur tout `Engine::new` et reste donc
/// appelable, bloquant pour le fil de travail.
fn build_script(resolver_path: Vec<String>, run: &RunInfo) -> Script {
    let mut script = Script {
        engine: Engine::new(),
        ctx: Scope::new(),
    };
    let mut resolver = ModuleResolversCollection::new();
    for path in resolver_path {
        resolver.push(FileModuleResolver::new_with_path(path));
    }
    script.engine.set_module_resolver(resolver);
    script.engine.set_max_expr_depths(256, 128);
    script.engine.set_max_call_levels(512);
    core_pure_rhai_register(&mut script.engine);
    chrono_rhai_register(&mut script.engine);
    hashes_rhai_register(&mut script.engine);
    semver_rhai_register(&mut script.engine);
    yaml_rhai_register(&mut script.engine);
    glob_rhai_register(&mut script.engine);
    register_step_logs(&mut script.engine, run);
    script.add_common();
    script
}

/// Enregistre les quatre fonctions Rhai `log_debug`, `log_info`, `log_warn`, `log_error`
/// (`fn(message: string)`, `./rhai_step.sdd` « Liste blanche », `FR-17`) : chacune émet
/// `tracing::debug!`/`info!`/`warn!`/`error!` avec les champs structurés `run_key` et `step_id`
/// (clones capturés de `run`) et le message — le journal est celui du step, pas celui du
/// processus. Elles ne font rien d'autre. `log_debug` est requis par le chemin de saut des shims
/// `import_run`/`import_template` d'[`Script::add_common`], d'où l'enregistrement avant celui-ci.
fn register_step_logs(engine: &mut Engine, run: &RunInfo) {
    let (run_key, step_id) = (run.run_key.clone(), run.step_id.clone());
    engine.register_fn("log_debug", move |message: ImmutableString| {
        tracing::debug!(run_key = %run_key, step_id = %step_id, "{message}");
    });
    let (run_key, step_id) = (run.run_key.clone(), run.step_id.clone());
    engine.register_fn("log_info", move |message: ImmutableString| {
        tracing::info!(run_key = %run_key, step_id = %step_id, "{message}");
    });
    let (run_key, step_id) = (run.run_key.clone(), run.step_id.clone());
    engine.register_fn("log_warn", move |message: ImmutableString| {
        tracing::warn!(run_key = %run_key, step_id = %step_id, "{message}");
    });
    let (run_key, step_id) = (run.run_key.clone(), run.step_id.clone());
    engine.register_fn("log_error", move |message: ImmutableString| {
        tracing::error!(run_key = %run_key, step_id = %step_id, "{message}");
    });
}

#[async_trait::async_trait]
impl MiryadWorkflowStep for RhaiStep {
    fn kind(&self) -> &'static str {
        "rhai"
    }

    fn raw_fields(&self) -> &'static [&'static str] {
        &["script"]
    }

    async fn run(
        &self,
        run: &RunInfo,
        config: Value,
        inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        // Étape 1 — le `config` doit porter `script` ; un config mal formé ne se corrige pas en
        // rejouant (déterministe vis-à-vis de l'entrée).
        let rhai_config = serde_json::from_value::<RhaiConfig>(config.clone()).map_err(|e| StepError {
            message: format!("config rhai invalide: {e}"),
            retryable: false,
        })?;
        // Étape 1 bis — bornes de `timeout_secs` validées APRÈS la désérialisation et AVANT toute
        // évaluation (`./rhai_step.sdd` « Durée d'exécution », `FR-18`, dans l'ordre des branches
        // `Handles`) : absent ⇒ défaut ; hors `1..=MAX_TIMEOUT_SECS` ⇒ erreur de config non
        // retryable, aucun `Script` n'est construit.
        let timeout_secs = rhai_config.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        if !(1..=MAX_TIMEOUT_SECS).contains(&timeout_secs) {
            return Err(StepError {
                message: format!(
                    "config rhai invalide: timeout_secs doit être compris entre 1 et {MAX_TIMEOUT_SECS}"
                ),
                retryable: false,
            });
        }
        // Le `resolver_path` et les `Arc` de `setups` sont clonés avant le `move` : la tâche
        // bloquant le thread ne doit rien à `&self`, et un `Script` neuf est construit à chaque
        // exécution — aucun état ne survit d'un `run` à l'autre.
        let resolver_path = self.resolver_path.clone();
        let setups = self.setups.clone();
        let run = run.clone();
        // Jeton d'annulation coopérative (« Durée d'exécution », `FR-18`) : le clone lu dans le
        // fil par `on_progress` et par le `cancel::scope` de l'évaluation ; l'original, annulé à
        // l'expiration du plafond de durée sous `tokio::time::timeout`, interrompt le fil au
        // prochain point de contrôle sans que `run` l'attende.
        let token = CancelToken::new();
        let eval_token = token.clone();
        // Étape 2 — évaluation hors du thread de travail tokio (le script, fourni par un admin,
        // peut boucler) ; un panic éventuel — y compris une closure de `setups` qui panique — y
        // est confiné et capturé en `JoinError` à l'étape 4 ; une panique du fil dédié
        // d'évaluation l'est par la jointure du canal (même étape, même message).
        let evaluation = tokio::task::spawn_blocking(move || {
            let mut script = build_script(resolver_path, &run);
            script.engine.set_max_operations(MAX_OPERATIONS);
            // Conversion infaillible HashMap→Map (pas de `serde_json::to_value` : aucun Result
            // ici). `config` est exposé en entier, `script` compris.
            script.set_dynamic("inputs", &Value::Object(inputs.clone().into_iter().collect()));
            // Identité du run (2026-10-06, `./rhai_step.sdd` étape 2) : même objet que `run` dans
            // le contexte de `./template.sdd`.
            script.set_dynamic(
                "run",
                &serde_json::json!({
                    "run_key": run.run_key,
                    "step_id": run.step_id,
                    "depth": run.depth,
                }),
            );
            // Variables de run (2026-10-30, lot B, `./rhai_step.sdd` « Variables de run », `FR-14`,
            // `CON-8`) : `run.vars` posée comme variable ORDINAIRE (jamais `push_constant` : une
            // constante map mutée fait paniquer Rhai avec l'optimiseur par défaut), à côté de
            // `inputs` et `run`, avant les closures de `setups`. Objet JSON, `{}` sans variables ;
            // une modification dans un script est locale à l'exécution (moteur neuf à chaque `run`).
            script.set_dynamic("vars", &run.vars);
            script.set_dynamic("config", &config);
            // Fonction `render` (2026-10-06) : enregistrée AVANT les closures de `setups`, pour
            // qu'une closure de l'app puisse la surcharger. Clones de `run` et `inputs` capturés ;
            // erreur de rendu → erreur Rhai d'exécution (non retryable à l'étape 3).
            let render_run = run.clone();
            let render_inputs = inputs.clone();
            script
                .engine
                .register_fn("render", move |template: String| -> RhaiRes<String> {
                    render_str(&template, &render_run, &render_inputs).map_err(|e| rhai_err_str(e.message))
                });
            // Closures d'extension (`with_setup`, #27) : après les `set_dynamic`, avant
            // l'évaluation — elles voient `inputs`/`config` posés et peuvent enregistrer des
            // fonctions hôte sur `script.engine`. Ordre d'enregistrement ; la première qui
            // panique interrompt tout (les suivantes ne tournent pas, le script n'est pas
            // évalué) — `Handles` de ./rhai_step.sdd.
            for setup in &setups {
                setup(&mut script, &config, &inputs);
            }
            // Annulation coopérative (« Durée d'exécution », `FR-18`) : `on_progress` est appelé
            // à chaque point d'opération (arête arrière de boucle, rhai 1.26.1 `Op::Tick`) et
            // rend Some(valeur) pour mettre fin à l'évaluation dès que le jeton est annulé —
            // `ErrorTerminated`, le fil s'arrête de lui-même peu après. Le contrôle du plafond
            // d'opérations est opéré par le moteur avant ce rappel (rhai `track_operation`) :
            // `on_progress` n'empiète jamais sur `MAX_OPERATIONS` ni sur sa surcharge par une
            // closure de `setups`.
            let progress_token = eval_token.clone();
            script.engine.on_progress(move |_operations: u64| {
                progress_token
                    .is_cancelled()
                    .then(|| Dynamic::from("cancelled: timeout_secs reached"))
            });
            // Fil d'exécution (`./rhai_step.sdd` « Fil d'exécution du script », 2026-11-01,
            // arbitrage `OI-9` rendu, `DEC-10`) : l'évaluation passe dans un **fil dédié** à pile
            // de `SCRIPT_STACK_SIZE`, créé ici — dans le `spawn_blocking` — et joint par canal
            // `mpsc`, afin que `set_max_call_levels(512)` s'épuise avant la pile native (mesure
            // A6 : avortement dès 144 niveaux en debug sur la pile de ~2 Mio). Le moteur et le
            // `Scope` (`Script`) passent par mouvement (`Send`, vérifié à la compilation) ; le
            // jeton d'annulation est un clone partageant le même drapeau (`Send + Sync`) ; les
            // closures de `setups` et leur contexte runtime restent sur CE fil (`JoinError`
            // inchangé). `cancel::scope` s'ouvre DANS le fil d'évaluation : le jeton de
            // `vynil_core::cancel` est porté par le thread-local du fil qui ouvre la portée.
            let (sender, receiver) = mpsc::channel::<Result<Value, vynil_core::Error>>();
            let script_source = rhai_config.script;
            let scope_token = eval_token;
            let eval_thread = std::thread::Builder::new()
                .stack_size(SCRIPT_STACK_SIZE)
                .spawn(move || {
                    // Étape 5 — l'évaluation s'exécute dans la portée d'annulation de
                    // `vynil-core` : les appels natifs bloquants qui liraient le jeton rendent
                    // `Error::Cancelled` en 25 ms environ. La `Map` JSON est rendue telle quelle.
                    let rendu = cancel::scope(&scope_token, || script.eval_map_json(&script_source));
                    // Une panique n'atteint pas cet envoi : le sender tombe avec le unwind et le
                    // canal se ferme sans valeur (étape 4).
                    let _ = sender.send(rendu);
                })
                .map_err(EvalThreadError::Spawn)?;
            // Jointure par canal : une valeur reçue prouve que le fil a fini d'émettre ; un canal
            // fermé sans valeur est sa panique — jamais un panic qui remonte (`Handles`). Le
            // `JoinHandle` n'est pas joint, la jointure du canal le rend redondant.
            drop(eval_thread);
            receiver.recv().map_err(EvalThreadError::ChannelClosed)
        });
        // Le futur de la tâche bloquante est attendu sous le plafond de durée (« Durée
        // d'exécution ») : à l'expiration, `run` annule le jeton et rend IMMÉDIATEMENT le rendu
        // de `WorkflowError::ScriptTimeout` (`MRD-WORKFLOW-009`), sans attendre la fin du fil —
        // un fil non coopératif (le `sleep` du substrat rhai, une closure de `setups` qui ne rend
        // pas la main) y survit jusqu'à ce qu'il rende la main de lui-même, comportement accepté
        // (`./rhai_step.sdd` « Liste blanche », arbitrage `DEC-8` du 2026-11-01).
        match tokio::time::timeout(Duration::from_secs(timeout_secs), evaluation).await {
            Ok(joined) => match joined
                // Étape 4 — panic à l'intérieur du bloc bloquant lui-même (thread du
                // `spawn_blocking`, y compris une closure de `setups` qui panique) : le
                // `JoinError` le capture, le thread appelant ne panique jamais.
                .map_err(|e| StepError {
                    message: format!("rhai task panicked: {e}"),
                    retryable: false,
                })? {
                // Étape 4 bis — panic du fil dédié d'évaluation (canal fermé sans valeur) ou
                // création de ce fil impossible : même message, aucune traduction dédiée
                // (`Handles`, 2026-11-01).
                Err(e) => Err(StepError {
                    message: format!("rhai task panicked: {e}"),
                    retryable: false,
                }),
                // Étape 5 — valeur JSON rendue par le script, aucune enveloppe.
                Ok(Ok(rendu)) => Ok(rendu),
                // Étape 3 — tout échec Rhai (`Error::RhaiError` comme `SerializationError`) est non
                // retryable : déterministe vis-à-vis de l'entrée, rien à gagner à rejouer ; le
                // `Display` thiserror de `vynil_core::Error` porte déjà le contexte utile. Un
                // script qui échoue pour une autre cause que la durée garde son message habituel.
                Ok(Err(e)) => Err(StepError {
                    message: e.to_string(),
                    retryable: false,
                }),
            },
            Err(_elapsed) => {
                token.cancel();
                Err(StepError {
                    message: WorkflowError::ScriptTimeout {
                        seconds: timeout_secs,
                    }
                    .to_string(),
                    retryable: false,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::error::WorkflowError;
    use super::super::step::MiryadWorkflowStep;
    use super::super::step::RunInfo;
    use super::super::step::StepRegistry;
    use super::{RhaiStep, Script};
    use crate::workflow::template::render_config;
    use serde_json::{Value, json};
    use std::collections::HashMap;

    fn no_inputs() -> HashMap<String, Value> {
        HashMap::new()
    }

    /// `RunInfo` neutre (#26 lot A) : le kind `"rhai"` l'ignore (`_run`), le script ne voit
    /// toujours que `inputs` et `config` — aucun changement de comportement attendu.
    fn run_info() -> RunInfo {
        RunInfo {
            run_key: "t".into(),
            step_id: "s".into(),
            depth: 0,
            vars: json!({}),
            trigger: None,
        }
    }

    /// Chemin des fixtures de résolution de modules Rhai, ancré sur la racine du dépôt via
    /// `CARGO_MANIFEST_DIR` (indépendant du répertoire courant du runner).
    fn fixture_resolver_path() -> String {
        concat!(env!("CARGO_MANIFEST_DIR"), "/src/workflow/fixtures_rhai").to_string()
    }

    /// Scenario « script simple retourne une Map JSON » : `inputs.name` est exposé au script, la
    /// `Map` rendue par `eval_map_json` ressort telle quelle, sans enveloppe.
    #[tokio::test]
    async fn script_simple_retourne_une_map_json() {
        let step = RhaiStep::default();
        let mut inputs: HashMap<String, Value> = HashMap::new();
        inputs.insert("name".to_string(), json!("Ada"));
        let rendu = step
            .run(
                &run_info(),
                json!({ "script": "#{ greeting: \"hello \" + inputs.name }" }),
                inputs,
            )
            .await
            .expect("le script se réduit à une Map, run devait rendre un Ok");
        assert_eq!(rendu, json!({ "greeting": "hello Ada" }));
    }

    /// Scenario « config sans champ script est rejeté avant toute évaluation » : l'erreur vient
    /// de la désérialisation (`from_value` seul), jamais de Rhai — le message porte le préfixe
    /// contractuel et le manquement rapporté par serde, et aucune trace d'exécution de script
    /// n'apparaît. Le « aucun `Script` construit » n'est observable que par là : assertion sur le
    /// message seul, consignée dans le rapport de l'implementer.
    #[tokio::test]
    async fn config_sans_script_est_rejete_avant_toute_evaluation() {
        let step = RhaiStep::default();
        let Err(erreur) = step
            .run(&run_info(), json!({ "not_a_script": 1 }), no_inputs())
            .await
        else {
            panic!("une config sans `script` devait être refusée avant toute évaluation");
        };
        assert!(!erreur.retryable, "config invalide : jamais retryable");
        assert!(
            erreur.message.starts_with("config rhai invalide: "),
            "message attendu préfixé « config rhai invalide: », rendu : {:#?}",
            erreur.message
        );
        assert!(
            erreur.message.contains("missing field"),
            "le message doit venir de serde (preuve que `from_value` seul a échoué) : {:#?}",
            erreur.message
        );
        assert!(
            !erreur.message.contains("Rhai script error"),
            "aucune évaluation Rhai n'a pu avoir lieu : {:#?}",
            erreur.message
        );
    }

    /// Scenario « script qui échoue à l'exécution rend une erreur non-retryable » : `throw
    /// "boom"` remonte le texte de l'erreur Rhai portée par `vynil_core::Error`, sans retry.
    #[tokio::test]
    async fn script_qui_echoue_rend_une_erreur_non_retryable() {
        let step = RhaiStep::default();
        let Err(erreur) = step
            .run(&run_info(), json!({ "script": "throw \"boom\"" }), no_inputs())
            .await
        else {
            panic!("`throw \"boom\"` devait rendre un Err");
        };
        assert!(!erreur.retryable, "script échoué : jamais retryable");
        assert!(
            erreur.message.contains("boom"),
            "le texte de l'erreur Rhai doit transiter : {:#?}",
            erreur.message
        );
    }

    /// Scenario « script qui ne rend pas une Map échoue proprement » : `42` ne se réduit pas à
    /// une `Map` — `eval_map_json` échoue côté `vynil-core`, le message vient tel quel, sans
    /// panic et sans Ok d'une valeur non-objet.
    #[tokio::test]
    async fn script_qui_ne_rend_pas_une_map_echoue_proprement() {
        let step = RhaiStep::default();
        let rendu = step
            .run(&run_info(), json!({ "script": "42" }), no_inputs())
            .await;
        match rendu {
            Ok(valeur) => panic!("`42` ne devait jamais rendre un Ok, rendu : {valeur}"),
            Err(erreur) => {
                assert!(!erreur.retryable, "non-Map : jamais retryable");
                assert!(
                    !erreur.message.is_empty(),
                    "le message vient de `vynil_core::Error`, jamais vide"
                );
            }
        }
    }

    /// Scenario « config accessible au script au-delà du champ script » : `config.extra` est
    /// visible du script — c'est le JSON complet qui est exposé, et `RhaiConfig` n'est pas fermé
    /// aux champs inconnus.
    #[tokio::test]
    async fn config_accessible_au_script_au_dela_du_champ_script() {
        let step = RhaiStep::default();
        let rendu = step
            .run(
                &run_info(),
                json!({ "script": "#{ x: config.extra }", "extra": 7 }),
                no_inputs(),
            )
            .await
            .expect("le script se réduit à une Map, run devait rendre un Ok");
        assert_eq!(rendu, json!({ "x": 7 }));
    }

    /// Scenario « script qui boucle indéfiniment est interrompu par le plafond d'opérations » :
    /// `loop {}` rend une erreur Rhai non retryable en temps borné au lieu d'occuper un thread
    /// bloquant indéfiniment.
    #[tokio::test]
    async fn script_qui_boucle_est_interrompu_par_le_plafond() {
        let step = RhaiStep::default();
        let rendu = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            step.run(&run_info(), json!({ "script": "loop {}; #{}" }), no_inputs()),
        )
        .await
        .expect("le plafond d'opérations devait interrompre la boucle avant 30 s");
        match rendu {
            Ok(valeur) => panic!("une boucle infinie ne devait jamais rendre un Ok : {valeur}"),
            Err(erreur) => {
                assert!(
                    !erreur.retryable,
                    "un script qui boucle est déterministe : jamais retryable"
                );
                assert!(
                    erreur.message.to_lowercase().contains("operations"),
                    "erreur de dépassement d'opérations attendue : {:#?}",
                    erreur.message
                );
            }
        }
    }

    /// Scenario « deux appels successifs n'interfèrent jamais » : même instance de [`RhaiStep`],
    /// trois appels. Observable déterministe retenu (consigné au rapport) : une variable de
    /// script posée au premier appel ne doit ni survivre au second (une instance de `Script`
    /// réutilisée garderait `contamination` dans son `Scope` persistant et rendrait alors un Ok
    /// empoisonné — le test basculerait), ni invalider le moteur du troisième.
    #[tokio::test]
    async fn deux_appels_successifs_n_interferent_jamais() {
        let step = RhaiStep::default();
        // Appel A : dépose une variable de script et un input nommés, se termine en Ok.
        let mut inputs_a: HashMap<String, Value> = HashMap::new();
        inputs_a.insert("dep".to_string(), json!("Ada"));
        let rendu_a = step
            .run(
                &run_info(),
                json!({ "script": "let contamination = \"fuite\"; #{ a: contamination }" }),
                inputs_a,
            )
            .await
            .expect("l'appel A, script valide, devait rendre un Ok");
        assert_eq!(rendu_a, json!({ "a": "fuite" }));
        // Appel B : référence cette variable — elle ne doit pas exister (nouveau `Script`,
        // nouveau `Scope`). Un `Ok` ici prouverait un `Script` partagé.
        let rendu_b: Result<Value, _> = step
            .run(
                &run_info(),
                json!({ "script": "#{ b: contamination }" }),
                no_inputs(),
            )
            .await;
        match rendu_b {
            Ok(valeur) => {
                panic!("la variable de l'appel A a fui vers l'appel B (Script réutilisé) : {valeur}");
            }
            Err(erreur) => {
                assert!(!erreur.retryable, "variable absente : jamais retryable");
                assert!(
                    erreur.message.contains("contamination") && erreur.message.contains("not found"),
                    "erreur catégorisée « variable introuvable » attendue : {:#?}",
                    erreur.message
                );
            }
        }
        // Appel C : le moteur est intact, ses helpers enregistrés par `new_bare` répondent.
        let rendu_c = step
            .run(
                &run_info(),
                json!({ "script": "#{ c: base64_encode(\"x\") }" }),
                no_inputs(),
            )
            .await
            .expect("l'appel C, script valide, devait rendre un Ok");
        assert_eq!(rendu_c, json!({ "c": "eA==" }));
    }

    /// Scenario « `RhaiStep::default()` n'ouvre aucun import, `RhaiStep::new(...)` le permet » :
    /// le script `import` un module `lib`. Sans chemin de résolution, l'échec est catégorisé
    /// « Module not found » (convention `import_run`/`ErrorModuleNotFound` de `add_common`,
    /// jamais une erreur brute) ; avec le répertoire de fixture comme chemin, la fonction du
    /// module importé répond. Fidèle au source Rhai réel (rhai 1.26.1 sous `vynil-core` 0.7.7) :
    /// `import` est une instruction terminée par `;` (la forme `import("lib")::value()` évoquée
    /// par la spec n'existe pas — écart consigné au rapport) et la fixture est un module à `fn`
    /// nue, publique par défaut (`pub fn` et `export fn` ne sont pas valides pour une fonction
    /// sous cette version, vérifié en source : `tokenizer.rs`, `parser.rs`).
    #[tokio::test]
    async fn default_n_ouvre_aucun_import_new_le_permet() {
        let script = "import \"lib\" as m;\n#{ x: m::value() }";
        // default() : aucun chemin de résolution → module introuvable, erreur catégorisée.
        let par_defaut = RhaiStep::default();
        let Err(erreur) = par_defaut
            .run(&run_info(), json!({ "script": script }), no_inputs())
            .await
        else {
            panic!("sans chemin de résolution, `import \"lib\"` devait échouer");
        };
        assert!(!erreur.retryable, "module introuvable : jamais retryable");
        assert!(
            erreur.message.contains("Module not found") && erreur.message.contains("lib"),
            "l'erreur doit être catégorisée `ErrorModuleNotFound` pour `lib` : {:#?}",
            erreur.message
        );
        // new(vec![chemin]) : le même script réussit et rend la valeur du module importé.
        let avec_chemin = RhaiStep::new(vec![fixture_resolver_path()]);
        let rendu = avec_chemin
            .run(&run_info(), json!({ "script": script }), no_inputs())
            .await
            .expect("avec le chemin de fixture, l'import doit résoudre");
        assert_eq!(rendu, json!({ "x": 41 }));
    }

    /// Scenario « `with_setup` enregistre une fonction hôte appelable par le script » (#27) : la
    /// closure enregistre `double` sur le moteur de l'exécution, le script l'appelle normalement.
    #[tokio::test]
    async fn with_setup_enregistre_une_fonction_hote_appelable_par_le_script() {
        let step = RhaiStep::default().with_setup(|s, _config, _inputs| {
            s.engine.register_fn("double", |x: i64| x * 2);
        });
        let rendu = step
            .run(
                &run_info(),
                json!({ "script": "#{ v: double(21) }" }),
                no_inputs(),
            )
            .await
            .expect("la fonction hôte `double` enregistrée par `with_setup` doit répondre");
        assert_eq!(rendu, json!({ "v": 42 }));
    }

    /// Scenario « la closure de `with_setup` reçoit config et inputs » (#27) : les deux paramètres
    /// sont transmis en lecture seule à chaque closure ; `ctx_name()`/`ctx_extra()` rendent les
    /// valeurs capturées par clonage depuis les références reçues.
    #[tokio::test]
    async fn la_closure_de_with_setup_recoit_config_et_inputs() {
        let step = RhaiStep::default().with_setup(|s, config, inputs| {
            let name = inputs
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let extra = config.get("extra").and_then(Value::as_i64).unwrap_or(0);
            s.engine.register_fn("ctx_name", move || name.clone());
            s.engine.register_fn("ctx_extra", move || extra);
        });
        let mut inputs: HashMap<String, Value> = HashMap::new();
        inputs.insert("name".to_string(), json!("Ada"));
        let rendu = step
            .run(
                &run_info(),
                json!({ "script": "#{ a: ctx_name(), b: ctx_extra() }", "extra": 7 }),
                inputs,
            )
            .await
            .expect("les fonctions hôte voient config et inputs, run devait rendre un Ok");
        assert_eq!(rendu, json!({ "a": "Ada", "b": 7 }));
    }

    /// Scenario « plusieurs `with_setup` sont cumulatifs, dans l'ordre » (#27) : les deux closures
    /// sont effectives (le « dernier gagne » serait un effacement silencieux), appelées dans
    /// l'ordre d'enregistrement — pour `g` redéfinie, la règle du moteur Rhai (dernière
    /// enregistrée) prévaut.
    #[tokio::test]
    async fn plusieurs_with_setup_sont_cumulatifs_dans_l_ordre() {
        let step = RhaiStep::default()
            .with_setup(|s, _config, _inputs| {
                s.engine.register_fn("f", || 1_i64);
                s.engine.register_fn("g", || 10_i64);
            })
            .with_setup(|s, _config, _inputs| {
                s.engine.register_fn("h", || 2_i64);
                s.engine.register_fn("g", || 20_i64);
            });
        let rendu = step
            .run(
                &run_info(),
                json!({ "script": "#{ f: f(), h: h(), g: g() }" }),
                no_inputs(),
            )
            .await
            .expect("les deux closures cumulées devaient être effectives");
        assert_eq!(rendu, json!({ "f": 1, "h": 2, "g": 20 }));
    }

    /// Scenario « la closure est rappelée à chaque exécution sur un Script neuf » (#27) : le
    /// compteur capturé est incrémenté à chaque `run` (vaut 2 après deux appels), et chaque
    /// exécution a vu sa propre fonction enregistrée — la valeur rendue porte le numéro
    /// d'appel, preuve qu'aucun état de `Script` ne survit d'une exécution à l'autre.
    #[tokio::test]
    async fn la_closure_est_rappelee_a_chaque_execution_sur_un_script_neuf() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let appelee = Arc::new(AtomicUsize::new(0));
        let compteur = Arc::clone(&appelee);
        let step = RhaiStep::default().with_setup(move |s, _config, _inputs| {
            let numero = compteur.fetch_add(1, Ordering::SeqCst) + 1;
            s.engine
                .register_fn("apparitions", move || i64::try_from(numero).unwrap_or(i64::MAX));
        });
        let premier = step
            .run(
                &run_info(),
                json!({ "script": "#{ v: apparitions() }" }),
                no_inputs(),
            )
            .await
            .expect("la closure doit s'exécuter au premier run");
        assert_eq!(premier, json!({ "v": 1 }));
        let second = step
            .run(
                &run_info(),
                json!({ "script": "#{ v: apparitions() }" }),
                no_inputs(),
            )
            .await
            .expect("la closure doit être rappelée au second run, pas seulement au premier");
        assert_eq!(second, json!({ "v": 2 }));
        assert_eq!(
            appelee.load(Ordering::SeqCst),
            2,
            "une closure par run, jamais mise en cache"
        );
    }

    /// Scenario « la closure peut appeler du code async par `block_on` dans le pool bloquant »
    /// (#27) : la closure capture un `Handle` de runtime multi-thread, la fonction Rhai qu'elle
    /// enregistre y fait `block_on` — légitime car elle s'exécute sur un thread du pool
    /// bloquant, jamais un thread de travail. Pas de deadlock, pas de panic « Cannot start a
    /// runtime ».
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn la_closure_peut_appeler_du_code_async_par_block_on_dans_le_pool_bloquant() {
        let handle = tokio::runtime::Handle::current();
        let step = RhaiStep::default().with_setup(move |s, _config, _inputs| {
            let handle = handle.clone();
            s.engine
                .register_fn("slow", move || handle.block_on(std::future::ready(5_i64)));
        });
        let rendu = step
            .run(&run_info(), json!({ "script": "#{ v: slow() }" }), no_inputs())
            .await
            .expect("block_on sur le Handle capturé doit rendre 5, sans deadlock");
        assert_eq!(rendu, json!({ "v": 5 }));
    }

    /// Scenario « une closure de `with_setup` qui panique est traduite en erreur non-retryable »
    /// (#27) — premier chemin de test de la branche `JoinError` (`Handles`, couverture qui
    /// n'était que lecture depuis 2026-09-23) : le panic de la closure est capturé par le
    /// `spawn_blocking`, le message porte le préfixe contractuel, aucun panic ne remonte au
    /// test. La sonde (seconde closure, compteur) prouve que les closures suivantes ne sont pas
    /// appelées — donc que le script n'a pas été évalué.
    #[tokio::test]
    async fn une_closure_de_with_setup_qui_panique_est_traduite_en_erreur_non_retryable() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let sonde = Arc::new(AtomicUsize::new(0));
        let sonde_appel = Arc::clone(&sonde);
        let step = RhaiStep::default()
            .with_setup(|_s, _config, _inputs| panic!("setup boom"))
            .with_setup(move |_s, _config, _inputs| {
                let _ = sonde_appel.fetch_add(1, Ordering::SeqCst);
            });
        // Script valide : s'il avait été évalué, `run` rendrait un Ok — l'Err attendu prouve
        // l'interruption avant l'évaluation, la sonde à 0 qu'aucune closure suivante n'a tourné.
        let Err(erreur) = step
            .run(&run_info(), json!({ "script": "#{ v: 1 }" }), no_inputs())
            .await
        else {
            panic!("une closure qui panique ne devait jamais rendre un Ok");
        };
        assert!(!erreur.retryable, "panic de closure : jamais retryable");
        assert!(
            erreur.message.starts_with("rhai task panicked: "),
            "message attendu préfixé « rhai task panicked: », rendu : {:#?}",
            erreur.message
        );
        assert_eq!(
            sonde.load(Ordering::SeqCst),
            0,
            "les closures suivantes ne sont pas appelées après le panic, et le script n'a pas été évalué"
        );
    }

    /// Scenario « sans `with_setup` le comportement est inchangé » (#27) : `Default` et
    /// `new(vec![])` sans closure gardent les Scenario préexistants à l'identique (verrouillés
    /// par les sept tests ci-dessus, inchangés) ; une fonction hôte non enregistrée est une
    /// erreur Rhai non retryable, jamais un comportement par défaut ajouté par la crate. Test
    /// de non-régression : il verte d'emblée par construction — le Scenario décrit précisément
    /// une invariance (écart consigné au rapport de l'implementer).
    #[tokio::test]
    async fn sans_with_setup_le_comportement_est_inchange() {
        for step in [RhaiStep::default(), RhaiStep::new(Vec::new())] {
            let mut inputs: HashMap<String, Value> = HashMap::new();
            inputs.insert("name".to_string(), json!("Ada"));
            let rendu = step
                .run(
                    &run_info(),
                    json!({ "script": "#{ greeting: \"hello \" + inputs.name }" }),
                    inputs,
                )
                .await
                .expect("sans with_setup, le script simple doit se comporter comme avant #27");
            assert_eq!(rendu, json!({ "greeting": "hello Ada" }));

            let Err(erreur) = step
                .run(&run_info(), json!({ "script": "#{ v: double(1) }" }), no_inputs())
                .await
            else {
                panic!("`double` n'est enregistrée par aucune closure : le script devait échouer");
            };
            assert!(!erreur.retryable, "fonction introuvable : jamais retryable");
            assert!(
                erreur.message.contains("double") && erreur.message.to_lowercase().contains("not found"),
                "erreur Rhai « fonction introuvable » attendue pour `double` : {:#?}",
                erreur.message
            );
        }
    }

    /// Scenario « le script n'est jamais rendu mais les autres champs de config le sont »
    /// (2026-10-06) : le rendu central (`render_config`, ./template.sdd, sous le contrat de
    /// ./dispatcher.sdd) précède `run`. `script` doit être recopié intact (`raw_fields` du kind),
    /// `nom` rendu ; le script lit `config.nom` déjà rendu.
    #[tokio::test]
    async fn le_script_n_est_jamais_rendu_mais_les_autres_champs_de_config_le_sont() {
        let mut inputs: HashMap<String, Value> = HashMap::new();
        inputs.insert("a".to_string(), json!("Ada"));
        let config = json!({
            "script": r#"#{ s: "{{inputs.a}}", v: config.nom }"#,
            "nom": "{{inputs.a}}"
        });
        let config_rendue = render_config(&config, &run_info(), &inputs, RhaiStep::default().raw_fields())
            .expect("le rendu central de la config ne doit pas échouer");
        let rendu = RhaiStep::default()
            .run(&run_info(), config_rendue, inputs)
            .await
            .expect("le script se réduit à une Map, run devait rendre un Ok");
        assert_eq!(rendu, json!({ "s": "{{inputs.a}}", "v": "Ada" }));
    }

    /// Scenario « le script lit l'identité du run » (2026-10-06) : la variable `run` expose
    /// `run_key` et `depth` du `RunInfo` reçu par `run`.
    #[tokio::test]
    async fn le_script_lit_l_identite_du_run() {
        let run = RunInfo {
            run_key: "r1".into(),
            step_id: "s1".into(),
            depth: 2,
            vars: json!({}),
            trigger: None,
        };
        let rendu = RhaiStep::default()
            .run(
                &run,
                json!({ "script": "#{ k: run.run_key, d: run.depth }" }),
                no_inputs(),
            )
            .await
            .expect("la variable `run` doit être disponible au script");
        assert_eq!(rendu, json!({ "k": "r1", "d": 2 }));
    }

    /// Scenario « la fonction render utilise le contexte du run » (2026-10-06) : `render` rend
    /// une chaîne avec les `inputs` du step, comme la config d'un autre kind.
    #[tokio::test]
    async fn la_fonction_render_utilise_le_contexte_du_run() {
        let mut inputs: HashMap<String, Value> = HashMap::new();
        inputs.insert("a".to_string(), json!("Ada"));
        let rendu = RhaiStep::default()
            .run(
                &run_info(),
                json!({ "script": r#"#{ m: render("salut {{inputs.a}}") }"# }),
                inputs,
            )
            .await
            .expect("render doit rendre la chaîne avec le contexte du step");
        assert_eq!(rendu, json!({ "m": "salut Ada" }));
    }

    /// Scenario « render échoue en erreur de script non retryable » (2026-10-06) : une variable
    /// absente en mode strict fait échouer le script, non retryable, message porteur de
    /// `template invalide`.
    #[tokio::test]
    async fn render_echoue_en_erreur_de_script_non_retryable() {
        let Err(erreur) = RhaiStep::default()
            .run(
                &run_info(),
                json!({ "script": r#"#{ m: render("{{inputs.absent}}") }"# }),
                no_inputs(),
            )
            .await
        else {
            panic!("une variable absente dans `render` devait faire échouer le script");
        };
        assert!(!erreur.retryable, "erreur de rendu : jamais retryable");
        assert!(
            erreur.message.contains("template invalide"),
            "message attendu porteur de « template invalide » : {:#?}",
            erreur.message
        );
    }

    /// Scenario « `raw_fields` du kind rhai » (2026-10-06) : le registre expose `&["script"]` pour
    /// le kind `"rhai"`.
    #[test]
    fn raw_fields_du_kind_rhai_vaut_script() {
        let mut registre = StepRegistry::new();
        registre
            .register(RhaiStep::default())
            .expect("l'enregistrement d'un kind neuf ne doit pas échouer");
        assert_eq!(registre.raw_fields("rhai"), Some(&["script"][..]));
    }

    /// Scenario « le moteur n'expose ni l'environnement du processus ni les accès hôtes »
    /// (A4, `FR-16`, « Liste blanche » ; amendé 2026-11-01 : la moitié `sleep` est retirée du
    /// Scenario — `sleep` vient du `lang_core` de rhai, substrat accepté (`DEC-8`), et sera
    /// verrouillée par le Scenario réservé A5 « un script bloqué dans sleep rend le timeout sans
    /// attendre le fil ») : `get_env` (groupe hôte `core_host`) n'est jamais enregistré par
    /// `build_script` — le script échoue en erreur Rhai « fonction inconnue » non retryable.
    #[tokio::test]
    async fn le_moteur_n_expose_ni_lenvironnement_du_processus_ni_les_acces_hotes() {
        let Err(erreur) = RhaiStep::default()
            .run(
                &run_info(),
                json!({ "script": r#"#{ v: get_env("HOME") }"# }),
                no_inputs(),
            )
            .await
        else {
            panic!("`get_env` ne doit jamais être atteignable par un script, run devait rendre un Err");
        };
        assert!(!erreur.retryable, "fonction inconnue : jamais retryable");
        assert!(
            erreur.message.contains("get_env") && erreur.message.to_lowercase().contains("not found"),
            "`get_env` doit être désignée comme fonction inconnue : {:#?}",
            erreur.message
        );
    }

    /// Scenario « fichiers et shell restent absents même quand les features de `vynil-core` sont
    /// actives » (A4, `FR-16`) : le graphe de test compile `vynil-core` avec `fs` et `shell`
    /// (dev-dependency, /miryad-core.sdd tâche A1) ; la liste blanche ne doit jamais appeler leurs
    /// registrars. La sonde `Script::new_bare` du même test prouve que `is_file` (nom lu au source
    /// de `fs_fns`) et `shell_run` (source de `shell_fns`) existent bien dans ce graphe — c'est le
    /// contraste qui attribue leur absence à la liste blanche, non à un défaut de features.
    #[tokio::test]
    async fn fichiers_et_shell_resten_absents_meme_quand_les_features_de_vynil_core_sont_actives() {
        for (script, fonction) in [
            (
                r#"#{ v: is_file("/miryad-a4-sentinelle-inexistante") }"#,
                "is_file",
            ),
            (r#"#{ v: shell_run("true") }"#, "shell_run"),
        ] {
            let Err(erreur) = RhaiStep::default()
                .run(&run_info(), json!({ "script": script }), no_inputs())
                .await
            else {
                panic!(
                    "`{fonction}` ne doit jamais être atteignable par un script, run devait rendre un Err"
                );
            };
            assert!(!erreur.retryable, "fonction inconnue : jamais retryable");
            assert!(
                erreur.message.contains(fonction) && erreur.message.to_lowercase().contains("not found"),
                "`{fonction}` doit être désignée comme fonction inconnue : {:#?}",
                erreur.message
            );
        }
        // Sonde de contraste : les deux mêmes fonctions répondent sous `Script::new_bare` — elles
        // sont compilées dans ce graphe (features `fs` et `shell` via la dev-dependency).
        let mut sonde = Script::new_bare(Vec::new());
        let present = sonde
            .eval(r#"is_file("/miryad-a4-sentinelle-inexistante")"#)
            .expect("le registrar `fs` est compilé dans le graphe de test (dev-dependency `fs`)");
        assert_eq!(
            present.as_bool(),
            Ok(false),
            "`is_file` doit répondre sous `new_bare`"
        );
        let code = sonde
            .eval(r#"shell_run("true")"#)
            .expect("le registrar `shell` est compilé dans le graphe de test (dev-dependency `shell`)");
        assert_eq!(
            code.as_int(),
            Ok(0),
            "`shell_run` doit répondre 0 sous `new_bare` (code de sortie)"
        );
    }

    /// Scenario « les helpers purs restent disponibles » (A4, `FR-16`) : la liste blanche garde les
    /// six registrars purs — au moins une fonction de chacun : `core_pure` (`sha256`), `chrono`
    /// (`date_now`/`format`), `hashes` (`crc32_hash`), `semver` (`semver_from` et `<`), `yaml`
    /// (`yaml_encode`), `glob` (`glob`) ; les shims `import_run`/`import_template` d'`add_common`
    /// sautent silencieusement un module absent (trace `log_debug`, jamais erreur). Test
    /// d'invariance : vert d'emblée sous le `new_bare` actuel — il rougit si `build_script`
    /// enregistrait trop peu.
    #[tokio::test]
    async fn les_helpers_purs_resten_disponibles() {
        // `crc32_hash` rend un `u32` rhai que `eval_map_json` ne sait sérialiser qu'en chaîne du
        // nom de type (et sans `==` mélangé `u32`/`i64`) : sa valeur est rendue via `to_string`.
        let script = r#"
import_run("zz_module_absent", #{});
import_template("zz_module_absent", #{});
#{
    sha: sha256("hello"),
    crc: to_string(crc32_hash("hello")),
    glob_hit: glob("miryad.txt", "*.txt"),
    glob_miss: glob("miryad.txt", "*.rs"),
    yml: yaml_encode(#{ k: 1 }),
    semver_lt: semver_from("1.2.3") < semver_from("2.0.0"),
    chrono_type: type_of(date_now()),
    chrono_year: date_now().format("%Y"),
}"#;
        let rendu = RhaiStep::default()
            .run(&run_info(), json!({ "script": script }), no_inputs())
            .await
            .unwrap_or_else(|erreur| {
                panic!(
                    "les six registrars purs et les shims devaient tous répondre : {}",
                    erreur.message
                )
            });
        assert_eq!(
            rendu["sha"],
            json!("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"),
            "`core_pure` : sha256(\"hello\")"
        );
        assert_eq!(
            rendu["crc"],
            json!("907060870"),
            "`hashes` : crc32(\"hello\") = 0x3610A686"
        );
        assert_eq!(rendu["glob_hit"], json!(true), "`glob` : match de *.txt");
        assert_eq!(rendu["glob_miss"], json!(false), "`glob` : non-match *.rs");
        assert_eq!(
            rendu["yml"],
            json!("k: 1\n"),
            "`yaml` : yaml_encode trié, terminé par un saut de ligne"
        );
        assert_eq!(rendu["semver_lt"], json!(true), "`semver` : comparaison d'ordres");
        assert_eq!(
            rendu["chrono_type"],
            json!("DateTimeHandler"),
            "`chrono` : date_now et type enregistré"
        );
        let annee: u32 = rendu["chrono_year"]
            .as_str()
            .unwrap_or_default()
            .parse()
            .expect("l'année rendue par `format(\"%Y\")` est un nombre");
        assert!(
            (2020..=2100).contains(&annee),
            "`chrono` : année plausible, rendu {annee}"
        );
    }

    /// Scenario « `log_info` émet un événement `tracing` du step » (A4, `FR-17`) : chaque `log_*`
    /// émet au niveau qui lui correspond, porte le message et les champs structurés `run_key` et
    /// `step_id` du step — et rien d'autre.
    #[tokio::test]
    async fn log_info_emet_un_evenement_tracing_du_step() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let journal = CaptureJournal::default();
        // Le garde thread-local de l'idiome `auth/oidc.rs` n'atteindrait pas les événements émis
        // dans le pool `spawn_blocking` : c'est le subscriber global qui joue le rôle de capture.
        // Sans effet sur les tests voisins : aucun n'installe de subscriber global (vérifié au
        // source à l'implémentation A4) et la couche ne retient que la cible de ce fichier.
        tracing::subscriber::set_global_default(
            tracing_subscriber::registry::Registry::default().with(CoucheJournal(journal.clone())),
        )
        .expect("aucun autre test de la crate n'installe de subscriber global");

        let run = RunInfo {
            run_key: "r1".into(),
            step_id: "s1".into(),
            depth: 0,
            vars: json!({}),
            trigger: None,
        };

        let rendu = RhaiStep::default()
            .run(
                &run,
                json!({ "script": r#"log_info("bonjour"); #{ ok: true }"# }),
                no_inputs(),
            )
            .await
            .unwrap_or_else(|erreur| {
                panic!(
                    "log_info ne doit pas faire échouer le script : {}",
                    erreur.message
                )
            });
        assert_eq!(rendu, json!({ "ok": true }));

        let evenement = trouver_evenement(&journal, "bonjour").expect("un événement de message `bonjour`");
        assert_eq!(
            evenement.niveau,
            tracing::Level::INFO,
            "log_info doit émettre au niveau INFO"
        );
        assert_champs_du_step(&evenement, "r1", "s1");

        // Les trois autres niveaux, chacun le sien (« log_debug, log_warn et log_error émettent
        // chacun leur niveau »).
        for (fonction, niveau, message) in [
            ("log_debug", tracing::Level::DEBUG, "a4-sonde-debug"),
            ("log_warn", tracing::Level::WARN, "a4-sonde-warn"),
            ("log_error", tracing::Level::ERROR, "a4-sonde-error"),
        ] {
            let script = format!(r#"{fonction}("{message}"); #{{}}"#);
            let rendu = RhaiStep::default()
                .run(&run, json!({ "script": script }), no_inputs())
                .await
                .unwrap_or_else(|erreur| {
                    panic!(
                        "`{fonction}` ne doit pas faire échouer le script : {}",
                        erreur.message
                    )
                });
            assert_eq!(rendu, json!({}));
            let evenement = trouver_evenement(&journal, message)
                .unwrap_or_else(|| panic!("un événement de message `{message}`"));
            assert_eq!(
                evenement.niveau, niveau,
                "`{fonction}` doit émettre au niveau {niveau}"
            );
            assert_champs_du_step(&evenement, "r1", "s1");
        }
    }

    /// Scenario « un script qui dépasse `timeout_secs` est interrompu » (A5, `FR-18`) : le plafond
    /// d'opérations est levé par `set_max_operations(0)` (0 = illimité, rhai 1.26.1) — seule la
    /// durée peut arrêter `loop {}`. Le rendu est un `StepError` non retryable dont le message est
    /// le rendu de `WorkflowError::ScriptTimeout { seconds: 1 }` (préfixe `MRD-WORKFLOW-009`), en
    /// moins de 5 s. Le garde externe de 30 s ne doit jamais sonner en vert (rendu à ~1 s) : il
    /// borne la phase rouge, où `timeout_secs` est ignoré et la boucle file sans fin. Que le fil
    /// bloquant s'arrête de lui-même peu après est prouvé par la fin du test elle-même : la
    /// destruction du runtime tokio en attente des tâches bloquantes, un fil qui ne s'arrêterait
    /// pas ferait suspendre le test plutôt que le passer.
    #[tokio::test]
    async fn un_script_qui_depasse_timeout_secs_est_interrompu() {
        let step = RhaiStep::default().with_setup(|s, _config, _inputs| {
            s.engine.set_max_operations(0);
        });
        let debut = std::time::Instant::now();
        let resultat = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            step.run(
                &run_info(),
                json!({ "script": "loop {}", "timeout_secs": 1 }),
                no_inputs(),
            ),
        )
        .await
        .expect("`timeout_secs` devait rendre une erreur en moins de 30 s");
        let duree = debut.elapsed();
        match resultat {
            Ok(valeur) => panic!("une boucle sans plafond ne devait jamais rendre un Ok : {valeur}"),
            Err(erreur) => {
                assert!(
                    !erreur.retryable,
                    "durée dépassée : rejouer dépasserait de nouveau, jamais retryable"
                );
                assert!(
                    erreur.message.starts_with("MRD-WORKFLOW-009"),
                    "message attendu préfixé `MRD-WORKFLOW-009` : {:#?}",
                    erreur.message
                );
                assert_eq!(
                    erreur.message,
                    WorkflowError::ScriptTimeout { seconds: 1 }.to_string(),
                    "le message est le rendu de `ScriptTimeout {{ seconds: 1 }}`"
                );
                assert!(
                    duree < std::time::Duration::from_secs(5),
                    "rendu attendu en moins de 5 s, mesuré {duree:?}"
                );
            }
        }
    }

    /// Scenario « un script bloqué dans `sleep` rend le timeout sans attendre le fil »
    /// (A5, `FR-18`, arbitrage `DEC-8` du 2026-11-01) : `sleep` vient du `lang_core` de rhai,
    /// bloquant pour le fil et non coopératif. Le rendu de l'erreur `ScriptTimeout` doit survenir
    /// en moins de 5 s alors que le `sleep(10)` survit dans son fil jusqu'à son terme — assertion
    /// du message à ~1 s puis de la durée, la seule preuve que le retour n'a pas attendu le fil.
    /// Le test lui-même dure ~10 s : la destruction du runtime tokio en attend le fil survivant,
    /// comportement accepté et non un échec.
    #[tokio::test]
    async fn un_script_bloque_dans_sleep_rend_le_timeout_sans_attendre_le_fil() {
        let debut = std::time::Instant::now();
        let resultat = RhaiStep::default()
            .run(
                &run_info(),
                json!({ "script": "sleep(10); #{}", "timeout_secs": 1 }),
                no_inputs(),
            )
            .await;
        let duree = debut.elapsed();
        match resultat {
            Ok(valeur) => {
                panic!("`sleep(10)` ne devait pas s'achever avant `timeout_secs` 1, rendu : {valeur}")
            }
            Err(erreur) => {
                assert!(!erreur.retryable, "durée dépassée : jamais retryable");
                assert!(
                    erreur.message.starts_with("MRD-WORKFLOW-009"),
                    "message attendu préfixé `MRD-WORKFLOW-009` : {:#?}",
                    erreur.message
                );
                assert_eq!(
                    erreur.message,
                    WorkflowError::ScriptTimeout { seconds: 1 }.to_string(),
                    "le message est le rendu de `ScriptTimeout {{ seconds: 1 }}`"
                );
                assert!(
                    duree < std::time::Duration::from_secs(5),
                    "le rendu ne doit pas attendre la fin du `sleep` : mesuré {duree:?}"
                );
            }
        }
    }

    /// Scenario « `timeout_secs` hors bornes est une config invalide » (A5, `FR-18`) : `0` et
    /// `3601` sont refusés avant toute évaluation, au message exact du `Must` ; `3600` est la
    /// borne haute valide — la même config est alors acceptée et le script évalué. Le cas absent
    /// (défaut 300) est verrouillé par tous les autres tests du module, qui n'ont pas de
    /// `timeout_secs` et doivent rester verts à l'identique.
    #[tokio::test]
    async fn timeout_secs_hors_bornes_est_une_config_invalide() {
        for valeur in [0_u64, 3601] {
            let config = json!({ "script": "#{}", "timeout_secs": valeur });
            match RhaiStep::default().run(&run_info(), config, no_inputs()).await {
                Ok(rendu) => {
                    panic!(
                        "`timeout_secs` {valeur} devait être refusé avant toute évaluation, rendu : {rendu}"
                    )
                }
                Err(erreur) => {
                    assert!(
                        !erreur.retryable,
                        "`timeout_secs` {valeur} hors bornes : jamais retryable"
                    );
                    assert_eq!(
                        erreur.message,
                        "config rhai invalide: timeout_secs doit être compris entre 1 et 3600",
                        "message exact du `Must` « Durée d'exécution » pour `timeout_secs` {valeur}"
                    );
                }
            }
        }
        // 3600 est dans `1..=MAX_TIMEOUT_SECS` : config acceptée, script évalué normalement.
        let rendu = RhaiStep::default()
            .run(
                &run_info(),
                json!({ "script": "#{ ok: true }", "timeout_secs": 3600 }),
                no_inputs(),
            )
            .await
            .expect("3600 est la borne haute valide : la config doit être acceptée et le script évalué");
        assert_eq!(rendu, json!({ "ok": true }));
    }

    /// Scenario « une récursion jusqu'au plafond ne fait pas avorter le processus » (A6, 2026-10-30,
    /// `dsd/miryad-core/workflow.md` `OI-9` ; amendé 2026-11-01 — arbitrage rendu : fil dédié de
    /// 256 Mio) : le script du Scenario s'appelle lui-même sans cas de base et doit être arrêté
    /// par `set_max_call_levels(512)` de `build_script` avant que les ~512 couches n'épuisent la
    /// pile native — la mesure A6 du 2026-11-01 sur la pile de ~2 Mio du `spawn_blocking` faisait
    /// avorter le processus dès 144 niveaux en debug. Ce test ne corrige aucun plafond. Le message
    /// attendu désigne le dépassement des niveaux d'appels : rhai 1.26.1 rend ce dépassement sous
    /// la variante `ErrorStackOverflow` (« Stack overflow », lu au source : `func/script.rs`,
    /// `types/error.rs`) ; c'est la seule voie d'apparition de ce texte dans ce test, un
    /// débordement de pile natif n'ayant pas de message — il avorte. Le test tourne dans les
    /// conditions du step réel : via `RhaiStep::run`, donc dans le fil dédié d'évaluation à pile
    /// dimensionnée (`SCRIPT_STACK_SIZE`), jamais dans le fil du test.
    #[tokio::test]
    async fn une_recursion_jusqu_au_plafond_ne_fait_pas_avorter_le_processus() {
        let step = RhaiStep::default();
        let resultat = step
            .run(
                &run_info(),
                json!({ "script": "fn f(n) { f(n + 1) } f(0); #{}" }),
                no_inputs(),
            )
            .await;
        match resultat {
            Ok(valeur) => panic!(
                "une récursion sans cas de base devait échouer sur le plafond d'appels de Rhai, rendu : {valeur}"
            ),
            Err(erreur) => {
                assert!(
                    !erreur.retryable,
                    "récursion plafonnée : déterministe vis-à-vis de l'entrée, jamais retryable"
                );
                assert!(
                    erreur.message.to_lowercase().contains("stack overflow"),
                    "le message doit désigner le dépassement des niveaux d'appels de Rhai \
                     (`ErrorStackOverflow` sous rhai 1.26.1) : {:#?}",
                    erreur.message
                );
            }
        }
    }

    /// Scenario « `vars` est lisible, modifiable localement et sans panique » (B6, 2026-10-30,
    /// lot B, `FR-14`, `CON-8`, « Variables de run ») : `RunInfo.vars` est posée comme variable
    /// ordinaire `vars` à côté de `inputs` et `run`. Trois temps sur la même instance et le même
    /// `RunInfo` : lecture (`{"a": "p1"}`), modification locale sans panique — `vars.project = "x"`
    /// ne doit jamais toucher le piège de la constante map (`set_dynamic`, jamais `push_constant`) —
    /// puis relecture `p1` au run suivant : le moteur est neuf à chaque exécution, la modification
    /// ne se propage ni aux autres steps ni au run.
    #[tokio::test]
    async fn vars_est_lisible_modifiable_localement_et_sans_panique() {
        let run = RunInfo {
            run_key: "r1".into(),
            step_id: "s1".into(),
            depth: 0,
            vars: json!({ "project": "p1" }),
            trigger: None,
        };
        let step = RhaiStep::default();
        let premier = step
            .run(&run, json!({ "script": "#{ a: vars.project }" }), no_inputs())
            .await
            .expect("la variable `vars` doit être lisible par le script");
        assert_eq!(
            premier,
            json!({ "a": "p1" }),
            "`vars.project` lu depuis RunInfo.vars"
        );
        let second = step
            .run(
                &run,
                json!({ "script": "vars.project = \"x\"; #{ b: vars.project }" }),
                no_inputs(),
            )
            .await
            .expect("modifier `vars` dans un script est légal (variable ordinaire) : jamais de panique");
        assert_eq!(
            second,
            json!({ "b": "x" }),
            "la modification est visible dans le script qui la fait"
        );
        let troisieme = step
            .run(&run, json!({ "script": "#{ a: vars.project }" }), no_inputs())
            .await
            .expect("un run suivant avec les mêmes `vars` doit relire `p1`");
        assert_eq!(
            troisieme,
            json!({ "a": "p1" }),
            "la modification est locale à l'exécution : moteur neuf à chaque `run`"
        );
    }

    /// Un événement `tracing` capturé : niveau et champs rendus (`message`, `run_key`, `step_id`).
    #[derive(Clone, Debug)]
    struct EvenementCapture {
        niveau: tracing::Level,
        champs: Vec<(String, String)>,
    }

    #[derive(Clone, Default)]
    struct CaptureJournal(std::sync::Arc<std::sync::Mutex<Vec<EvenementCapture>>>);

    /// Couche de capture à la `CaptureLayer` de `auth/oidc.rs`, étendue au niveau et aux champs
    /// nommés ; ne retient que les événements de la cible de ce fichier.
    struct CoucheJournal(CaptureJournal);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CoucheJournal {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            if !event
                .metadata()
                .target()
                .starts_with("miryad_core::workflow::rhai_step")
            {
                return;
            }
            let mut visiteur = VisiteurChamps::default();
            event.record(&mut visiteur);
            self.0
                .0
                .lock()
                .expect("test mutex is not poisoned")
                .push(EvenementCapture {
                    niveau: *event.metadata().level(),
                    champs: visiteur.champs,
                });
        }
    }

    #[derive(Default)]
    struct VisiteurChamps {
        champs: Vec<(String, String)>,
    }

    impl tracing::field::Visit for VisiteurChamps {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.champs.push((field.name().to_string(), format!("{value:?}")));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.champs.push((field.name().to_string(), value.to_string()));
        }
    }

    fn trouver_evenement(journal: &CaptureJournal, message: &str) -> Option<EvenementCapture> {
        journal
            .0
            .lock()
            .expect("test mutex is not poisoned")
            .iter()
            .find(|evenement| {
                evenement
                    .champs
                    .iter()
                    .any(|(nom, valeur)| nom == "message" && valeur == message)
            })
            .cloned()
    }

    /// Les deux champs structurés du step, `run_key` et `step_id`, aux valeurs attendues.
    fn assert_champs_du_step(evenement: &EvenementCapture, run_key: &str, step_id: &str) {
        for (champ, attendu) in [("run_key", run_key), ("step_id", step_id)] {
            let rendu = evenement
                .champs
                .iter()
                .find(|(nom, _)| nom == champ)
                .map(|(_, valeur)| valeur.as_str());
            assert_eq!(rendu, Some(attendu), "le champ `{champ}` doit valoir `{attendu}`");
        }
    }
}
