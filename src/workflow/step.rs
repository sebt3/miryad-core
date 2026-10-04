//! Point d'extension des kinds de step de workflow (feature `workflow`) : un `impl` du trait
//! [`MiryadWorkflowStep`] par kind, fourni par l'application consommatrice, et [`StepRegistry`],
//! la collection des kinds connus qu'elle construit explicitement au démarrage.
//!
//! Ce fichier ne parle jamais Restate : la traduction d'un [`StepError`] en erreur de handler ou
//! en arrêt définitif est du ressort du dispatcher interne (`./dispatcher.rs`), et un kind
//! s'écrit, se lit et se teste comme une fonction async ordinaire, sans dépendre du protocole
//! d'invocation Restate.

use std::collections::HashMap;

use super::durable::MiryadDurableStep;
use super::error::WorkflowError;

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
    /// `run` (#26, 2026-10-04, **changement cassant du trait** — API instable avant `1.0`) est
    /// l'identité du run en lecture seule, assemblée par ./interpreter.rs et transmise par
    /// ./dispatcher.rs ; un kind qui n'en a pas besoin l'ignore (`_run`). `config` est le contenu
    /// opaque du champ `config` du `StepDefinition` de ce kind, reçu tel
    /// quel, jamais interprété ni validé ici. `inputs` porte une entrée par step amont déclaré
    /// dans `depends_on` (clé = id du step amont, valeur = la sortie que son propre `run` a
    /// rendue) ; un step sans dépendance reçoit un `HashMap` vide, jamais une absence. La valeur
    /// rendue est republiée par le moteur dans les `inputs` des steps qui déclarent celui-ci.
    ///
    /// Un kind qui reçoit une `config` ou un `inputs` dans une forme qu'il n'attend pas le
    /// signale par un [`StepError`] `retryable: false` — jamais par un panic.
    async fn run(
        &self,
        run: &RunInfo,
        config: serde_json::Value,
        inputs: HashMap<String, serde_json::Value>,
    ) -> Result<serde_json::Value, StepError>;
}

/// Identité du run en cours, telle que reçue par [`MiryadWorkflowStep::run`] (#26, 2026-10-04).
///
/// Donnée pure, sans aucune dépendance à `restate-sdk` : assemblée par ./interpreter.rs (clé de
/// workflow Restate, `id` du `StepDefinition`, profondeur lue de l'en-tête d'invocation) et
/// transmise telle quelle par ./dispatcher.rs. Fournie à **tous** les kinds, durables ou non ;
/// un kind qui n'en a pas besoin l'ignore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInfo {
    /// Clé du run en cours : la clé de workflow Restate de l'invocation de `DagInterpreter` qui
    /// exécute ce step — pour un run enfant, la clé dérivée par le parent (./durable.sdd).
    pub run_key: String,
    /// `id` du [`crate::workflow::StepDefinition`] en cours d'exécution, unique dans son DAG.
    pub step_id: String,
    /// Profondeur d'imbrication des sous-workflows : `0` pour un run déclenché par
    /// `client::trigger_run`, `+1` à chaque niveau de sous-workflow (./subworkflow.sdd).
    pub depth: u32,
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
/// consultés en lecture seule par le dispatcher interne via `Self::dispatch` et `Self::durable`.
///
/// Deux magasins internes (ordinaires, durables) sous **un seul espace de noms** : un `kind()`
/// ne peut être pris que par un seul kind, durable ou ordinaire (#26, ./durable.sdd).
///
/// Aucun champ public, aucune découverte automatique, aucun registre global : identique au
/// pattern des autres registres de la crate (`IrRegistry`, `McpToolRegistry`).
#[derive(Default)]
pub struct StepRegistry {
    steps: HashMap<&'static str, Box<dyn MiryadWorkflowStep>>,
    durables: HashMap<&'static str, Box<dyn MiryadDurableStep>>,
}

