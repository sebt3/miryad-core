//! Point d'extension des kinds de step de workflow (feature `workflow`) : un `impl` du trait
//! [`MiryadWorkflowStep`] par kind, fourni par l'application consommatrice, et [`StepRegistry`],
//! la collection des kinds connus qu'elle construit explicitement au démarrage.
//!
//! Ce fichier ne parle jamais Restate : la traduction d'un [`StepError`] en erreur de handler ou
//! en arrêt définitif est du ressort du dispatcher interne (`./dispatcher.rs`), et un kind
//! s'écrit, se lit et se teste comme une fonction async ordinaire, sans dépendre du protocole
//! d'invocation Restate.

use std::collections::HashMap;

/// Un type de step de workflow (« kind ») : la façon pour une application consommatrice d'ajouter
/// un comportement de step sans toucher à la crate — un `impl` par kind, aucun code par kind dans
/// miryad-core. [`StepRegistry::register`] en fixe le kind au démarrage ; le dispatcher interne
/// seul dispatche vers [`Self::run`].
///
/// `Send + Sync` est un super-trait (et non un ajout syntaxique au registre) : un `impl` doit
/// pouvoir vivre dans un `Box<dyn MiryadWorkflowStep>` traversant un handler async.
#[async_trait::async_trait]
pub trait MiryadWorkflowStep: Send + Sync {
    /// Clé stable du kind : clé du [`StepRegistry`] et valeur du champ `kind` du `StepDefinition`
    /// correspondant. Deux kinds enregistrés ne partagent jamais cette valeur (garanti par
    /// [`StepRegistry::register`]).
    fn kind(&self) -> &'static str;

    /// Exécute le step.
    ///
    /// `config` est le contenu opaque du champ `config` du `StepDefinition` de ce kind, reçu tel
    /// quel, jamais interprété ni validé ici. `inputs` porte une entrée par step amont déclaré
    /// dans `depends_on` (clé = id du step amont, valeur = la sortie que son propre `run` a
    /// rendue) ; un step sans dépendance reçoit un `HashMap` vide, jamais une absence. La valeur
    /// rendue est republiée par le moteur dans les `inputs` des steps qui déclarent celui-ci.
    ///
    /// Un kind qui reçoit une `config` ou un `inputs` dans une forme qu'il n'attend pas le
    /// signale par un [`StepError`] `retryable: false` — jamais par un panic.
    async fn run(
        &self,
        config: serde_json::Value,
        inputs: HashMap<String, serde_json::Value>,
    ) -> Result<serde_json::Value, StepError>;
}

/// Échec d'un step tel que le rend [`MiryadWorkflowStep::run`].
///
/// Erreur applicative du kind (potentiellement écrit par l'application consommatrice), jamais une
/// erreur interne de la crate : aucun code `MRD-*`, même contrat que `HookError`. Sa traduction
/// en retry ou en arrêt est du ressort exclusif du dispatcher interne.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct StepError {
    /// Description libre de l'échec, rendue telle quelle par l'affichage — choix délibéré : pas de
    /// champ `code` structuré, une codification des erreurs applicatives de workflow irait trop
    /// loin.
    pub message: String,
    /// `true` : échec transitoire (réseau, ressource momentanément indisponible) que le dispatcher
    /// doit laisser au retry ; `false` : échec permanent (config invalide, logique métier) qu'il
    /// doit arrêter immédiatement. Ce fichier ne lit jamais ce champ.
    pub retryable: bool,
}

/// Les kinds de step connus d'une application donnée, construits explicitement au démarrage et
/// consultés en lecture seule par le dispatcher interne via [`Self::dispatch`].
///
/// Aucun champ public, aucune découverte automatique, aucun registre global : identique au
/// pattern des autres registres de la crate (`IrRegistry`, `McpToolRegistry`).
#[derive(Default)]
pub struct StepRegistry {
    steps: HashMap<&'static str, Box<dyn MiryadWorkflowStep>>,
}

