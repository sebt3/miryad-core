//! Crate d'intégration rattachée à `src/workflow/mod.sdd` : le `Scenario` « chemins plats
//! résolvent sans connaître la structure interne du module », depuis l'extérieur de la crate
//! (`miryad_core::workflow::…`, chemin d'un vrai consommateur — le test unitaire de `mod.rs` n'a
//! que `crate::`, `E0433` sur le nom propre de la crate en cible `--test` d'une lib).
//! Compilation seule : aucun des neuf chemins ne nomme `client`, `dispatcher`, `interpreter`,
//! `step`, `rhai_step`, `error` ou `definition`.

#![cfg(feature = "workflow")]

/// Scenario « chemins plats résolvent sans connaître la structure interne du module » : les neuf
/// chemins du `Scenario` référencés en position de type (rien n'est instancié ni appelé), plus
/// `configure_policy` référencée en valeur.
#[test]
fn chemins_plats_resolvent_depuis_l_exterieur() {
    fn witness_execution(
        _: Option<miryad_core::workflow::DagInterpreter>,
        _: Option<miryad_core::workflow::StepDispatcher>,
        _: Option<Box<dyn miryad_core::workflow::MiryadWorkflowStep>>,
        _: Option<miryad_core::workflow::StepRegistry>,
    ) {
    }
    fn witness_definition(
        _: Option<miryad_core::workflow::WorkflowConfig>,
        _: Option<miryad_core::workflow::RhaiStep>,
        _: Option<miryad_core::workflow::WorkflowError>,
        _: Option<miryad_core::workflow::WorkflowDefinition>,
    ) {
    }
    witness_execution(None, None, None, None);
    witness_definition(None, None, None, None);
    std::hint::black_box(miryad_core::workflow::configure_policy);
}
