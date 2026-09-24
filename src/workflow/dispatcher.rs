//! Service Restate `StepDispatcher` (feature `workflow`) : exécution durable d'un step de
//! workflow, quel que soit son kind.
//!
//! Jointure entre le monde [`MiryadWorkflowStep`] (Rust pur, ./step.rs — qui ne connaît pas
//! Restate) et le protocole `restate-sdk` (./interpreter.rs, ce fichier) : les services
//! `restate-sdk` sont liés à la compilation (`Endpoint::builder().bind(...)`), aucun
//! enregistrement dynamique par kind n'est possible — un seul service existe pour tous les
//! kinds, la dynamique vit entièrement côté [`StepRegistry`] en Rust pur.

use std::collections::HashMap;

use restate_sdk::endpoint::ServiceOptions;
use restate_sdk::prelude::{Context, ContextSideEffects, HandlerError, Json, TerminalError, service};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::step::{StepError, StepRegistry};

/// Corps de requête de [`StepDispatcher::execute`] — forme sérialisée échangée avec
/// ./interpreter.rs via le protocole `restate-sdk`.
#[derive(Serialize, Deserialize)]
pub(crate) struct StepInvocation {
    kind: String,
    config: Value,
    inputs: HashMap<String, Value>,
}

/// L'unique service Restate du module : résout un kind dans le registre de l'app et enveloppe
/// son exécution dans exactement un `ctx.run()`.
pub struct StepDispatcher {
    registry: StepRegistry,
}

impl StepDispatcher {
    /// Attache le registre de kinds de l'app consommatrice au service ; l'app lie ensuite
    /// l'instance à son `Endpoint` (hors périmètre de la crate).
    #[must_use]
    pub fn new(registry: StepRegistry) -> Self {
        Self { registry }
    }
}

// `client_visibility = "pub(crate)"` : les clients générés par la macro pour invoquer ce service
// (XClient / XIngressClient) ne sortent jamais de la crate — ./interpreter.rs est leur seul
// consommateur (./dispatcher.sdd `Exposes` : « aucune fonction pub de logique métier en dehors
// de la forme service »). Le nom de service et `execute` sur le fil restent inchangés.
#[service(name = "StepDispatcher", client_visibility = "pub(crate)")]
impl StepDispatcher {
    #[handler]
    async fn execute(
        &self,
        ctx: Context<'_>,
        req: Json<StepInvocation>,
    ) -> Result<Json<Value>, HandlerError> {
        let Json(StepInvocation { kind, config, inputs }) = req;
        // Un seul `ctx.run()` par invocation : l'exécution du kind est journalisée d'un bloc,
        // jamais rejouée après reprise sur crash. La fermeture se borne à déléguer à
        // `run_invocation` (rien d'autre ne s'y ajoute). Jamais de `.retry_policy()` ici — la
        // politique est celle posée au `bind()` du service par l'app (`recommended_options`).
        let output = ctx
            .run(move || run_invocation(&self.registry, kind, config, inputs))
            .await?;
        Ok(output)
    }
}

/// Seul corps de la fermeture de `ctx.run` du handler : dispatch du kind, puis attente de son
/// futur. Séparaison tranchée par Sébastien (2026-09-23) : `restate-sdk` ne virtualise aucun
/// `Context`, cette fonction rend les trois branches (404 kind inconnu, transit intact du
/// résultat, exécution exactement une fois) testables sans serveur. L'invariant « un seul
/// `ctx.run()` par invocation » vit dans le handler, la logique ici.
async fn run_invocation(
    registry: &StepRegistry,
    kind: String,
    config: Value,
    inputs: HashMap<String, Value>,
) -> Result<Json<Value>, HandlerError> {
    // Err de `dispatch` = kind absent du registre : erreur de configuration du DAG (4xx),
    // jamais une panne du service (5xx). Code 404 posé explicitement, et jamais par
    // `step_error_to_handler_error`, réservée aux StepError d'un run.
    let found = registry.dispatch(&kind, config, inputs).map_err(|_| {
        HandlerError::from(TerminalError::new_with_code(404, format!("kind inconnu: {kind}")))
    })?;
    // Enveloppe `Json` posée ici (wrapper requis pour tout type non primitif traversant le
    // protocole `restate-sdk`) : la `Must` fixe le `run` en `Result<Json<Value>, TerminalError>`,
    // la sortie du run et la forme sur le fil restent le `Value` nu du kind.
    found.await.map(Json).map_err(step_error_to_handler_error)
}

