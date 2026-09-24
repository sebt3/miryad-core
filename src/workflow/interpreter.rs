//! Service Restate `DagInterpreter` (feature `workflow`) : marche générique d'un DAG de workflow,
//! de bout en bout (contrat porté par `./interpreter.sdd`).
//!
//! Unique `#[workflow]` de la crate : reçoit un [`DagSteps`] (./definition.rs), le revalide
//! structurellement (`validate_dag` — `pub(crate)`, ./definition.rs — aucune hypothèse sur sa
//! provenance), puis marche le graphe
//! par couches topologiques. Le fan-out d'une couche passe uniquement par
//! `ctx.request(...).call()` poussé dans un `DurableFuturesUnordered` **construit pour cette
//! couche** — jamais `ctx.run()` (la durabilité d'un step appartient à ./dispatcher.rs), jamais
//! `futures::future::join_all` (blocage d'une soixantaine de secondes par couche, spike
//! Restate 2026-09-22).

use std::collections::HashMap;
use std::collections::HashSet;

use restate_sdk::context::RequestTarget;
use restate_sdk::prelude::ContextClient;
use restate_sdk::prelude::DurableFuturesUnordered;
use restate_sdk::prelude::HandlerError;
use restate_sdk::prelude::Json;
use restate_sdk::prelude::TerminalError;
use restate_sdk::prelude::WorkflowContext;
use restate_sdk::prelude::workflow;

use super::definition::DagSteps;
use super::definition::StepDefinition;
use super::definition::validate_dag;
use super::dispatcher::StepInvocation;

/// Marque du workflow Restate — struct unitaire sans état : tout l'état d'une marche (`completed`,
/// `results`) vit local au handler `run`, rejoué depuis le journal Restate à la reprise.
pub struct DagInterpreter;

/// L'unique workflow Restate du module : exécute un DAG de steps de sa réception à sa complétion.
/// Nom d'enregistrement `"DagInterpreter"` (défaut du nom du struct, aucun attribut `name`) —
/// c'est celui que ./client.rs code en dur dans `trigger_run`. Ce fichier ne construit jamais
/// l'instance ni n'appelle `run` lui-même : seul `restate-sdk` pilote le handler (reprise sur
/// crash incluse).
// `client_visibility = "pub(crate)"` : les clients générés par la macro pour invoquer ce workflow
// (DagInterpreterClient / DagInterpreterIngressClient) ne sortent jamais de la crate — ./client.rs
// passe par l'ingress HTTP nu, Restate pilote `run` lui-même (./interpreter.sdd `Exposes` : le
// service est « consommé uniquement via le protocole restate-sdk »). Ratifié sur le @service de
// ./dispatcher.rs, même fuite `doc_markdown`/`missing_docs` évitée ; le nom du workflow et `run`
// sur le fil restent inchangés.
#[workflow(client_visibility = "pub(crate)")]
impl DagInterpreter {
    /// Marche le DAG reçu, couche après couche, et rend la table `{ id du step -> sortie }`.
    ///
    /// Le `WorkflowContext` de `restate-sdk` `0.12.1` n'est virtualisé par aucun harnais : la
    /// boucle de marche n'est pas prouvable en `mod tests` (borne des `Tasks` de
    /// ./interpreter.sdd — les deux fonctions pures qu'elle appelle, [`ready_steps`] et
    /// [`build_inputs`], le sont).
    #[handler]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        dag: Json<DagSteps>,
    ) -> Result<Json<HashMap<String, serde_json::Value>>, HandlerError> {
        // Déstructuration du wrapper `Json` : sous le wrapper `restate-sdk`, `dag.0` est bien la
        // liste des steps, telle que le `Must` de ./interpreter.sdd nomme les choses.
        let Json(dag) = dag;
        // Étape 1 — revalidation structurelle systématique, avant tout step : ./client.rs a beau
        // être le déclencheur usuel (DAG passé par `before_create`), le corps reçu est traité
        // sans aucune hypothèse (`Accepts`). Le `to_string()` de `WorkflowError::InvalidDag`
        // porte déjà le préfixe `MRD-WORKFLOW-004:` — réutilisé verbatim, jamais reconstruit.
        if let Err(erreur) = validate_dag(&dag.0) {
            return Err(HandlerError::from(TerminalError::new_with_code(
                400,
                erreur.to_string(),
            )));
        }

        // Étape 2 — marche topologique. `validate_dag` ayant proscrit cycle et dépendance
        // pendante, chaque itération produit au moins un step prêt tant que `completed` n'a pas
        // atteint la taille du DAG : pas de garde de couche vide ici (`Must not` revérifier ce
        // que `validate_dag` prouve).
        let mut completed: HashSet<String> = HashSet::new();
        let mut results: HashMap<String, serde_json::Value> = HashMap::new();
        while completed.len() != dag.0.len() {
            let prepared = ready_steps(&dag.0, &completed);
            // Un `DurableFuturesUnordered` par couche, construit ici — jamais partagé entre deux
            // couches, jamais `join_all` (spike 2026-09-22 : ~60 s de blocage par couche).
            let mut futures = DurableFuturesUnordered::new();
            let mut ready_ids: Vec<String> = Vec::with_capacity(prepared.len());
            for step in &prepared {
                ready_ids.push(step.id.clone());
                let invocation = StepInvocation {
                    kind: step.kind.clone(),
                    config: step.config.clone(),
                    inputs: build_inputs(step, &results),
                };
                futures.push(
                    ctx.request::<Json<StepInvocation>, Json<serde_json::Value>>(
                        RequestTarget::Service {
                            name: "StepDispatcher".into(),
                            handler: "execute".into(),
                        },
                        Json(invocation),
                    )
                    .call(),
                );
            }
            // `idx` est l'index de push dans CETTE couche, jamais un id ; `ready_ids` est poussé
            // dans le même ordre que les futures. Un `None` ici est un bug interne, rendu en
            // TerminalError (jamais un panic — harnais).
            while let Some((idx, result)) = futures.next().await? {
                let Some(id) = ready_ids.get(idx) else {
                    return Err(HandlerError::from(TerminalError::new(
                        "internal index out of bounds",
                    )));
                };
                // Premier `Err` de step observé : propagé immédiatement (`?` → `HandlerError`
                // terminal), sans attendre les frères encore en vol ni les annuler — un step
                // frère dispatché poursuit ses effets de bord côté ./dispatcher.rs, son résultat
                // n'est simplement plus attendu (`Tasks` actée 2026-09-23).
                let Json(output) = result?;
                results.insert(id.clone(), output);
                completed.insert(id.clone());
            }
        }

        // Étape 3 — DAG intégralement marché.
        Ok(Json(results))
    }
}