impl StepRegistry {
    /// Registre sans aucun kind enregistré.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ajoute un kind au registre et rend le registre pour chaînage. Possède le `step` (boîté en
    /// interne) : le registre vit typiquement du démarrage de l'app jusqu'à son arrêt.
    ///
    /// # Panics
    ///
    /// Panique si `step.kind()` collide avec un kind déjà enregistré. Erreur de configuration du
    /// démarrage de l'app, jamais une entrée utilisateur : le fail-fast est délibéré (un écrasement
    /// silencieux serait un bug de configuration masqué) — seul site `panic!` de ce fichier,
    /// exemption documentée par `step.sdd`.
    // Seul site `panic!` autorisé par ./step.sdd (`Must`, `Tasks`) : erreur de configuration au
    // démarrage de l'app, jamais une entrée utilisateur.
    pub fn register(&mut self, step: impl MiryadWorkflowStep + 'static) -> &mut Self {
        let kind = step.kind();
        // `assert!` plutôt que `if` + `panic!` : même panic fail-fast, même message, sans
        // déclencher `manual_assert` du harnais.
        assert!(
            !self.steps.contains_key(kind),
            "kind de step déjà enregistré : {kind}"
        );
        self.steps.insert(kind, Box::new(step));
        self
    }

    /// Retourne le futur de [`MiryadWorkflowStep::run`] du kind trouvé — jamais exécuté ici,
    /// c'est le dispatcher interne qui l'`.await` — ou un [`StepError`] `retryable: false`
    /// (`kind inconnu: {kind}`) quand aucun `impl` n'enregistre ce kind : référence absente du
    /// registre = erreur de configuration du DAG, jamais transitoire, donc jamais retryable.
    // Consommateur vivant = ./dispatcher.rs, à rédiger (step.sdd `References`/`Must`) : le
    // premier appelant de dispatch arrive avec ce batch, `dead_code` retiré alors.
    #[allow(dead_code)]
    pub(crate) fn dispatch(
        &self,
        kind: &str,
        config: serde_json::Value,
        inputs: HashMap<String, serde_json::Value>,
    ) -> Result<impl Future<Output = Result<serde_json::Value, StepError>>, StepError> {
        let step = self.steps.get(kind).ok_or_else(|| StepError {
            message: format!("kind inconnu: {kind}"),
            retryable: false,
        })?;
        Ok(step.run(config, inputs))
    }
}

#[cfg(test)]
mod tests {
    use super::{MiryadWorkflowStep, StepError, StepRegistry};
    use serde_json::{Value, json};
    use std::collections::HashMap;

    // ── Fixtures : un `impl` par kind, chaque kind identifiable par sa valeur de retour ──

    struct KindA;
    struct KindB;
    struct KindC;
    struct EchoConfig;
    struct EchoInputs;
    struct NoDeps;
    struct AlwaysTransient;
    struct AlwaysPermanent;
    struct DupFirst;
    struct DupSecond;
    struct PanicsInRun;