/// Unique traduction `StepError` → erreurs `restate-sdk` de toute la crate.
fn step_error_to_handler_error(e: StepError) -> HandlerError {
    if e.retryable {
        // Blanket `From<E: Into<Box<dyn StdError + Send + Sync>>>` de `restate-sdk` : range
        // l'erreur en `HandlerErrorInner::Retryable`, Restate retente `execute` selon la
        // politique posée au `bind()` du service.
        HandlerError::from(e)
    } else {
        // Un StepError d'un kind trouvé est une panne applicative : TerminalError, code 500
        // par défaut. Un kind inconnu n'emprunte jamais ce chemin (`execute`).
        HandlerError::from(TerminalError::new(e.message))
    }
}

/// Politique de retry recommandée pour le `bind()` de [`StepDispatcher`] — jamais appliquée
/// par ce fichier, l'app en fait ce qu'elle veut.
#[must_use]
pub fn recommended_options() -> ServiceOptions {
    // Pas de retry permanent (décision Sébastien 2026-09-23, ./dispatcher.sdd `Must`) : au
    // 5e essai l'invocation passe en `paused`, relable manuellement (jamais `Kill`, jamais le
    // retry indéfini du serveur par absence de plafond).
    ServiceOptions::new()
        .retry_policy_max_attempts(5)
        .retry_policy_pause_on_max_attempts()
}

#[cfg(test)]
mod tests {
    use super::{recommended_options, run_invocation, step_error_to_handler_error};
    use crate::workflow::step::{MiryadWorkflowStep, StepError, StepRegistry};
    use restate_sdk::prelude::Json;
    use serde_json::Value;
    use serde_json::json;
    use std::collections::HashMap;
    use std::error::Error as StdError;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── Fixtures : kinds de registre, chaque Scenario pur monte son `StepRegistry` dédié ──

    /// Kind dont le `run` rend systématiquement un `StepError` `retryable: true` (scenario
    /// « step retryable devient `HandlerError` retryable » de ./dispatcher.sdd).
    struct Transient;
    /// Kind dont le `run` rend systématiquement un `StepError` `retryable: false` (scenario
    /// « step non-retryable devient `HandlerError` terminal »).
    struct Permanent;
    /// Kind dont le `run` retourne sa `config` inchangée (scenario « résultat du kind trouvé
    /// transite intact »).
    struct EchoConfig;
    /// Kind dont le `run` incrémente un compteur externe `Arc<AtomicUsize>` à chaque exécution
    /// (scenario « un seul `ctx.run()` par invocation de `execute` »).
    struct Compteur(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl MiryadWorkflowStep for Transient {
        fn kind(&self) -> &'static str {
            "transitoire"
        }
        async fn run(&self, _config: Value, _inputs: HashMap<String, Value>) -> Result<Value, StepError> {
            Err(StepError {
                message: "indisponible".to_string(),
                retryable: true,
            })
        }
    }