impl StepRegistry {
    /// Registre sans aucun kind enregistré.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ajoute un kind ordinaire au registre et rend le registre pour chaînage. Possède le `step`
    /// (boîté en interne) : le registre vit typiquement du démarrage de l'app jusqu'à son arrêt.
    ///
    /// # Errors
    ///
    /// [`WorkflowError::DuplicateStepKind`] (`MRD-WORKFLOW-006`) si `step.kind()` collide avec un
    /// kind déjà enregistré, **ordinaire ou durable** (#26 — un seul espace de noms) : erreur de
    /// configuration du démarrage de l'app, jamais une entrée utilisateur. Rien n'est inséré, le
    /// premier kind reste celui du registre — un écrasement silencieux serait un bug de
    /// configuration masqué.
    pub fn register(&mut self, step: impl MiryadWorkflowStep + 'static) -> Result<&mut Self, WorkflowError> {
        let kind = step.kind();
        if self.steps.contains_key(kind) || self.durables.contains_key(kind) {
            return Err(WorkflowError::DuplicateStepKind(kind.to_string()));
        }
        self.steps.insert(kind, Box::new(step));
        Ok(self)
    }

    /// Ajoute un kind **durable** (./durable.sdd) au registre et rend le registre pour chaînage,
    /// dans le même espace de noms que [`Self::register`] : le magasin durable est distinct, la
    /// clé est partagée.
    ///
    /// # Errors
    ///
    /// [`WorkflowError::DuplicateStepKind`] (`MRD-WORKFLOW-006`) si `step.kind()` est déjà pris,
    /// par un kind ordinaire **ou** durable : rien n'est inséré, le premier kind reste celui du
    /// registre — même contrat de configuration que [`Self::register`] (#26).
    pub fn register_durable(
        &mut self,
        step: impl MiryadDurableStep + 'static,
    ) -> Result<&mut Self, WorkflowError> {
        let kind = step.kind();
        if self.steps.contains_key(kind) || self.durables.contains_key(kind) {
            return Err(WorkflowError::DuplicateStepKind(kind.to_string()));
        }
        self.durables.insert(kind, Box::new(step));
        Ok(self)
    }

    /// Le kind durable enregistré sous `kind`, ou `None` — seul point d'entrée de ./dispatcher.rs
    /// pour les kinds durables, consulté **avant** [`Self::dispatch`]. Un kind durable ne se
    /// résout jamais par `dispatch` : les deux magasins sont disjoints, seul l'espace de noms des
    /// clés est partagé (#26, ./durable.sdd).
    pub(crate) fn durable(&self, kind: &str) -> Option<&dyn MiryadDurableStep> {
        self.durables.get(kind).map(|boîte| &**boîte)
    }

    /// Retourne le futur de [`MiryadWorkflowStep::run`] du kind **ordinaire** trouvé — jamais
    /// exécuté ici, c'est le dispatcher interne qui l'`.await` — ou un [`StepError`]
    /// `retryable: false` (`kind inconnu: {kind}`) quand aucun `impl` ordinaire n'enregistre ce
    /// kind : référence absente du registre = erreur de configuration du DAG, jamais transitoire,
    /// donc jamais retryable. Un kind **durable** enregistré ne se résout pas ici : `dispatch` ne
    /// regarde que le magasin ordinaire, [`Self::durable`] l'autre (#26). `run` (#26, 2026-10-04)
    /// est l'identité du run, relayée au `run` du kind sans être lue ici.
    /// Capture de durée explicite (`use<'r>`, Hypothesis `'s: 'r`) : sous `#[async_trait]`, le
    /// futur rendu par le `run` d'un `dyn MiryadWorkflowStep` emprunte `self` et `run` — sans
    /// elle, `impl Future` ne peut pas les capturer (`E0700`, édition 2024) ; `'s: 'r` borne la
    /// durée de l'emprunt de `self` sur celle de `run`, unique durée que le futur a besoin de
    /// nommer.
    pub(crate) fn dispatch<'s: 'r, 'r>(
        &'s self,
        kind: &str,
        run: &'r RunInfo,
        config: serde_json::Value,
        inputs: HashMap<String, serde_json::Value>,
    ) -> Result<impl Future<Output = Result<serde_json::Value, StepError>> + use<'r>, StepError> {
        let step = self.steps.get(kind).ok_or_else(|| StepError {
            message: format!("kind inconnu: {kind}"),
            retryable: false,
        })?;
        Ok(step.run(run, config, inputs))
    }
}

