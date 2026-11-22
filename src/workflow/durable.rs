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

use restate_sdk::context::{ContextAwakeables, RequestTarget};
use restate_sdk::prelude::{
    Context, ContextClient, ContextSideEffects, ContextTimers, HandlerError, Json, TerminalError,
};
use serde_json::Value;

use super::definition::DagSteps;
use super::interpreter::RunInput;
use super::step::{RunInfo, StepError};

/// Nom de l'en-tête de requête qui porte la profondeur d'imbrication d'un run enfant : écrit par
/// [`StepContext::run_child_dag`], lu par ./interpreter.rs (`parse_depth`). Une constante, jamais
/// deux littéraux (#26 — déplacée depuis ./interpreter.rs au lot B, valeur inchangée).
pub(crate) const DEPTH_HEADER: &str = "x-miryad-depth";

/// Message exact rendu par [`StepContext::awakeable_timeout`] quand le minuteur durable échoit
/// avant toute résolution (amendement #26 du 2026-11-14). Un kind qui doit distinguer délai,
/// rejet et annulation compare ce texte (arbitré : `@StepError` inchangé, aucun genre d'erreur
/// nouveau) ; sa forme exacte — sans identifiant ni durée — est verrouillée par un test unitaire.
pub(crate) const AWAKEABLE_TIMEOUT_MESSAGE: &str = "awakeable timed out";

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

    /// Clés de premier niveau de `config` que le rendu de template central ne doit jamais toucher
    /// (./template.sdd), même sémantique et même contrat que
    /// [`crate::workflow::step::MiryadWorkflowStep::raw_fields`]. Défaut : aucune clé. Le rendu d'un
    /// kind durable est journalisé avant `run` (./dispatcher.sdd) : `run` reçoit une `config` déjà
    /// rendue.
    fn raw_fields(&self) -> &'static [&'static str] {
        &[]
    }
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

    /// L'identité du run en cours, en lecture seule (`run_key`, `step_id`, `depth`, `vars`,
    /// `trigger` tels que transmis par le dispatcher, ./dispatcher.sdd).
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
    /// Le corps de la requête est un `RunInput` `{ dag, vars, trigger }` (./interpreter.rs,
    /// amendements lot B et lot 0 lymaille, ./interpreter.sdd) : `vars` sont les variables
    /// constantes de l'enfant, telles quelles (le subworkflow les a déjà fait rendre par son
    /// parent, il n'y a pas de second rendu ici) ; `trigger` est **hérité du parent** —
    /// `self.run.trigger` cloné (amendement 2026-11-02, `FR-10`/`DEC-12`) : le run enfant a
    /// l'identité du déclencheur du parent, sans autre canal ni paramètre.
    ///
    /// `dag` n'est pas pré-validé ici : `DagInterpreter::run` revalide systématiquement (un DAG
    /// invalide échoue en `400` `MRD-WORKFLOW-004`, ./interpreter.sdd).
    ///
    /// # Errors
    ///
    /// [`StepError`] `retryable: false` dont le message est celui de l'erreur terminale de
    /// l'enfant (DAG invalide, step en échec, annulation), conservé verbatim — un échec de
    /// l'enfant fait échouer le parent, le rejeu donnerait le même échec.
    pub async fn run_child_dag(
        &self,
        dag: &DagSteps,
        vars: &serde_json::Map<String, Value>,
    ) -> Result<HashMap<String, Value>, StepError> {
        let enfant = child_run_key(&self.run.run_key, &self.run.step_id);
        let Json(sorties) = self
            .ctx
            .request::<Json<RunInput>, Json<HashMap<String, Value>>>(
                RequestTarget::workflow("DagInterpreter", enfant, "run"),
                Json(RunInput {
                    dag: dag.clone(),
                    vars: vars.clone(),
                    trigger: self.run.trigger.clone(),
                }),
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

    /// Crée un **awakeable** (issue #26) : rend son identifiant et le futur de l'attente.
    ///
    /// L'identifiant est une entrée du journal : après reprise, le **même** identifiant est rendu
    /// (jamais recalculé ni régénéré). L'appelant le transmet au travail externe **par un effet
    /// journalisé** ([`StepContext::run_effect`]) — jamais directement dans `run`, sinon une
    /// reprise rejouerait la remise (doublon du travail externe). Ordre prescrit : `awakeable` →
    /// `run_effect` (remise) → attente du futur.
    ///
    /// L'identifiant est un **secret porteur** : quiconque le détient peut résoudre l'awakeable
    /// (`POST {ingress}/restate/awakeables/{id}/resolve`). Il n'est ni tracé ni placé dans une
    /// [`StepError`] par ce fichier ; sa confidentialité est l'affaire du kind qui le remet. La
    /// valeur résolue est une **donnée externe non fiable** : le step la rend comme sortie
    /// ordinaire, sa validation de forme est celle du kind.
    ///
    /// `T` porte la valeur JSON résolue par l'appelant externe ; l'enveloppe `Json` est posée en
    /// interne (même convention que [`StepContext::run_effect`]), l'appelant manipule `T` nu.
    ///
    /// # Errors
    ///
    /// Le futur rend [`StepError`] `retryable: false` quand l'awakeable est **rejeté** (le message
    /// de rejet de l'appelant externe transite verbatim, comme celui d'un enfant) ou quand
    /// l'invocation est annulée pendant l'attente ; `Ok(v)` sur résolution avec `v`.
    #[must_use = "l'identifiant et le futur d'un awakeable : jeter l'un des deux perd la résolution ou l'attente"]
    pub fn awakeable<T>(&self) -> (String, impl Future<Output = Result<T, StepError>> + Send + 'a)
    where
        T: serde::de::DeserializeOwned + Send + 'static,
    {
        // L'entrée de journal est posée à l'appel (déterminisme du rejeu : même identifiant).
        let (id, attente) = self.ctx.awakeable::<Json<T>>();
        (id, async move { valeur_ou_rejet::<T>(attente.await) })
    }

    /// Awakeable **à délai durable** (issue #26) : même contrat que [`StepContext::awakeable`],
    /// avec un minuteur (`ctx.sleep(timeout)`) créé **à l'appel** — l'ordre des entrées du journal
    /// reste celui du code, même après reprise — et mis en concurrence avec l'attente par
    /// `restate_sdk::select!`. Le minuteur est déplacé dans le futur retourné.
    ///
    /// `timeout` nul est accepté (le minuteur échoit aussitôt). Une résolution arrivée **après
    /// l'échéance** n'est observée par personne : le SDK `0.12.1` n'a pas d'annulation d'awakeable
    /// (le comportement de l'ingress face à cette résolution tardive est mesuré par
    /// ./client.sdd, hors contrat de ce fichier). Un kind qui doit distinguer délai et rejet
    /// compare le message de l'erreur à `AWAKEABLE_TIMEOUT_MESSAGE`.
    ///
    /// # Errors
    ///
    /// Le futur rend [`StepError`] `retryable: false` dont le message est exactement
    /// `AWAKEABLE_TIMEOUT_MESSAGE` quand le minuteur gagne ; sur rejet de l'awakeable (message
    /// verbatim) ou annulation de l'invocation, comme [`StepContext::awakeable`] ; `Ok(v)` si
    /// l'awakeable gagne avec `v`.
    #[must_use = "l'identifiant et le futur d'un awakeable : jeter l'un des deux perd la résolution ou l'attente"]
    pub fn awakeable_timeout<T>(
        &self,
        timeout: std::time::Duration,
    ) -> (String, impl Future<Output = Result<T, StepError>> + Send + 'a)
    where
        T: serde::de::DeserializeOwned + Send + 'static,
    {
        // awakeable puis sleep : les deux entrées de journal sont posées à l'appel, dans cet ordre,
        // y compris après reprise (déterminisme du rejeu).
        let (id, attente) = self.ctx.awakeable::<Json<T>>();
        let minuteur = self.ctx.sleep(timeout);
        (id, async move {
            restate_sdk::select! {
                réveil = attente => valeur_ou_rejet::<T>(réveil),
                réveil = minuteur => match réveil {
                    Ok(()) => Err(StepError {
                        message: AWAKEABLE_TIMEOUT_MESSAGE.to_string(),
                        retryable: false,
                    }),
                    Err(erreur) => Err(StepError {
                        message: erreur.to_string(),
                        retryable: false,
                    }),
                },
                // Le repli implicite du macro (`Err(TerminalError::new_with_code(409,
                // "cancelled"))?`) ne compile pas sous une sortie `Result<_, StepError>` :
                // le reproduire explicitement garde le même message observable (Handles de
                // ./durable.sdd : annulation pendant l'attente → StepError non retryable).
                on_cancel => Err(StepError {
                    message: TerminalError::new_with_code(409, "cancelled").to_string(),
                    retryable: false,
                }),
            }
        })
    }
}

/// Traduction du réveil d'un awakeable (valeur JSON ou `TerminalError`) en résultat de step :
/// `Ok(v)` nu, ou [`StepError`] `retryable: false` portant le message du rejet verbatim — un retry
/// rejouerait la même issue (le journal la contient, Handles de ./durable.sdd). Assistant privé
/// commun de [`StepContext::awakeable`] et [`StepContext::awakeable_timeout`].
fn valeur_ou_rejet<T>(réveil: Result<Json<T>, TerminalError>) -> Result<T, StepError>
where
    T: serde::de::DeserializeOwned,
{
    match réveil {
        Ok(Json(valeur)) => Ok(valeur),
        Err(erreur) => Err(StepError {
            message: erreur.to_string(),
            retryable: false,
        }),
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

    /// Scenario « `raw_fields` d'un kind durable vaut vide par défaut » (2026-10-06) : `DurableD`
    /// ne redéfinit pas `raw_fields`, `StepRegistry::raw_fields("d")` rend `Some(&[])`.
    #[test]
    fn raw_fields_d_un_kind_durable_vaut_vide_par_defaut() {
        let mut registry = StepRegistry::new();
        registry
            .register_durable(DurableD)
            .expect("un kind durable neuf doit s'enregistrer");
        let vide: &[&str] = &[];
        assert_eq!(registry.raw_fields("d"), Some(vide));
    }

    /// Scenario « le texte du délai est verrouillé » (issue #26, 2026-11-14) :
    /// `AWAKEABLE_TIMEOUT_MESSAGE` vaut exactement `awakeable timed out` — sans identifiant ni
    /// durée (aucun secret dans un message d'erreur). Un kind qui distingue délai et rejet compare
    /// ce texte (arbitré : `@StepError` inchangé, aucun genre d'erreur nouveau).
    #[test]
    fn le_texte_du_délai_est_verrouillé() {
        assert_eq!(super::AWAKEABLE_TIMEOUT_MESSAGE, "awakeable timed out");
    }
}