/// Steps dont toutes les dépendances sont dans `completed` et qui n'y sont pas déjà, dans
/// l'ordre de la liste d'origine du DAG — un `ready_ids` déterministe entre deux exécutions du
/// même DAG (propriété exploitée par les tests, pas garantie vis-à-vis de Restate).
/// Fonction pure, sans `ctx` ni effet de bord : la seule partie decidable sans harnais Restate
/// de la préparation de couche (`Tasks` de ./interpreter.sdd).
pub(crate) fn ready_steps<'a>(
    dag: &'a [StepDefinition],
    completed: &HashSet<String>,
) -> Vec<&'a StepDefinition> {
    dag.iter()
        .filter(|step| !completed.contains(step.id.as_str()))
        .filter(|step| step.depends_on.iter().all(|dep| completed.contains(dep.as_str())))
        .collect()
}

/// Une entrée par id de `step.depends_on`, valeur = la sortie collectée du step amont dans
/// `results`. Jamais une entrée de plus (un résultat disponible mais absent de `depends_on` ne
/// traverse pas), jamais une de moins. `build_inputs` n'est appelé que depuis l'étape 2.b de
/// [`DagInterpreter::run`] : chaque dépendance est alors dans `completed`, donc déjà dans
/// `results` — `results.get(id)` y est un `Some` à ce point de la marche. Un `None` (violation
/// de cette invariante, chemin hors contrat) ne fabrique jamais une valeur inexistante et ne
/// panique jamais : l'entrée est omise, et c'est le step aval qui la réclame qui la paie, côté
/// ./dispatcher.rs, en `StepError` terminal du kind — jamais en panic dans ce fichier.
pub(crate) fn build_inputs(
    step: &StepDefinition,
    results: &HashMap<String, serde_json::Value>,
) -> HashMap<String, serde_json::Value> {
    step.depends_on
        .iter()
        .filter_map(|id| results.get(id).map(|sortie| (id.clone(), sortie.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::build_inputs;
    use super::ready_steps;
    use crate::workflow::definition::StepDefinition;
    use serde_json::json;
    use std::collections::HashMap;
    use std::collections::HashSet;

    // ── Fixtures — purement mémoire : `ready_steps` et `build_inputs` sont des fonctions pures,
    // sans `ctx` ni base (séparaison tranchée par ./interpreter.sdd `Must`, même rationale que
    // @run_invocation de ./dispatcher.sdd).

    /// Step de fixture : `kind`/`config` opaques, jamais interprétés par ./interpreter.rs (`Must
    /// not`).
    fn etape(id: &str, depends_on: &[&str]) -> StepDefinition {
        StepDefinition {
            id: id.to_string(),
            depends_on: depends_on.iter().copied().map(String::from).collect(),
            kind: "noop".to_string(),
            config: serde_json::Value::Null,
        }
    }

    /// Losange du Scenario 1 : `"A"` sans dépendance, `"B"` et `"C"` dépendant de `"A"`, `"D"`
    /// dépendant de `"B"` et `"C"` — `completed` des scenarios 2 à 4 est un sous-ensemble strict.
    fn dag_losange() -> Vec<StepDefinition> {
        vec![
            etape("A", &[]),
            etape("B", &["A"]),
            etape("C", &["A"]),
            etape("D", &["B", "C"]),
        ]
    }

    fn completed(ids: &[&str]) -> HashSet<String> {
        ids.iter().copied().map(String::from).collect()
    }

    fn ids<'a>(prepares: &[&'a StepDefinition]) -> Vec<&'a str> {
        prepares.iter().map(|etape| etape.id.as_str()).collect()
    }

    /// Scenario « `ready_steps` rend les steps sans dépendance en premier » : losange, `completed`
    /// vide → exactement `["A"]`.
    #[test]
    fn ready_steps_rend_les_steps_sans_dependance_en_premier() {
        let dag = dag_losange();
        let rendu = ready_steps(&dag, &completed(&[]));
        assert_eq!(ids(&rendu), vec!["A"]);
    }

    /// Scenario « `ready_steps` rend plusieurs steps d'une même couche » : losange,
    /// `completed = {"A"}` → exactement `["B", "C"]`, dans l'ordre du DAG d'origine (pas
    /// l'ordre d'insertion d'un `HashSet` de passage).
    #[test]
    fn ready_steps_rend_plusieurs_steps_d_une_meme_couche() {
        let dag = dag_losange();
        let rendu = ready_steps(&dag, &completed(&["A"]));
        assert_eq!(ids(&rendu), vec!["B", "C"]);
    }

    /// Scenario « `ready_steps` ne rend pas un step dont une dépendance manque » : losange,
    /// `completed = {"A", "B"}` (`"C"` pas encore terminé) → la couche est exactement `["C"]` —
    /// `"D"`, dont la dépendance `"C"` manque, n'y paraît jamais, et `"B"` déjà terminé n'y
    /// paraît plus. Le `Then` verbatim d'origine (« le résultat est vide ») était une erreur
    /// d'arithmétique du losange : `"C"`, dont la seule dépendance `"A"` est complétée, est
    /// prêt, et une couche vide contredirait l'étape 2.a du `Must`. ./interpreter.sdd porte la
    /// version corrigée et verrouille l'observable ci-dessus ; la relecture `[?]` de cette
    /// correction reste ouverte dans ses `Tasks`, pas dans ce fichier.
    #[test]
    fn ready_steps_ne_rend_pas_un_step_dont_une_dependance_manque() {
        let dag = dag_losange();
        let rendu = ids(&ready_steps(&dag, &completed(&["A", "B"])));
        assert!(
            !rendu.contains(&"D"),
            "\"D\" dépend de \"B\" et \"C\" ; \"C\" manque encore, \"D\" ne doit jamais être \
             rendu : {rendu:?}"
        );
        assert_eq!(
            rendu,
            vec!["C"],
            "la couche en attente n'est vide que pour \"D\" — \"C\" est prêt (seule dépendance \
             \"A\", complétée)"
        );
    }

    /// Scenario « `ready_steps` ignore un step déjà complété » : losange, les quatre steps
    /// complétés → vide — un step prêt mais déjà dans `completed` n'est jamais relancé.
    #[test]
    fn ready_steps_ignore_un_step_deja_complete() {
        let dag = dag_losange();
        let rendu = ready_steps(&dag, &completed(&["A", "B", "C", "D"]));
        assert!(rendu.is_empty(), "attendu vide, rendu : {:?}", ids(&rendu));
    }

    /// Scenario « `inputs` assemblé depuis les résultats des dépendances, jamais au-delà » : `"B"`
    /// dépend de `"A"` seul ; le résultat de `"C"`, disponible dans `results` mais absent de
    /// `depends_on`, ne traverse jamais.
    #[test]
    fn inputs_assemble_depuis_les_resultats_des_dependances_jamais_au_dela() {
        let b = etape("B", &["A"]);
        let mut results: HashMap<String, serde_json::Value> = HashMap::new();
        results.insert("A".to_string(), json!(1));
        results.insert("C".to_string(), json!(2));

        let inputs = build_inputs(&b, &results);

        assert_eq!(
            inputs,
            HashMap::from([("A".to_string(), json!(1))]),
            "exactement une entrée — `\"A\"` → `json!(1)`, jamais `\"C\"` bien que présent dans \
             `results`"
        );
    }
}
