//! Kind natif `"rhai"` (feature `workflow`) : exécution d'un script Rhai via
//! [`vynil_core::engine::Script`], seul `impl` de [`MiryadWorkflowStep`] fourni par la crate.
//!
//! Le script reçoit deux variables — `inputs` (les sorties des steps amont déclarés, telles que
//! livrées par le trait) et `config` (le `config` complet du step, `script` compris) — et doit se
//! réduire à une `Map` Rhai, rendue telle quelle en JSON. Tout échec (config mal formé, erreur
//! d'exécution Rhai, panic interne) est un [`StepError`] `retryable: false` : rejouer un script
//! qui a échoué rendrait le même résultat.
//!
//! L'évaluation Rhai est synchrone et son moteur vit exclusivement dans une tâche
//! `tokio::task::spawn_blocking` : un script lent ne doit pas monopoliser le thread de travail
//! qui sert les autres invocations concurrentes. Aucun `Script` n'est porté par [`RhaiStep`] —
//! chaque `run` construit son moteur neuf, aucune variable d'une invocation ne fuit vers
//! l'autre.
//!
//! Ce fichier ne connaît ni accès DB, ni appel HTTP, ni client MCP, et n'enregistre lui-même
//! aucune fonction Rhai au-delà de ce que `Script::new_bare` fournit. Le point d'extension est
//! [`RhaiStep::with_setup`] (issue sebt3/miryad-core#27) : l'application empile des closures
//! qui, à chaque exécution, enregistrent leurs fonctions hôte sur le moteur neuf — jamais un
//! besoin additionnel codé en dur ici.
//!
//! [`MiryadWorkflowStep`]: super::step::MiryadWorkflowStep
//! [`StepError`]: super::step::StepError

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use vynil_core::engine::Script;

use super::step::{MiryadWorkflowStep, StepError};

/// Plafond d'opérations Rhai par exécution : `spawn_blocking` isole le thread de travail mais ne
/// peut pas interrompre une tâche bloquante — sans ce plafond, un `loop {}` occuperait un thread
/// du pool bloquant indéfiniment (`./rhai_step.sdd` `Must`, étape 2).
const MAX_OPERATIONS: u64 = 10_000_000;

/// Fermeture d'extension posée par [`RhaiStep::with_setup`] : le type exact des éléments de
/// `RhaiStep::setups`, dicté par `./rhai_step.sdd` `Must` — elle reçoit le `Script` neuf de
/// l'exécution, puis `config` et `inputs` en lecture seule. Alias privé de lisibilité — le
/// contrat est ce type, nommé ou non.
type SetupFn = dyn Fn(&mut Script, &Value, &HashMap<String, Value>) + Send + Sync + 'static;

/// Le `config` du kind `"rhai"`, désérialisé depuis le JSON opaque livré à
/// [`MiryadWorkflowStep::run`] — un seul champ attendu, les autres clés restent accessibles au
/// script via la variable `config` (le JSON complet lui est exposé, `script` compris).
#[derive(Deserialize)]
struct RhaiConfig {
    /// Texte source Rhai évalué par [`Script::eval_map_json`].
    script: String,
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
    /// (chemins transmis tels quels à [`Script::new_bare`]). Décision explicite de l'app :
    /// miryad-core n'ajoute lui-même aucun chemin (`Default` rend la liste vide, tout `import`
    /// échoue alors en erreur catégorisée, jamais en erreur brute). N'ajoute aucune closure de
    /// setup — le comportement est celui d'avant `with_setup`.
    #[must_use]
    pub fn new(resolver_path: Vec<String>) -> Self {
        Self {
            resolver_path,
            setups: Vec::new(),
        }
    }

    /// Empile une closure d'extension appelée à **chaque** exécution, dans le
    /// `spawn_blocking` de `run`, sur le `Script` neuf de l'exécution — après
    /// [`Script::new_bare`], la pose du plafond d'opérations et les `set_dynamic` de
    /// `inputs`/`config`, avant `eval_map_json` : la closure voit les deux variables déjà
    /// posées et peut enregistrer des fonctions hôte sur `script.engine` (issue
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

#[async_trait::async_trait]
impl MiryadWorkflowStep for RhaiStep {
    fn kind(&self) -> &'static str {
        "rhai"
    }