#[cfg(test)]
mod tests {
    use super::{MiryadWorkflowStep, RunInfo, StepError, StepRegistry};
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
    struct RunIdentity;

    macro_rules! fixture_kind {
        ($ty:ty, $kind:literal, $body:expr) => {
            #[async_trait::async_trait]
            impl MiryadWorkflowStep for $ty {
                fn kind(&self) -> &'static str {
                    $kind
                }
                async fn run(
                    &self,
                    run: &RunInfo,
                    config: Value,
                    inputs: HashMap<String, Value>,
                ) -> Result<Value, StepError> {
                    let run_body: fn(&RunInfo, Value, HashMap<String, Value>) -> Result<Value, StepError> =
                        $body;
                    run_body(run, config, inputs)
                }
            }
        };
    }

    fixture_kind!(
        KindA,
        "a",
        |_run: &RunInfo, _config: Value, _inputs: HashMap<String, Value>| { Ok(json!("valeur-a")) }
    );
    fixture_kind!(
        KindB,
        "b",
        |_run: &RunInfo, _config: Value, _inputs: HashMap<String, Value>| { Ok(json!("valeur-b")) }
    );
    fixture_kind!(
        KindC,
        "c",
        |_run: &RunInfo, _config: Value, _inputs: HashMap<String, Value>| { Ok(json!("valeur-c")) }
    );
    fixture_kind!(
        EchoConfig,
        "echo",
        |_run: &RunInfo, config: Value, _inputs: HashMap<String, Value>| { Ok(config) }
    );
    fixture_kind!(
        EchoInputs,
        "echo_inputs",
        |_run: &RunInfo, _config: Value, inputs: HashMap<String, Value>| {
            Ok(Value::Object(inputs.into_iter().collect()))
        }
    );
    fixture_kind!(
        NoDeps,
        "no_deps",
        |_run: &RunInfo, _config: Value, inputs: HashMap<String, Value>| { Ok(json!(inputs.len())) }
    );
    fixture_kind!(AlwaysTransient, "toujours-transitoire", |_run: &RunInfo,
                                                            _config: Value,
                                                            _inputs: HashMap<
        String,
        Value,
    >| {
        Err(StepError {
            message: "indisponible".to_string(),
            retryable: true,
        })
    });
    fixture_kind!(AlwaysPermanent, "toujours-permanent", |_run: &RunInfo,
                                                          _config: Value,
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
        |_run: &RunInfo, _config: Value, _inputs: HashMap<String, Value>| { Ok(json!("premier")) }
    );
    fixture_kind!(
        DupSecond,
        "dupliqué",
        |_run: &RunInfo, _config: Value, _inputs: HashMap<String, Value>| { Ok(json!(null)) }
    );
    // Kind volontairement défaillant (`Tasks` de ./step.sdd) : son `run` panique, verbatim, sans
    // `StepError`. Exemption `panic` du harnais déjà couverte par l'en-tête `cfg(test)` de
    // src/lib.rs — ici (test), et seulement ici, un panic est un outil de test.
    fixture_kind!(PanicsInRun, "panique-dans-run", |_run: &RunInfo,
                                                    _config: Value,
                                                    _inputs: HashMap<
        String,
        Value,
    >| {
        panic!("panic délibéré du kind panique-dans-run")
    });
    // Kind du Scenario #26 : rend les trois champs de la `RunInfo` reçue, preuve de transit intact.
    fixture_kind!(
        RunIdentity,
        "echo_run",
        |run: &RunInfo, _config: Value, _inputs: HashMap<String, Value>| {
            Ok(json!({ "run_key": run.run_key, "step_id": run.step_id, "depth": run.depth }))
        }
    );

    /// Achemine un appel de bout en bout : `dispatch` (synchrone, qui choisit le kind ou refuse)
    /// puis exécution du futur rendu, comme le fera le dispatcher interne.
    async fn execute(
        registry: &StepRegistry,
        kind: &str,
        run: &RunInfo,
        config: Value,
        inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        match registry.dispatch(kind, run, config, inputs) {
            Ok(future) => future.await,
            Err(error) => Err(error),
        }
    }

    fn no_inputs() -> HashMap<String, Value> {
        HashMap::new()
    }

    /// `RunInfo` neutre pour les tests qui n'exercent pas son contenu (seul le Scenario #26
    /// valide une identité précise).
    fn run_info() -> RunInfo {
        RunInfo {
            run_key: "t".into(),
            step_id: "s".into(),
            depth: 0,
        }
    }

    /// Scenario « dispatch vers le bon kind parmi plusieurs enregistrés » : avec `"a"`, `"b"` et
    /// `"c"` enregistrés, dispatch(`"b"`) exécute le `run` de `"b"` — ni `"a"`, ni `"c"`.
    #[tokio::test]
    async fn dispatch_rend_le_bon_kind_parmi_trois() {
        let mut registry = StepRegistry::new();
        registry
            .register(KindA)
            .and_then(|r| r.register(KindB))
            .and_then(|r| r.register(KindC))
            .expect("kinds distincts");
        let rendu = execute(&registry, "b", &run_info(), json!(null), no_inputs())
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
        registry.register(EchoInputs).expect("kind distinct");
        let mut inputs: HashMap<String, Value> = HashMap::new();
        inputs.insert("A".to_string(), json!(1));
        inputs.insert("B".to_string(), json!({ "x": true }));
        let rendu = execute(&registry, "echo_inputs", &run_info(), json!(null), inputs)
            .await
            .expect("le kind « echo_inputs » est enregistré");
        assert_eq!(rendu, json!({ "A": 1, "B": { "x": true } }));
    }

    /// Scenario « step sans dépendance reçoit un inputs vide, jamais absent » : `dispatch` avec un
    /// `HashMap::new()` parvient au kind comme un `HashMap` de longueur 0, pas comme une absence.
    #[tokio::test]
    async fn step_sans_dependance_reçoit_inputs_vide() {
        let mut registry = StepRegistry::new();
        registry.register(NoDeps).expect("kind distinct");
        let rendu = execute(&registry, "no_deps", &run_info(), json!(null), no_inputs())
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
        registry.register(KindA).expect("kind distinct");
        match registry.dispatch("inexistant", &run_info(), json!(null), no_inputs()) {
            Err(error) => {
                assert_eq!(error.message, "kind inconnu: inexistant");
                assert!(!error.retryable, "un kind inconnu ne doit jamais être retryable");
                assert_eq!(error.to_string(), "kind inconnu: inexistant");
            }
            Ok(_) => panic!("le kind inconnu devait rendre un Err, jamais un futur"),
        }
    }

    /// Scenario « collision d'enregistrement rend une erreur au démarrage » : un second kind
    /// portant un `kind()` déjà présent rend `DuplicateStepKind` sans écraser le premier.
    #[tokio::test]
    async fn collision_d_enregistrement_rend_une_erreur() {
        let mut registry = StepRegistry::new();
        registry.register(DupFirst).expect("premier enregistrement");
        let Err(erreur) = registry.register(DupSecond) else {
            panic!("un kind dupliqué devait rendre un Err");
        };
        assert!(
            matches!(&erreur, crate::workflow::error::WorkflowError::DuplicateStepKind(k) if k == "dupliqué"),
            "variante attendue DuplicateStepKind(\"dupliqué\") : {erreur:?}"
        );
        assert_eq!(
            erreur.to_string(),
            "MRD-WORKFLOW-006: step kind already registered: dupliqué"
        );
        // Le premier kind reste celui enregistré.
        let run = run_info();
        let Ok(futur) = registry.dispatch("dupliqué", &run, json!(null), no_inputs()) else {
            panic!("le kind dupliqué devait rester enregistré");
        };
        assert_eq!(futur.await.ok(), Some(json!("premier")));
    }

    /// Scenario « `StepError` retryable distingue deux stratégies pour l'appelant » : deux kinds
    /// qui échouent, l'un `retryable: true`, l'autre `retryable: false` ; `dispatch` les rend
    /// fidèlement (message et booléens intacts) — la distinction n'est jamais lue ici.
    #[tokio::test]
    async fn steperror_retryable_transite_fidelement() {
        let mut registry = StepRegistry::new();
        registry
            .register(AlwaysTransient)
            .and_then(|r| r.register(AlwaysPermanent))
            .expect("kinds distincts");
        let Err(transitoire) = execute(
            &registry,
            "toujours-transitoire",
            &run_info(),
            json!(null),
            no_inputs(),
        )
        .await
        else {
            panic!("le kind transitoire rend toujours une erreur");
        };
        assert_eq!(transitoire.message, "indisponible");
        assert!(transitoire.retryable);
        assert_eq!(transitoire.to_string(), "indisponible");
        let Err(permanent) = execute(
            &registry,
            "toujours-permanent",
            &run_info(),
            json!(null),
            no_inputs(),
        )
        .await
        else {
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
        registry.register(EchoConfig).expect("kind distinct");
        let config = json!({ "a": 1, "b": [true, null] });
        let rendu = execute(&registry, "echo", &run_info(), config.clone(), no_inputs())
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
        registry.register(PanicsInRun).expect("kind distinct");
        // Étape 1 — `dispatch` lui-même ne panique pas : le kind est trouvé, le futur est rendu,
        // non exécuté. Si le registre convertissait le panic en `StepError` dès ce point,
        // `.expect` échouerait — ce serait une faute distincte, également rouge ici.
        let run = run_info();
        let run_future = registry
            .dispatch("panique-dans-run", &run, json!(null), no_inputs())
            .expect("le kind « panique-dans-run » est enregistré, dispatch doit rendre un futur");
        // Étape 2 — le panic éclate au poll du futur, tel que le kind l'a émis, sans conversion.
        let _ = run_future.await;
    }

    // ── Fixtures #26 lot B : un kind durable et un kind ordinaire partageant l'espace de noms ──

    /// Kind durable de fixture (Scenario « un kind durable enregistré ne se résout pas par
    /// `dispatch` ») : son `run` n'est jamais appelé ici — aucun `StepContext` constructible en
    /// test (borne `restate-sdk` `0.12.1`, ./durable.sdd).
    struct DurableD;
    /// Kind ordinaire `"x"` du Scenario « collision d'enregistrement entre kind ordinaire et kind
    /// durable » : identifiable par sa sortie propre, prouvant « sans écraser le premier ».
    struct OrdinaireX;
    /// Kind durable `"x"` du même Scenario — même `kind()` que [`OrdinaireX`], sous le même
    /// `StepRegistry` ou l'autre selon le sens testé.
    struct DurableX;

    #[async_trait::async_trait]
    impl crate::workflow::durable::MiryadDurableStep for DurableD {
        fn kind(&self) -> &'static str {
            "d"
        }
        async fn run(
            &self,
            _ctx: &crate::workflow::durable::StepContext<'_>,
            _config: Value,
            _inputs: HashMap<String, Value>,
        ) -> Result<Value, StepError> {
            unreachable!("aucun StepContext constructible en test — ce run n'est jamais appelé")
        }
    }

    #[async_trait::async_trait]
    impl crate::workflow::durable::MiryadDurableStep for DurableX {
        fn kind(&self) -> &'static str {
            "x"
        }
        async fn run(
            &self,
            _ctx: &crate::workflow::durable::StepContext<'_>,
            _config: Value,
            _inputs: HashMap<String, Value>,
        ) -> Result<Value, StepError> {
            unreachable!("aucun StepContext constructible en test — ce run n'est jamais appelé")
        }
    }

    fixture_kind!(
        OrdinaireX,
        "x",
        |_run: &RunInfo, _config: Value, _inputs: HashMap<String, Value>| { Ok(json!("ordinaire")) }
    );

    /// Scenario « un kind durable enregistré ne se résout pas par `dispatch` » (#26) : `durable`
    /// et `dispatch` consultent des magasins disjoints — `durable("d")` rend `Some`,
    /// `dispatch("d", ..)` rend le `StepError` « kind inconnu » non retryable comme pour un kind
    /// jamais enregistré, `durable("inconnu")` rend `None`.
    #[test]
    fn un_kind_durable_ne_se_résout_pas_par_dispatch() {
        let mut registry = StepRegistry::new();
        registry
            .register_durable(DurableD)
            .expect("kind durable distinct");
        assert!(registry.durable("d").is_some(), "durable(\"d\") doit rendre Some");
        assert!(
            registry.durable("inconnu").is_none(),
            "durable(\"inconnu\") doit rendre None"
        );
        match registry.dispatch("d", &run_info(), json!(null), no_inputs()) {
            Err(error) => {
                assert_eq!(error.message, "kind inconnu: d");
                assert!(
                    !error.retryable,
                    "un kind durable non résolu par dispatch reste une erreur de configuration, \
                     jamais retryable"
                );
            }
            Ok(_) => panic!("un kind durable ne doit jamais se résoudre par dispatch"),
        }
    }

    /// Scenario « collision d'enregistrement entre kind ordinaire et kind durable » (#26) : un
    /// `kind()` déjà pris, dans l'un ou l'autre sens (ordinaire puis durable, durable puis
    /// ordinaire), rend `DuplicateStepKind` sans rien insérer ni écraser — les deux magasins
    /// partagent un seul espace de noms.
    #[tokio::test]
    async fn collision_croisée_entre_ordinaire_et_durable_rend_une_erreur() {
        // Sens ordinaire d'abord : le durable refusé, l'ordinaire reste résolvable par dispatch.
        let mut registry = StepRegistry::new();
        registry
            .register(OrdinaireX)
            .expect("premier enregistrement ordinaire");
        let Err(erreur) = registry.register_durable(DurableX) else {
            panic!("un kind durable reprenant un kind ordinaire doit rendre un Err");
        };
        assert!(
            matches!(&erreur, crate::workflow::error::WorkflowError::DuplicateStepKind(k) if k == "x"),
            "variante attendue DuplicateStepKind(\"x\") : {erreur:?}"
        );
        let run = run_info();
        let futur = registry
            .dispatch("x", &run, json!(null), no_inputs())
            .expect("le premier kind ordinaire doit rester enregistré");
        assert_eq!(futur.await.ok(), Some(json!("ordinaire")));
        assert!(
            registry.durable("x").is_none(),
            "le durable refusé ne doit rien avoir inséré"
        );

        // Sens durable d'abord : l'ordinaire refusé, le durable reste résolvable par durable().
        let mut registry = StepRegistry::new();
        registry
            .register_durable(DurableX)
            .expect("premier enregistrement durable");
        let Err(erreur) = registry.register(OrdinaireX) else {
            panic!("un kind ordinaire reprenant un kind durable doit rendre un Err");
        };
        assert!(
            matches!(&erreur, crate::workflow::error::WorkflowError::DuplicateStepKind(k) if k == "x"),
            "variante attendue DuplicateStepKind(\"x\") : {erreur:?}"
        );
        let trouvé = registry
            .durable("x")
            .expect("le premier kind durable doit rester enregistré");
        assert_eq!(trouvé.kind(), "x");
        assert!(
            registry
                .dispatch("x", &run_info(), json!(null), no_inputs())
                .is_err(),
            "l'ordinaire refusé ne doit rien avoir inséré"
        );
    }

    /// Scenario « `RunInfo` est transmis intact au kind » (#26) : `dispatch` avec
    /// `RunInfo { run_key: "r1", step_id: "s1", depth: 2 }` fait rendre au kind les trois champs
    /// exacts — le registre ne modifie, ne normalise ni ne perd aucune des trois valeurs.
    #[tokio::test]
    async fn run_info_transite_intacte_vers_le_kind() {
        let mut registry = StepRegistry::new();
        registry.register(RunIdentity).expect("kind distinct");
        let run = RunInfo {
            run_key: "r1".to_string(),
            step_id: "s1".to_string(),
            depth: 2,
        };
        let rendu = execute(&registry, "echo_run", &run, json!(null), no_inputs())
            .await
            .expect("le kind « echo_run » est enregistré");
        assert_eq!(rendu, json!({ "run_key": "r1", "step_id": "s1", "depth": 2 }));
    }
}