    macro_rules! fixture_kind {
        ($ty:ty, $kind:literal, $body:expr) => {
            #[async_trait::async_trait]
            impl MiryadWorkflowStep for $ty {
                fn kind(&self) -> &'static str {
                    $kind
                }
                async fn run(
                    &self,
                    config: Value,
                    inputs: HashMap<String, Value>,
                ) -> Result<Value, StepError> {
                    let run_body: fn(Value, HashMap<String, Value>) -> Result<Value, StepError> = $body;
                    run_body(config, inputs)
                }
            }
        };
    }

    fixture_kind!(KindA, "a", |_config: Value, _inputs: HashMap<String, Value>| {
        Ok(json!("valeur-a"))
    });
    fixture_kind!(KindB, "b", |_config: Value, _inputs: HashMap<String, Value>| {
        Ok(json!("valeur-b"))
    });
    fixture_kind!(KindC, "c", |_config: Value, _inputs: HashMap<String, Value>| {
        Ok(json!("valeur-c"))
    });
    fixture_kind!(
        EchoConfig,
        "echo",
        |config: Value, _inputs: HashMap<String, Value>| { Ok(config) }
    );
    fixture_kind!(
        EchoInputs,
        "echo_inputs",
        |_config: Value, inputs: HashMap<String, Value>| { Ok(Value::Object(inputs.into_iter().collect())) }
    );
    fixture_kind!(
        NoDeps,
        "no_deps",
        |_config: Value, inputs: HashMap<String, Value>| { Ok(json!(inputs.len())) }
    );
    fixture_kind!(AlwaysTransient, "toujours-transitoire", |_config: Value,
                                                            _inputs: HashMap<
        String,
        Value,
    >| {
        Err(StepError {
            message: "indisponible".to_string(),
            retryable: true,
        })
    });
    fixture_kind!(AlwaysPermanent, "toujours-permanent", |_config: Value,
                                                          _inputs: HashMap<
        String,
        Value,
    >| {
        Err(StepError {
            message: "config invalide".to_string(),
            retryable: false,
        })
    });
    fixture_kind!(
        DupFirst,
        "dupliqué",
        |_config: Value, _inputs: HashMap<String, Value>| { Ok(json!(null)) }
    );
    fixture_kind!(
        DupSecond,
        "dupliqué",
        |_config: Value, _inputs: HashMap<String, Value>| { Ok(json!(null)) }
    );
    // Kind volontairement défaillant (`Tasks` de ./step.sdd) : son `run` panique, verbatim, sans
    // `StepError`. Exemption `panic` du harnais déjà couverte par l'en-tête `cfg(test)` de
    // src/lib.rs — ici (test), et seulement ici, un panic est un outil de test.
    fixture_kind!(PanicsInRun, "panique-dans-run", |_config: Value,
                                                    _inputs: HashMap<
        String,
        Value,
    >| {
        panic!("panic délibéré du kind panique-dans-run")
    });

    /// Achemine un appel de bout en bout : `dispatch` (synchrone, qui choisit le kind ou refuse)
    /// puis exécution du futur rendu, comme le fera le dispatcher interne.
    async fn execute(
        registry: &StepRegistry,
        kind: &str,
        config: Value,
        inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        match registry.dispatch(kind, config, inputs) {
            Ok(future) => future.await,
            Err(error) => Err(error),
        }
    }

    fn no_inputs() -> HashMap<String, Value> {
        HashMap::new()
    }

    /// Scenario « dispatch vers le bon kind parmi plusieurs enregistrés » : avec `"a"`, `"b"` et
    /// `"c"` enregistrés, dispatch(`"b"`) exécute le `run` de `"b"` — ni `"a"`, ni `"c"`.
    #[tokio::test]
    async fn dispatch_rend_le_bon_kind_parmi_trois() {
        let mut registry = StepRegistry::new();
        registry.register(KindA).register(KindB).register(KindC);
        let rendu = execute(&registry, "b", json!(null), no_inputs())
            .await
            .expect("le kind « b » est enregistré, dispatch devait rendre un futur");
        assert_eq!(rendu, json!("valeur-b"));
        assert_ne!(rendu, json!("valeur-a"));
        assert_ne!(rendu, json!("valeur-c"));
    }

    /// Scenario « inputs transite intact du dépendant vers le kind » : deux entrées `"A"` et
    /// `"B"` traversent `dispatch` jusqu'au kind sans fusion avec `config`, renommage ni
    /// réordonnancement observable.
    #[tokio::test]
    async fn inputs_transite_intact_vers_le_kind() {
        let mut registry = StepRegistry::new();
        registry.register(EchoInputs);
        let mut inputs: HashMap<String, Value> = HashMap::new();
        inputs.insert("A".to_string(), json!(1));
        inputs.insert("B".to_string(), json!({ "x": true }));
        let rendu = execute(&registry, "echo_inputs", json!(null), inputs)
            .await
            .expect("le kind « echo_inputs » est enregistré");
        assert_eq!(rendu, json!({ "A": 1, "B": { "x": true } }));
    }

    /// Scenario « step sans dépendance reçoit un inputs vide, jamais absent » : `dispatch` avec un
    /// `HashMap::new()` parvient au kind comme un `HashMap` de longueur 0, pas comme une absence.
    #[tokio::test]
    async fn step_sans_dependance_reçoit_inputs_vide() {
        let mut registry = StepRegistry::new();
        registry.register(NoDeps);
        let rendu = execute(&registry, "no_deps", json!(null), no_inputs())
            .await
            .expect("le kind « no_deps » est enregistré");
        assert_eq!(rendu, json!(0));
    }

    /// Scenario « kind inconnu ne panique jamais » : un kind absent du registre est une erreur de
    /// configuration du DAG — `StepError { retryable: false, message: "kind inconnu: {kind}" }`
    /// rendu immédiatement, sans panic ni futur silencieux.
    #[test]
    fn kind_inconnu_rend_steperror_non_retryable_sans_panic() {
        let mut registry = StepRegistry::new();
        registry.register(KindA);
        match registry.dispatch("inexistant", json!(null), no_inputs()) {
            Err(error) => {
                assert_eq!(error.message, "kind inconnu: inexistant");
                assert!(!error.retryable, "un kind inconnu ne doit jamais être retryable");
                assert_eq!(error.to_string(), "kind inconnu: inexistant");
            }
            Ok(_) => panic!("le kind inconnu devait rendre un Err, jamais un futur"),
        }
    }

    /// Scenario « collision d'enregistrement panique au démarrage » : un second kind portant un
    /// `kind()` déjà présent interrompt le démarrage par `panic!` — fail-fast de configuration.
    #[test]
    #[should_panic(expected = "dupliqué")]
    fn collision_d_enregistrement_panique() {
        let mut registry = StepRegistry::new();
        registry.register(DupFirst);
        registry.register(DupSecond);
    }

    /// Scenario « `StepError` retryable distingue deux stratégies pour l'appelant » : deux kinds
    /// qui échouent, l'un `retryable: true`, l'autre `retryable: false` ; `dispatch` les rend
    /// fidèlement (message et booléens intacts) — la distinction n'est jamais lue ici.
    #[tokio::test]
    async fn steperror_retryable_transite_fidelement() {
        let mut registry = StepRegistry::new();
        registry.register(AlwaysTransient).register(AlwaysPermanent);
        let Err(transitoire) = execute(&registry, "toujours-transitoire", json!(null), no_inputs()).await
        else {
            panic!("le kind transitoire rend toujours une erreur");
        };
        assert_eq!(transitoire.message, "indisponible");
        assert!(transitoire.retryable);
        assert_eq!(transitoire.to_string(), "indisponible");
        let Err(permanent) = execute(&registry, "toujours-permanent", json!(null), no_inputs()).await else {
            panic!("le kind permanent rend toujours une erreur");
        };
        assert_eq!(permanent.message, "config invalide");
        assert!(!permanent.retryable);
        assert_eq!(permanent.to_string(), "config invalide");
    }

    /// Scenario « run reçoit la config telle quelle, sans validation préalable » : un `config`
    /// hétérogène traverse `dispatch` structurellement identique — aucune normalisation, aucun
    /// champ ajouté ou retiré par le chemin du registre.
    #[tokio::test]
    async fn config_transite_intacte_sans_validation() {
        let mut registry = StepRegistry::new();
        registry.register(EchoConfig);
        let config = json!({ "a": 1, "b": [true, null] });
        let rendu = execute(&registry, "echo", config.clone(), no_inputs())
            .await
            .expect("le kind « echo » est enregistré");
        assert_eq!(rendu, config);
    }

    /// Scenario (clause `Tasks` step.sdd — fixture « un kind qui panique délibérément ») : le
    /// `run` d'un kind peut paniquer, et ce panic traverse `dispatch` sans être capté ni converti
    /// en [`StepError`] — [`StepRegistry::dispatch`] n'en est jamais responsable (il choisit le
    /// kind et rend le futur, sans l'exécuter) ; seule [`StepRegistry::register`] a la garantie
    /// fail-fast.
    /// Le message du panic ressort verbatim : s'il était un jour avalé (`catch_unwind`) ou
    /// converti, ce test passe au rouge.
    #[tokio::test]
    #[should_panic(expected = "panic délibéré du kind panique-dans-run")]
    async fn le_panic_de_run_traverse_dispatch_sans_etre_converti() {
        let mut registry = StepRegistry::new();
        registry.register(PanicsInRun);
        // Étape 1 — `dispatch` lui-même ne panique pas : le kind est trouvé, le futur est rendu,
        // non exécuté. Si le registre convertissait le panic en `StepError` dès ce point,
        // `.expect` échouerait — ce serait une faute distincte, également rouge ici.
        let run_future = registry
            .dispatch("panique-dans-run", json!(null), no_inputs())
            .expect("le kind « panique-dans-run » est enregistré, dispatch doit rendre un futur");
        // Étape 2 — le panic éclate au poll du futur, tel que le kind l'a émis, sans conversion.
        let _ = run_future.await;
    }
}
