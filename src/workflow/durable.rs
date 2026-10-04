//! Kinds de step « durables » (feature `workflow`) : [`MiryadDurableStep`], les kinds qui, au-delà
//! d'un calcul pur, **pilotent le moteur lui-même** — dormir durablement, journaliser leurs propres
//! effets de bord, lancer un sous-DAG enfant et en attendre la fin — et [`StepContext`], façade
//! volontairement étroite du `Context` Restate, remise par ./dispatcher.rs à l'appel de
//! [`MiryadDurableStep::run`].
//!
//! Un kind ordinaire ([`crate::workflow::step::MiryadWorkflowStep`]) s'exécute dans **un seul**
//! `ctx.run()` posé par ./dispatcher.rs : une fermeture `ctx.run()` ne peut appeler ni
//! `ctx.request`, ni `ctx.sleep` (limite du SDK `restate-sdk` `0.12.1`), donc elle ne peut ni
//! dormir durablement ni attendre un run enfant. Le dispatcher appelle donc un kind durable
//! **sans** `ctx.run()` englobant ; c'est le kind qui journalise ses effets via
//! [`StepContext::run_effect`].
//!
//! La surface de [`StepContext`] est un sous-ensemble choisi, pas le `Context` complet : une API
//! publique instable avant `1.0` ne doit pas être couplée à la version de `restate-sdk` ; chaque
//! capacité ajoutée est un engagement.

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use restate_sdk::context::RequestTarget;
use restate_sdk::prelude::{
    Context, ContextClient, ContextSideEffects, ContextTimers, HandlerError, Json, TerminalError,
};
use serde_json::Value;

use super::definition::DagSteps;
use super::step::{RunInfo, StepError};

/// Nom de l'en-tête de requête qui porte la profondeur d'imbrication d'un run enfant : écrit par
/// [`StepContext::run_child_dag`], lu par ./interpreter.rs (`parse_depth`). Une constante, jamais
/// deux littéraux (#26 — déplacée depuis ./interpreter.rs au lot B, valeur inchangée).
pub(crate) const DEPTH_HEADER: &str = "x-miryad-depth";

/// Clé de workflow Restate du run enfant porté par un step : `{run_key}:{step_id}`. Fonction pure
/// et déterministe — la même invocation parente rejouée recalcule la même clé, condition de
/// l'idempotence au rejeu (la clé de workflow est unique par run : jamais de doublon). Lisible
/// dans l'UI Restate : l'arbre des runs se reconstruit par préfixe de clé.
pub(crate) fn child_run_key(run_key: &str, step_id: &str) -> String {
    format!("{run_key}:{step_id}")
}

/// Un type de step de workflow **durable** : un kind qui pilote le moteur lui-même, là où un kind
/// ordinaire ([`crate::workflow::step::MiryadWorkflowStep`]) est enfermé dans un unique
/// `ctx.run()` sans accès au contexte. Le dispatcher interne
/// ([`crate::workflow::dispatcher::StepDispatcher`]) l'appelle **sans** `ctx.run()` englobant et
/// lui remet un [`StepContext`].
///
/// `Send + Sync` est un super-trait (même raison que [`crate::workflow::step::MiryadWorkflowStep`] :
/// un `impl` doit pouvoir vivre dans un `Box<dyn MiryadDurableStep>` traversant un handler async).
///
/// **Contrat de déterminisme.** L'invocation d'un kind durable est rejouée par Restate depuis son
/// journal à chaque reprise : tout appel non déterministe (horloge, aléa, I/O réseau ou base) fait
/// hors de [`StepContext::run_effect`] casse le rejeu — c'est au kind de journaliser lui-même ses
/// effets de bord. Une fermeture `run_effect` ne rappelle pas le `StepContext` (pas de
/// `run_effect` dans un `run_effect`, limite du SDK).
#[async_trait::async_trait]
pub trait MiryadDurableStep: Send + Sync {
    /// Clé stable du kind : clé du [`crate::workflow::step::StepRegistry`], dans le **même espace
    /// de noms** que les kinds ordinaires — un `kind()` déjà pris, par un kind ordinaire ou
    /// durable, est refusé au register
    /// ([`crate::workflow::step::StepRegistry::register_durable`]).
    fn kind(&self) -> &'static str;