    async fn run(&self, config: Value, inputs: HashMap<String, Value>) -> Result<Value, StepError> {
        // Étape 1 — le `config` doit porter `script` ; un config mal formé ne se corrige pas en
        // rejouant (déterministe vis-à-vis de l'entrée).
        let rhai_config = serde_json::from_value::<RhaiConfig>(config.clone()).map_err(|e| StepError {
            message: format!("config rhai invalide: {e}"),
            retryable: false,
        })?;
        // Le `resolver_path` et les `Arc` de `setups` sont clonés avant le `move` : la tâche
        // bloquant le thread ne doit rien à `&self`, et un `Script` neuf est construit à chaque
        // exécution — aucun état ne survit d'un `run` à l'autre.
        let resolver_path = self.resolver_path.clone();
        let setups = self.setups.clone();
        // Étape 2 — évaluation hors du thread de travail tokio (le script, fourni par un admin,
        // peut boucler) ; un panic éventuel — y compris une closure de `setups` qui panique — y
        // est confiné et capturé en `JoinError` à l'étape 4.
        tokio::task::spawn_blocking(move || {
            let mut script = Script::new_bare(resolver_path);
            script.engine.set_max_operations(MAX_OPERATIONS);
            // Conversion infaillible HashMap→Map (pas de `serde_json::to_value` : aucun Result
            // ici). `config` est exposé en entier, `script` compris.
            script.set_dynamic("inputs", &Value::Object(inputs.clone().into_iter().collect()));
            script.set_dynamic("config", &config);
            // Closures d'extension (`with_setup`, #27) : après les `set_dynamic`, avant
            // l'évaluation — elles voient `inputs`/`config` posés et peuvent enregistrer des
            // fonctions hôte sur `script.engine`. Ordre d'enregistrement ; la première qui
            // panique interrompt tout (les suivantes ne tournent pas, le script n'est pas
            // évalué) — `Handles` de ./rhai_step.sdd.
            for setup in &setups {
                setup(&mut script, &config, &inputs);
            }
            // Étape 5 — la `Map` JSON est rendue telle quelle, aucune enveloppe.
            script.eval_map_json(&rhai_config.script)
        })
        .await
        // Étape 4 — panic à l'intérieur du bloc bloquant : le `JoinError` le capture, le thread
        // appelant ne panique jamais.
        .map_err(|e| StepError {
            message: format!("rhai task panicked: {e}"),
            retryable: false,
        })?
        // Étape 3 — tout échec Rhai (`Error::RhaiError` comme `SerializationError`) est non
        // retryable : déterministe vis-à-vis de l'entrée, rien à gagner à rejouer ; le `Display`
        // thiserror de `vynil_core::Error` porte déjà le contexte utile.
        .map_err(|e| StepError {
            message: e.to_string(),
            retryable: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::step::MiryadWorkflowStep;
    use super::RhaiStep;
    use serde_json::{Value, json};
    use std::collections::HashMap;

    fn no_inputs() -> HashMap<String, Value> {
        HashMap::new()
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
        let Err(erreur) = step.run(json!({ "not_a_script": 1 }), no_inputs()).await else {
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
        let Err(erreur) = step.run(json!({ "script": "throw \"boom\"" }), no_inputs()).await else {
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
        let rendu = step.run(json!({ "script": "42" }), no_inputs()).await;
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
            step.run(json!({ "script": "loop {}; #{}" }), no_inputs()),
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
                json!({ "script": "let contamination = \"fuite\"; #{ a: contamination }" }),
                inputs_a,
            )
            .await
            .expect("l'appel A, script valide, devait rendre un Ok");
        assert_eq!(rendu_a, json!({ "a": "fuite" }));
        // Appel B : référence cette variable — elle ne doit pas exister (nouveau `Script`,
        // nouveau `Scope`). Un `Ok` ici prouverait un `Script` partagé.
        let rendu_b: Result<Value, _> = step
            .run(json!({ "script": "#{ b: contamination }" }), no_inputs())
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
            .run(json!({ "script": "#{ c: base64_encode(\"x\") }" }), no_inputs())
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
        let Err(erreur) = par_defaut.run(json!({ "script": script }), no_inputs()).await else {
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
            .run(json!({ "script": script }), no_inputs())
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
            .run(json!({ "script": "#{ v: double(21) }" }), no_inputs())
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
            .run(json!({ "script": "#{ f: f(), h: h(), g: g() }" }), no_inputs())
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
            .run(json!({ "script": "#{ v: apparitions() }" }), no_inputs())
            .await
            .expect("la closure doit s'exécuter au premier run");
        assert_eq!(premier, json!({ "v": 1 }));
        let second = step
            .run(json!({ "script": "#{ v: apparitions() }" }), no_inputs())
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
            .run(json!({ "script": "#{ v: slow() }" }), no_inputs())
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
        let Err(erreur) = step.run(json!({ "script": "#{ v: 1 }" }), no_inputs()).await else {
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
                    json!({ "script": "#{ greeting: \"hello \" + inputs.name }" }),
                    inputs,
                )
                .await
                .expect("sans with_setup, le script simple doit se comporter comme avant #27");
            assert_eq!(rendu, json!({ "greeting": "hello Ada" }));

            let Err(erreur) = step
                .run(json!({ "script": "#{ v: double(1) }" }), no_inputs())
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
}
