//! Moteur de workflow à DAG (feature `workflow`) : des workflows persistés en base, édités par
//! l'admin via le CRUD générique de `WorkflowDefinition` comme toute autre entité, et exécutés
//! par un cluster Restate self-hosté — un par cluster Kubernetes, jamais géré par cette crate.
//!
//! `workflow` ne monte **aucune** route sur le `axum::Router` de l'app : ses services parlent le
//! protocole `restate-sdk`, servis par un `HttpServer`/`Endpoint` que l'app construit et lie
//! elle-même dans son propre `main()`. Schéma de déploiement (StatefulSet Restate, vynil box)
//! dans `docs/architecture.md`.

pub mod client;
pub mod definition;
pub mod dispatcher;
pub mod error;
pub mod interpreter;
pub mod rhai_step;
pub mod step;

// Ré-exports à plat, par sous-module (ordre alphabétique des sous-modules, comme les déclarations
// ci-dessus) ; à l'intérieur des accolades, rustfmt (style edition 2024) impose son tri des noms —
// pure cosmetique sur la liste du `Must` de ./mod.sdd. `validate_dag` (et tout autre item
// `pub(crate)` d'un enfant) reste volontairement hors de cette liste (`Must not` de ./mod.sdd) :
// jamais atteint depuis l'extérieur de la crate.
pub use client::{RunHandle, WorkflowConfig, register_deployment, trigger_run};
pub use definition::{
    ActiveModel as WorkflowDefinitionActiveModel, Column as WorkflowDefinitionColumn, DagSteps,
    Entity as WorkflowDefinition, Model as WorkflowDefinitionModel, StepDefinition, WorkflowPolicy,
    configure_policy,
};
pub use dispatcher::{StepDispatcher, recommended_options};
pub use error::WorkflowError;
pub use interpreter::DagInterpreter;
pub use rhai_step::RhaiStep;
pub use step::{MiryadWorkflowStep, StepError, StepRegistry};

#[cfg(test)]
mod tests {
    /// Scénario verrouillé : « chemins plats résolvent sans connaître la structure interne du
    /// module » (`./mod.sdd`). Compilation seule — aucun comportement au-delà de la résolution.
    ///
    /// Forme tranchée à l'implémentation : le nom propre de la crate (`miryad_core::workflow::…`,
    /// celui du scénario) ne résout pas dans la cible unitaire d'une lib — `E0433`, cargo ne passe
    /// pas le self-`--extern` à la cible `--test`. La convention des tests inline de la crate est
    /// `crate::`/`super::` ; elle vérifie la résolution des neuf mêmes chemins plats. La résolution
    /// depuis l'extérieur par un consommateur est exercée par les crates de `tests/`
    /// (`use miryad_core::workflow::…`). Aucun des neuf chemins ci-dessous ne nomme `client`,
    /// `dispatcher`, `interpreter`, `step`, `rhai_step`, `error` ou `definition`.
    #[test]
    fn chemins_plats_resolvent_sans_structure_interne() {
        // Deux témoins locaux de quatre paramètres : chaque type est référencé en position de
        // type, rien n'est instancié, appelé ni lié (les liaisons `_…` sous harnais pedantic sont
        // proscrites — `no_effect_underscore_binding`, `type_complexity`).
        fn witness_execution(
            _: Option<crate::workflow::DagInterpreter>,
            _: Option<crate::workflow::StepDispatcher>,
            _: Option<Box<dyn crate::workflow::MiryadWorkflowStep>>,
            _: Option<crate::workflow::StepRegistry>,
        ) {
        }
        fn witness_definition(
            _: Option<crate::workflow::WorkflowConfig>,
            _: Option<crate::workflow::RhaiStep>,
            _: Option<crate::workflow::WorkflowError>,
            _: Option<crate::workflow::WorkflowDefinition>,
        ) {
        }
        witness_execution(None, None, None, None);
        witness_definition(None, None, None, None);
        // Neuvième chemin : `configure_policy`, référencé en valeur sans être appelé — sa sémantique
        // `OnceLock` est prouvée par `tests/workflow_policy.rs`, hors de ce scénario.
        std::hint::black_box(crate::workflow::configure_policy);
    }
}