    #[async_trait::async_trait]
    impl MiryadWorkflowStep for Permanent {
        fn kind(&self) -> &'static str {
            "permanent"
        }
        async fn run(&self, _config: Value, _inputs: HashMap<String, Value>) -> Result<Value, StepError> {
            Err(StepError {
                message: "config invalide".to_string(),
                retryable: false,
            })
        }
    }

    #[async_trait::async_trait]
    impl MiryadWorkflowStep for EchoConfig {
        fn kind(&self) -> &'static str {
            "echo"
        }
        async fn run(&self, config: Value, _inputs: HashMap<String, Value>) -> Result<Value, StepError> {
            Ok(config)
        }
    }

    #[async_trait::async_trait]
    impl MiryadWorkflowStep for Compteur {
        fn kind(&self) -> &'static str {
            "compteur"
        }
        async fn run(&self, _config: Value, _inputs: HashMap<String, Value>) -> Result<Value, StepError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(json!(null))
        }
    }

    /// Résout le kind via `StepRegistry::dispatch` puis attend son futur, comme le fera le
    /// `run` du handler — le `StepError` rendu est celui que le dispatcher devra traduire.
    async fn step_error_of(registry: &StepRegistry, kind: &str) -> StepError {
        registry
            .dispatch(kind, json!(null), HashMap::new())
            .expect("le kind de fixture est enregistré")
            .await
            .expect_err("le kind de fixture rend toujours une erreur")
    }

    /// Scenario « step retryable devient `HandlerError` retryable » : la traduction passe par le
    /// blanket `From` de `HandlerError` (jamais par `TerminalError`) — observable au `Display`
    /// `HandlerErrorInner::Retryable`, préfixé `"Retryable error: "`, message d'origine intact.
    #[tokio::test]
    async fn step_retryable_devient_handlererror_retryable() {
        let mut registry = StepRegistry::new();
        registry.register(Transient);
        let erreur = step_error_of(&registry, "transitoire").await;
        assert!(erreur.retryable, "la fixture doit rendre un retryable: true");
        let traduit = step_error_to_handler_error(erreur);
        // `HandlerError` ne porte pas de `Display` propre : son observable public est
        // `AsRef<dyn StdError>` → `HandlerErrorInner`, seul à afficher le préfixe distinctif.
        let affiche = AsRef::<dyn StdError>::as_ref(&traduit).to_string();
        assert!(
            affiche.starts_with("Retryable error: "),
            "un retryable doit être rendu par HandlerErrorInner::Retryable, affiché : {affiche}"
        );
        assert!(
            affiche.contains("indisponible"),
            "le message d'origine doit subsister, affiché : {affiche}"
        );
    }

    /// Scenario « step non-retryable devient `HandlerError` terminal » : `TerminalError` de code
    /// `500`, message inchangé, jamais le préfixe `"Retryable error: "` au `Display`.
    #[tokio::test]
    async fn step_non_retryable_devient_handlererror_terminal() {
        let mut registry = StepRegistry::new();
        registry.register(Permanent);
        let erreur = step_error_of(&registry, "permanent").await;
        assert!(!erreur.retryable, "la fixture doit rendre un retryable: false");
        let traduit = step_error_to_handler_error(erreur);
        let affiche = AsRef::<dyn StdError>::as_ref(&traduit).to_string();
        assert!(
            affiche.contains("Terminal error [500]:"),
            "un non-retryable doit devenir une @TerminalError de code 500, affiché : {affiche}"
        );
        assert!(
            affiche.contains("config invalide"),
            "le message d'origine doit subsister, affiché : {affiche}"
        );
        assert!(
            !affiche.contains("Retryable error: "),
            "un terminal ne porte jamais le préfixe Retryable, affiché : {affiche}"
        );
    }

    /// Scenario « kind inconnu produit une erreur 404, distincte d'un `StepError` applicatif » :
    /// la `Must` « Kind inconnu » pose `404` explicitement (jamais le `500` du scénario
    /// précédent, jamais `step_error_to_handler_error`). Forme adaptée au seam `run_invocation`
    /// (tranchage Sébastien 2026-09-23, `Tasks` de ./dispatcher.sdd : « les tests appellent
    /// `run_invocation` directement » — un `execute` réel suppose un serveur Restate).
    #[tokio::test]
    async fn kind_inconnu_produit_une_erreur_404() {
        let registry = StepRegistry::new();
        let Err(erreur) = run_invocation(&registry, "absent".to_string(), Value::Null, HashMap::new()).await
        else {
            panic!("un kind absent du registre doit rendre une erreur, jamais Ok");
        };
        // `HandlerError` ne porte pas de `Display` propre : son observable public est
        // `AsRef<dyn StdError>` → `HandlerErrorInner`, seul à afficher le préfixe distinctif.
        let affiche = AsRef::<dyn StdError>::as_ref(&erreur).to_string();
        assert_eq!(
            affiche, "Terminal error [404]: kind inconnu: absent",
            "le code 404 explicite et le message verbatim « kind inconnu: absent » sont attendus"
        );
        assert!(
            !affiche.contains("Retryable error:"),
            "un kind absent du registre ne doit JAMAIS être Retryable (chemin 4xx, pas un StepError \
             retryable), affiche : {affiche}"
        );
    }

    /// Scenario « résultat du kind trouvé transite intact » : `run_invocation` avec kind `"echo"`
    /// dont le `run` retourne sa `config` inchangée, transite `Ok(Json(config))` structurellement
    /// identique — la jointure `dispatch` → await → `.map(Json)` ne modifie pas le `Value`.
    #[tokio::test]
    async fn resultat_du_kind_trouve_transite_intact() {
        let mut registry = StepRegistry::new();
        registry.register(EchoConfig);
        let rendu = run_invocation(&registry, "echo".to_string(), json!({"a": 1}), HashMap::new())
            .await
            .expect("le kind de fixture `echo` est enregistré");
        // `Json` (`restate_sdk::prelude::Json`) est un `newtype` `pub(crate)`-field sur le `Value`.
        let Json(valeur_transmise) = rendu;
        assert_eq!(
            valeur_transmise,
            json!({"a": 1}),
            "la config doit transiter sans enveloppe ni métadonnée ajoutée"
        );
    }

    /// Scenario « un seul `ctx.run()` par invocation de `execute` » : la forme testable du seam
    /// `run_invocation` (la logique du handler, extraite), un kind compteur externe
    /// `Arc<AtomicUsize>`. L'invariant « un seul `ctx.run()` par invocation » vit dans le handler
    /// (`Must` — le `ctx.run` du dispatcher est inobservable hors serveur) ; la démonstration du
    /// seam : exactement une fois `MiryadWorkflowStep::run` appelé par `run_invocation` — un
    /// second poll du futur (ou un `dispatch` double) serait une violation visible.
    #[tokio::test]
    async fn un_seul_appel_a_run_par_invocation() {
        let compteur = Arc::new(AtomicUsize::new(0));
        let mut registry = StepRegistry::new();
        registry.register(Compteur(Arc::clone(&compteur)));
        let _ = run_invocation(&registry, "compteur".to_string(), Value::Null, HashMap::new())
            .await
            .expect("le kind `compteur` est enregistré");
        let n = compteur.load(Ordering::SeqCst);
        assert_eq!(
            n, 1,
            "un appel à run_invocation doit exécuter le `run` du kind exactement une fois, \
             compteur : {n}"
        );
    }

    /// Scenario « `recommended_options` pose un plafond de tentatives et une pause, jamais un
    /// retry indéfini » : `Debug` est le seul point d'observation (champs `pub(crate)` dans
    /// `restate-sdk` `0.12.1`) ; plafond `5`, `Pause` au plafond, jamais `None` ni `Kill`.
    #[test]
    fn recommended_options_plafonne_tentatives_et_pause() {
        let rendu = format!("{:?}", recommended_options());
        assert!(
            rendu.contains("retry_policy_max_attempts: Some(5)"),
            "le plafond de tentatives doit être 5, rendu : {rendu}"
        );
        assert!(
            rendu.contains("retry_policy_on_max_attempts: Some(Pause)"),
            "l'atteinte du plafond doit mettre en pause, rendu : {rendu}"
        );
        assert!(
            !rendu.contains("retry_policy_max_attempts: None"),
            "aucun plafond (None) reproduirait le retry indéfini du serveur, rendu : {rendu}"
        );
        assert!(
            !rendu.contains("Some(Kill)"),
            "Kill empêcherait la relance manuelle voulue, rendu : {rendu}"
        );
    }
}
