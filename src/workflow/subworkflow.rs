//! Kind natif `"subworkflow"` (feature `workflow`) : un step dont l'exécution est **un autre
//! DAG**, lancé comme run enfant de [`crate::workflow::DagInterpreter`] (clé dérivée
//! `{run_key}:{step_id}`, en-tête de profondeur incrémenté — [`StepContext::run_child_dag`],
//! ./durable.sdd) et attendu jusqu'à sa fin, de façon durable et idempotente au rejeu. Du point
//! de vue du DAG parent, c'est un step comme un autre (il se place dans une couche, fan-out
//! compris) ; sa sortie est la table des sorties du DAG enfant (clé = id du step enfant, valeur =
//! sa sortie), sans enveloppe.
//!
//! Le DAG enfant est **en ligne dans le `config`** du step (`{"dag": [...]}`) : la crate ne charge
//! jamais de définition en base dans le moteur (règle de déterminisme de ./interpreter.sdd), et le
//! cas du consommateur est justement un DAG fabriqué par run. Une application qui veut « lancer la
//! définition stockée X » écrit son propre kind durable (elle charge X, puis
//! [`StepContext::run_child_dag`]) — hors de ce fichier.
//!
//! Le fichier porte le **garde-fou de récursion** : `validate_dag` (./definition.rs,
//! `pub(crate)`) ne voit que le DAG courant, un cycle A → B → A traverse plusieurs définitions ; le plafond est une profondeur maximale à
//! l'exécution ([`DEFAULT_MAX_DEPTH`], surchargeable par [`SubWorkflowStep::new`]), pas une
//! détection de cycle statique. Un enfant au-delà du plafond est refusé en
//! [`crate::workflow::WorkflowError::MaxDepthExceeded`] (`MRD-WORKFLOW-008`) — la récursion ne se
//! résorbe pas en rejouant.
//!
//! Les trois causes d'échec du step (config invalide, profondeur, DAG enfant invalide) sont
//! déterministes : un [`StepError`] `retryable: false` à chaque fois, l'échec d'un enfant
//! également (ses retries éventuels se jouent *dans* l'enfant). Les `inputs` du parent ne sont
//! **pas** transmis à l'enfant dans cette itération (./subworkflow.sdd `Tasks`).
//!
//! [`StepContext::run_child_dag`]: super::durable::StepContext::run_child_dag

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;

use super::definition::DagSteps;
use super::definition::validate_dag;
use super::durable::MiryadDurableStep;
use super::durable::StepContext;
use super::error::WorkflowError;
use super::step::RunInfo;
use super::step::StepError;

/// Profondeur maximale d'un run enfant par défaut : `0` est le run racine déclenché par
/// [`crate::workflow::trigger_run`], `1` un enfant direct… Un enfant à la profondeur `5` est
/// refusé sous ce défaut. Valeur choisie assez haute pour de l'orchestration réelle, assez basse
/// pour qu'un cycle A → B → A échoue vite — surchargeable par [`SubWorkflowStep::new`].
pub const DEFAULT_MAX_DEPTH: u32 = 4;

/// Kind de step `"subworkflow"` : exécute le DAG porté en ligne par le `config` du step comme un
/// run enfant de [`crate::workflow::DagInterpreter`] et rend ses sorties (module doc, et
/// ./subworkflow.sdd).
///
/// L'application consommatrice enregistre elle-même cette instance dans son
/// [`crate::workflow::StepRegistry`] (`register_durable(SubWorkflowStep::default())`) : aucun
/// auto-enregistrement, comme [`crate::workflow::RhaiStep`].
pub struct SubWorkflowStep {
    /// Plafond de profondeur appliqué par [`prepare`] : un enfant à `max_depth + 1` est refusé.
    max_depth: u32,
}

impl SubWorkflowStep {
    /// Construit un kind `"subworkflow"` plafonnant la profondeur de ses runs enfants à
    /// `max_depth` (`0` : aucun sous-workflow possible — moyen légitime pour une app de désactiver
    /// le kind sans le retirer, ./subworkflow.sdd `Handles`).
    #[must_use]
    pub fn new(max_depth: u32) -> Self {
        Self { max_depth }
    }
}

impl Default for SubWorkflowStep {
    /// Rend [`new`](Self::new) appelé avec [`DEFAULT_MAX_DEPTH`].
    fn default() -> Self {
        Self::new(DEFAULT_MAX_DEPTH)
    }
}

/// Le `config` du kind `"subworkflow"` : le DAG enfant, en ligne. Un seul champ attendu, tout
/// autre contenu est rejeté par [`prepare`] (étape 1).
#[derive(Deserialize)]
struct SubWorkflowConfig {
    /// Le DAG enfant, dans la même forme JSON que la colonne `steps` d'une `WorkflowDefinition`.
    dag: DagSteps,
}