    /// Exécute le step durable.
    ///
    /// `config` et `inputs` ont la même forme et la même provenance que pour
    /// [`crate::workflow::step::MiryadWorkflowStep::run`] ; l'identité du run se lit par
    /// [`StepContext::run_info`]. Le dispatcher n'enveloppe cet appel dans aucun `ctx.run()` :
    /// les effets de bord se journalisent via [`StepContext::run_effect`]. Un [`StepError`]
    /// `retryable: true` est laissé à la politique de retry du `bind()` du service, un
    /// `retryable: false` l'arrête — comme pour un kind ordinaire.
    async fn run(
        &self,
        ctx: &StepContext<'_>,
        config: serde_json::Value,
        inputs: HashMap<String, serde_json::Value>,
    ) -> Result<serde_json::Value, StepError>;
}

/// Façade étroite du `Context` Restate, remise à [`MiryadDurableStep::run`] par le dispatcher
/// interne — unique point de construction (new est `pub(crate)` : ./dispatcher.rs est le seul
/// constructeur, une application ne reçoit un `StepContext` que dans `run`, jamais elle n'en
/// construit un).
///
/// Opaque par contrat : `restate_sdk::Context` n'est pas exposé par cette surface (ni référence,
/// ni clone, ni accesseur) ; chaque capacité nouvelle passe par une méthode dédiée et un
/// `Scenario`/`Tasks` de ./durable.sdd.
pub struct StepContext<'a> {
    ctx: &'a Context<'a>,
    run: RunInfo,
}

impl<'a> StepContext<'a> {
    /// Constructeur unique, appelé par ./dispatcher.rs (branche durable de `execute`) ; jamais
    /// exposé hors de la crate.
    pub(crate) fn new(ctx: &'a Context<'a>, run: RunInfo) -> Self {
        Self { ctx, run }
    }

    /// L'identité du run en cours, en lecture seule (`run_key`, `step_id`, `depth` tels que
    /// transmis par le dispatcher, ./dispatcher.sdd).
    #[must_use]
    pub fn run_info(&self) -> &RunInfo {
        &self.run
    }

    /// Dort `duration`, de façon durable : le minute est tenu par Restate, une reprise après
    /// crash reprend au réveil prévu sans redormir. Délègue à `ctx.sleep` du SDK.
    ///
    /// # Errors
    ///
    /// [`StepError`] `retryable: false` dont le message est celui de la `TerminalError` du SDK
    /// (p. ex. annulation de l'invocation), conservé verbatim.
    pub async fn sleep(&self, duration: Duration) -> Result<(), StepError> {
        self.ctx.sleep(duration).await.map_err(|erreur| StepError {
            message: erreur.to_string(),
            retryable: false,
        })
    }

    /// Effet de bord journalisé : exécute `f` une fois et mémoïse son résultat dans le journal —
    /// après une reprise, la valeur déjà journalisée est rendue sans réexécuter la fermeture.
    ///
    /// Les bornes sont celles qu'impose `ctx.run` de `restate-sdk` `0.12.1` (fermeture et futur
    /// `Send`, valeur `'static`) ; l'enveloppe `Json` y est posée et retirée ici (le SDK ne
    /// fournit ses traits de sérialisation composites que derrière ce wrapper), l'appelant
    /// manipule `T` nu. Un [`StepError`] `retryable: true` de la fermeture est laissé à la
    /// politique de retry (aucune `.retry_policy()` posée ici, comme ./dispatcher.rs), un
    /// `retryable: false` devient une erreur terminale immédiate. Ne pas appeler `run_effect`
    /// dans la fermeture d'un autre `run_effect` (limite du SDK).
    ///
    /// # Errors
    ///
    /// [`StepError`] `retryable: false` avec le message de l'erreur terminale quand l'effet
    /// échoue définitivement — `retryable: false` rendu par la fermeture, ou épuisement des
    /// tentatives sur un `retryable: true`.
    pub async fn run_effect<T, F, Fut>(&self, f: F) -> Result<T, StepError>
    where
        T: serde::Serialize + serde::de::DeserializeOwned + Send + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, StepError>> + Send + 'static,
    {
        let Json(valeur) = self
            .ctx
            .run(move || async move {
                f().await.map(Json).map_err(|erreur| {
                    if erreur.retryable {
                        // Blanket `From` du SDK : HandlerError retryable, laissé à la politique
                        // du `bind()` (aucune `.retry_policy()` posée ici).
                        HandlerError::from(erreur)
                    } else {
                        HandlerError::from(TerminalError::new(erreur.message))
                    }
                })
            })
            .await
            .map_err(|erreur| StepError {
                message: erreur.to_string(),
                retryable: false,
            })?;
        Ok(valeur)
    }

    /// Lance `dag` comme run enfant de `DagInterpreter` et attend sa fin, **de façon idempotente
    /// au rejeu** : la clé de l'enfant est `child_run_key` du run et du step en cours, donc une
    /// invocation parente rejouée recalcule la même clé et Restate rattache l'appel au run enfant
    /// déjà démarré (jamais de doublon). L'en-tête `DEPTH_HEADER` porte la profondeur incrémentée
    /// (`run.depth + 1` en `saturating_add`).
    ///
    /// `dag` n'est pas pré-validé ici : `DagInterpreter::run` revalide systématiquement (un DAG
    /// invalide échoue en `400` `MRD-WORKFLOW-004`, ./interpreter.sdd).
    ///
    /// # Errors
    ///
    /// [`StepError`] `retryable: false` dont le message est celui de l'erreur terminale de
    /// l'enfant (DAG invalide, step en échec, annulation), conservé verbatim — un échec de
    /// l'enfant fait échouer le parent, le rejeu donnerait le même échec.
    pub async fn run_child_dag(&self, dag: &DagSteps) -> Result<HashMap<String, Value>, StepError> {
        let enfant = child_run_key(&self.run.run_key, &self.run.step_id);
        let Json(sorties) = self
            .ctx
            .request::<Json<DagSteps>, Json<HashMap<String, Value>>>(
                RequestTarget::workflow("DagInterpreter", enfant, "run"),
                Json(dag.clone()),
            )
            .header(
                DEPTH_HEADER.to_string(),
                self.run.depth.saturating_add(1).to_string(),
            )
            .call()
            .await
            .map_err(|erreur| StepError {
                message: erreur.to_string(),
                retryable: false,
            })?;
        Ok(sorties)
    }
}

#[cfg(test)]
mod tests {
    use super::child_run_key;
    use super::{MiryadDurableStep, StepContext};
    use crate::workflow::step::{StepError, StepRegistry};
    use serde_json::Value;
    use std::collections::HashMap;

    /// Scenario « `child_run_key` est déterministe et lisible » : la même entrée rend deux fois
    /// la même clé `"run-1:sous"` — condition de l'idempotence au rejeu (la clé de workflow
    /// Restate est unique par run, un rejeu ne crée jamais de doublon).
    #[test]
    fn child_run_key_est_déterministe_et_lisible() {
        let premier = child_run_key("run-1", "sous");
        let second = child_run_key("run-1", "sous");
        assert_eq!(premier, "run-1:sous");
        assert_eq!(premier, second, "la même entrée doit toujours rendre la même clé");
    }

    /// Scenario « deux steps du même run ont des clés enfants distinctes » : `"a"` et `"b"` du
    /// même `run_key` produisent deux clés différentes — deux steps d'un même DAG ne peuvent
    /// jamais entrer en collision sur la clé de leur run enfant.
    #[test]
    fn deux_steps_du_même_run_ont_des_clés_enfants_distinctes() {
        let a = child_run_key("run-1", "a");
        let b = child_run_key("run-1", "b");
        assert_ne!(
            a, b,
            "deux step_id distincts du même run doivent distinguer leurs enfants"
        );
    }

    // Fixture : un kind durable de `kind()` `"d"`. Son `run` n'est jamais appelé — aucun
    // `StepContext` n'est constructible en test (borne `restate-sdk` 0.12.1 : aucun `Context`
    // virtualisé), seule la branche durable de ./dispatcher.rs en construit un.
    struct DurableD;

    #[async_trait::async_trait]
    impl MiryadDurableStep for DurableD {
        fn kind(&self) -> &'static str {
            "d"
        }
        async fn run(
            &self,
            _ctx: &StepContext<'_>,
            _config: Value,
            _inputs: HashMap<String, Value>,
        ) -> Result<Value, StepError> {
            unreachable!("aucun StepContext constructible en test — ce run n'est jamais appelé")
        }
    }

    /// Scenario « un kind durable de fixture s'enregistre et se retrouve par `durable()` » :
    /// `register_durable` accepte la fixture, `durable("d")` la rend avec son `kind()` — preuve
    /// que le trait est dyn-compatible (`Box<dyn MiryadDurableStep>` dans le registre).
    #[test]
    fn un_kind_durable_s_enregistre_et_se_retrouve_par_durable() {
        let mut registry = StepRegistry::new();
        registry
            .register_durable(DurableD)
            .expect("un kind durable neuf doit s'enregistrer");
        let trouvé = registry
            .durable("d")
            .expect("le kind durable « d » est enregistré");
        assert_eq!(trouvé.kind(), "d");
        // Le témoin explicite de la dyn-compatibilité demandée par le Scenario.
        let _: Box<dyn MiryadDurableStep> = Box::new(DurableD);
    }
}