/// Tout ce que [`SubWorkflowStep::run`] décide avant d'appeler le moteur, en fonction pure —
/// sans `StepContext`, donc testable en unitaire. Dans cet ordre, interrompue au premier échec
/// (./subworkflow.sdd `Must`) :
///
/// 1. désérialisation du `config` ([`SubWorkflowConfig`]) ;
/// 2. garde de profondeur : l'enfant hériterait de `run.depth + 1`, refusé au-delà de `max_depth`
///    (`MRD-WORKFLOW-008`) ;
/// 3. `validate_dag` sur le DAG enfant — échec avant tout appel, pas après le démarrage d'un
///    run enfant condamné (`MRD-WORKFLOW-004` verbatim) ;
/// 4. le DAG validé.
///
/// Les trois échecs sont des [`StepError`] `retryable: false` : config, profondeur et DAG invalide
/// sont déterministes, les retries des steps enfants se jouent *dans* l'enfant.
fn prepare(run: &RunInfo, config: Value, max_depth: u32) -> Result<DagSteps, StepError> {
    let etape = serde_json::from_value::<SubWorkflowConfig>(config).map_err(|erreur| StepError {
        message: format!("config subworkflow invalide: {erreur}"),
        retryable: false,
    })?;
    let child_depth = run.depth.saturating_add(1);
    if child_depth > max_depth {
        return Err(StepError {
            message: WorkflowError::MaxDepthExceeded {
                depth: child_depth,
                max: max_depth,
            }
            .to_string(),
            retryable: false,
        });
    }
    validate_dag(&etape.dag.0).map_err(|erreur| StepError {
        message: erreur.to_string(),
        retryable: false,
    })?;
    Ok(etape.dag)
}

#[async_trait::async_trait]
impl MiryadDurableStep for SubWorkflowStep {
    fn kind(&self) -> &'static str {
        "subworkflow"
    }

    /// Prépare (`prepare`) puis lance le DAG enfant via [`StepContext::run_child_dag`] —
    /// idempotent au rejeu (clé enfant `{run_key}:{step_id}`, profondeur incrémentée, portées par
    /// ./durable.rs, jamais calculées ici). Sortie : la table des sorties de l'enfant en objet
    /// JSON sans enveloppe. Les `inputs` du parent ne sont pas transmis (./subworkflow.sdd
    /// `Tasks`). L'échec de l'enfant fait échouer ce step donc le parent, message conservé
    /// verbatim — pas de branche de repli dans cette itération.
    async fn run(
        &self,
        ctx: &StepContext<'_>,
        config: Value,
        _inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        let dag = prepare(ctx.run_info(), config, self.max_depth)?;
        let sorties = ctx.run_child_dag(&dag).await?;
        Ok(Value::Object(sorties.into_iter().collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::DEFAULT_MAX_DEPTH;
    use super::SubWorkflowStep;
    use super::prepare;
    use crate::workflow::MiryadDurableStep as _;
    use crate::workflow::step::RunInfo;
    use serde_json::Value;
    use serde_json::json;

    // Fixtures — `prepare` est une fonction pure (spec ./subworkflow.sdd `Must`) : aucun
    // `StepContext`, aucun serveur Restate. Le `run` de MiryadDurableStep n'est pas testable en
    // unitaire (aucun `Context` virtualisé dans `restate-sdk` 0.12.1, même borne que
    // ./durable.sdd) : il est prouvé par les intégrations `#[ignore]` de tests/workflow_restate.rs.

    /// `RunInfo` à la profondeur donnée — `run_key`/`step_id` neutres, seul `depth` porte le
    /// contrat exercé ici.
    fn run(depth: u32) -> RunInfo {
        RunInfo {
            run_key: "r".to_string(),
            step_id: "s".to_string(),
            depth,
        }
    }

    /// `config` valide minimal : un seul step `"a"`, forme `{"dag": [{id, depends_on, kind,
    /// config}]}` du `Accepts` de ./subworkflow.sdd.
    fn config_valide() -> Value {
        json!({ "dag": [{ "id": "a", "depends_on": [], "kind": "x", "config": {} }] })
    }

    /// Extrait le [`StepError`] d'un `Err`, faute distincte marquée rouge explicitement sinon.
    fn erreur_rendue(
        resultat: Result<super::DagSteps, crate::workflow::StepError>,
    ) -> crate::workflow::StepError {
        match resultat {
            Err(erreur) => erreur,
            Ok(dag) => panic!("attendu un Err, rendu Ok({dag:?})"),
        }
    }

    /// Scenario « prepare rend le DAG d'un config valide » : `run.depth = 0`, `max_depth = 4`,
    /// un `config` portant un seul step `"a"` → `Ok(DagSteps)` d'un seul step d'id `"a"`.
    #[test]
    fn prepare_rend_le_dag_dun_config_valide() {
        let dag = prepare(&run(0), config_valide(), 4).expect("un config valide doit être accepté");
        assert_eq!(dag.0.len(), 1, "un seul step attendu");
        assert_eq!(dag.0[0].id, "a");
    }

    /// Scenario « prepare refuse un config sans champ dag » : `{"autre": 1}` → `StepError`
    /// `retryable: false`, message commençant par `config subworkflow invalide: `.
    #[test]
    fn prepare_refuse_un_config_sans_champ_dag() {
        let erreur = erreur_rendue(prepare(&run(0), json!({ "autre": 1 }), 4));
        assert!(
            erreur.message.starts_with("config subworkflow invalide: "),
            "préfixe contractuel attendu : {erreur}"
        );
        assert!(!erreur.retryable, "un config invalide n'est jamais retryable");
    }

    /// Scenario « prepare accepte la profondeur exactement au plafond » : `run.depth = 3`,
    /// `max_depth = 4` → `Ok` — l'enfant serait à la profondeur 4, pas au-delà.
    #[test]
    fn prepare_accepte_la_profondeur_exactement_au_plafond() {
        prepare(&run(3), config_valide(), 4)
            .expect("l'enfant à la profondeur 4 (= plafond) doit être accepté");
    }

    /// Scenario « prepare refuse un enfant au-delà du plafond » : `run.depth = 4`, `max_depth =
    /// 4` → message exactement `MRD-WORKFLOW-008: sub-workflow depth 5 exceeds the maximum of 4`,
    /// `retryable: false`.
    #[test]
    fn prepare_refuse_un_enfant_au_delà_du_plafond() {
        let erreur = erreur_rendue(prepare(&run(4), config_valide(), 4));
        assert_eq!(
            erreur.message,
            "MRD-WORKFLOW-008: sub-workflow depth 5 exceeds the maximum of 4"
        );
        assert!(!erreur.retryable, "la récursion ne se résorbe pas en rejouant");
    }

    /// Scenario « `max_depth` zéro désactive le sous-workflow » : `run.depth = 0`, `max_depth = 0`
    /// → message `MRD-WORKFLOW-008: sub-workflow depth 1 exceeds the maximum of 0` — le premier
    /// enfant serait à la profondeur 1, aucun sous-workflow n'est possible.
    #[test]
    fn max_depth_zéro_désactive_le_sous_workflow() {
        let erreur = erreur_rendue(prepare(&run(0), config_valide(), 0));
        assert_eq!(
            erreur.message,
            "MRD-WORKFLOW-008: sub-workflow depth 1 exceeds the maximum of 0"
        );
        assert!(!erreur.retryable);
    }

    /// Scenario « prepare refuse un DAG enfant invalide avant tout appel » : cycle `a` → `b` →
    /// `a` → message commençant par `MRD-WORKFLOW-004:` (le `to_string` de `InvalidDag`, verbatim).
    #[test]
    fn prepare_refuse_un_dag_enfant_invalide_avant_tout_appel() {
        let cyclique = json!({
            "dag": [
                { "id": "a", "depends_on": ["b"], "kind": "x", "config": {} },
                { "id": "b", "depends_on": ["a"], "kind": "x", "config": {} }
            ]
        });
        let erreur = erreur_rendue(prepare(&run(0), cyclique, 4));
        assert!(
            erreur.message.starts_with("MRD-WORKFLOW-004:"),
            "le code MRD-WORKFLOW-004 doit porter le message : {erreur}"
        );
        assert!(!erreur.retryable, "un DAG invalide n'est jamais retryable");
    }

    /// Scenario « prepare refuse un DAG enfant vide » : `{"dag": []}` est un `config` bien formé
    /// (étape 1 passée), refusé à l'étape 3 par `validate_dag`, message verbatim commençant par
    /// `MRD-WORKFLOW-004:`.
    #[test]
    fn prepare_refuse_un_dag_enfant_vide() {
        let erreur = erreur_rendue(prepare(&run(0), json!({ "dag": [] }), 4));
        assert!(
            erreur.message.starts_with("MRD-WORKFLOW-004:"),
            "message verbatim de validate_dag attendu : {erreur}"
        );
        assert!(!erreur.retryable);
    }

    /// Scenario « le kind et le défaut sont ceux du contrat » : `SubWorkflowStep::default()` a
    /// `kind() == "subworkflow"` ; `DEFAULT_MAX_DEPTH` vaut `4` ; la `max_depth` portée par le
    /// défaut laisse passer un enfant depuis `run.depth = 3` et refuse celui depuis `4` — le
    /// défaut applique bien `DEFAULT_MAX_DEPTH`.
    #[test]
    fn le_kind_et_le_défaut_sont_ceux_du_contrat() {
        let step = SubWorkflowStep::default();
        assert_eq!(step.kind(), "subworkflow");
        assert_eq!(DEFAULT_MAX_DEPTH, 4);
        assert_eq!(
            step.max_depth, DEFAULT_MAX_DEPTH,
            "le défaut applique bien DEFAULT_MAX_DEPTH"
        );
        prepare(&run(3), config_valide(), step.max_depth)
            .expect("la profondeur 3 (enfant à 4) doit passer sous le défaut");
        let erreur = erreur_rendue(prepare(&run(4), config_valide(), step.max_depth));
        assert!(
            erreur.message.starts_with("MRD-WORKFLOW-008:"),
            "la profondeur 4 (enfant à 5) doit être refusée sous le défaut : {erreur}"
        );
    }
}
