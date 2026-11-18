//! Service Restate `DagInterpreter` (feature `workflow`) : marche générique d'un DAG de workflow,
//! de bout en bout (contrat porté par `./interpreter.sdd`).
//!
//! Unique `#[workflow]` de la crate : reçoit un `RunInput` (`{"dag", "vars", "trigger"}` —
//! `trigger` facultatif, omis à la sérialisation quand absent ; amendements 2026-10-30 lot B et
//! 2026-11-02 lot 0 lymaille) dont le [`DagSteps`] (./definition.rs) est revalidé structurellement
//! (`validate_dag` — `pub(crate)`, ./definition.rs — aucune hypothèse sur sa
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
use serde::Deserialize;
use serde::Serialize;

use super::definition::DagSteps;
use super::definition::StepDefinition;
use super::definition::validate_dag;
use super::dispatcher::StepInvocation;
use super::durable::DEPTH_HEADER;

/// Marque du workflow Restate — struct unitaire sans état : tout l'état d'une marche (`completed`,
/// `results`) vit local au handler `run`, rejoué depuis le journal Restate à la reprise.
pub struct DagInterpreter;

/// Corps JSON de l'entrée du run (amendement 2026-10-30, lot B, `FR-12` ; troisième champ
/// `trigger` : amendement 2026-11-02, lot 0 lymaille, `FR-10`/`DEC-12`) :
/// `{"dag": [..steps..], "vars": {..}}`, plus `"trigger": "<subject>"` quand le run a un
/// déclencheur — changement cassant de la forme d'entrée (jusqu'ici le
/// tableau de steps nu), assumé : API instable avant `1.0`, aucune production. ./client.rs et
/// ./durable.rs (`run_child_dag`) construisent ce corps ; ./interpreter.rs le lit
/// (`./interpreter.sdd` `Must`, « Entrée du run »).
#[derive(Serialize, Deserialize)]
pub(crate) struct RunInput {
    /// Le DAG à marcher — revalidé par `validate_dag` en tête de `run`, sans hypothèse sur sa
    /// provenance (`Accepts`).
    pub(crate) dag: DagSteps,
    /// Variables constantes du run, journalisées par Restate avec l'entrée ; `#[serde(default)]` :
    /// un corps sans `vars` donne `{}`.
    #[serde(default)]
    pub(crate) vars: serde_json::Map<String, serde_json::Value>,
    /// `subject` de la personne qui a déclenché le run (amendement 2026-11-02, lot 0 lymaille,
    /// `FR-10`/`DEC-12`) : conservé intact pour tout le run et recopié dans chaque
    /// `StepInvocation`. `#[serde(default, skip_serializing_if = "Option::is_none")]` : un corps
    /// sans `trigger` donne `None`, un `None` n'est pas écrit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) trigger: Option<String>,
}

/// Profondeur du run lue depuis la valeur brute de l'en-tête [`DEPTH_HEADER`] : un `u32` valide
/// est rendu tel quel ; en-tête absent, non numérique ou hors plage `u32` rend `0` — un run
/// déclenché par `client::trigger_run` n'en porte pas. Ne lève jamais d'erreur, ne panique
/// jamais (`./interpreter.sdd` `Must` étape 1b, #26).
pub(crate) fn parse_depth(raw: Option<&str>) -> u32 {
    raw.and_then(|raw| raw.parse::<u32>().ok()).unwrap_or(0)
}

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
    /// ./interpreter.sdd — les fonctions pures qu'elle appelle, [`ready_steps`], [`build_inputs`]
    /// et [`build_invocation`], le sont).
    #[handler]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        input: Json<RunInput>,
    ) -> Result<Json<HashMap<String, serde_json::Value>>, HandlerError> {
        // Étape 0 (amendement 2026-10-30, lot B, `FR-12` ; `trigger` : amendement 2026-11-02, lot 0
        // lymaille, `FR-10`/`DEC-12`) — déstructuration du wrapper `Json` : `vars` et `trigger` sont
        // conservés intacts pour tout le run et alimentent chaque `StepInvocation` via
        // [`build_invocation`] (les unes recopiées en `Value::Object`, l'autre en `Option<String>`) ;
        // `dag` est la liste des steps, comme avant. Le `to_string()` de `WorkflowError::InvalidDag`
        // porte déjà le préfixe `MRD-WORKFLOW-004:` — réutilisé verbatim à l'étape 1, jamais
        // reconstruit.
        let Json(RunInput { dag, vars, trigger }) = input;
        // Étape 1 — revalidation structurelle systématique, avant tout step : ./client.rs a beau
        // être le déclencheur usuel (DAG passé par `before_create`), le corps reçu est traité
        // sans aucune hypothèse (`Accepts`).
        if let Err(erreur) = validate_dag(&dag.0) {
            return Err(HandlerError::from(TerminalError::new_with_code(
                400,
                erreur.to_string(),
            )));
        }

        // Étape 1b (#26, 2026-10-04) — lecture de la profondeur, une seule fois pour tout le run :
        // en-tête absent, non numérique ou hors `u32` → `0` (`parse_depth`, aucune erreur levée
        // ici). Le garde-fou de récursion vit dans ./subworkflow.rs, pas dans cette lecture.
        let depth = parse_depth(ctx.headers().get(DEPTH_HEADER).map(String::as_str));

        // Étape 2 — marche topologique. `validate_dag` ayant proscrit cycle et dépendance
        // pendante, chaque itération produit au moins un step prêt tant que `completed` n'a pas
        // atteint la taille du DAG : pas de garde de couche vide ici (`Must not` revérifier ce
        // que `validate_dag` prouve).
        let mut completed: HashSet<String> = HashSet::new();
        // Étape a' (amendement 2026-11-18, issue #84, DSD `workflow` `FR-20`/`DEC-14`) —
        // `skipped` : les steps ignorés, comptés comme terminés pour l'ordonnancement mais sans
        // clé dans `results` (les décisions sont des fonctions pures de `vars` et `results`,
        // rejouées à l'identique après crash — `NFR-1`).
        let mut skipped: HashSet<String> = HashSet::new();
        let mut results: HashMap<String, serde_json::Value> = HashMap::new();
        // Exports nommés résolus (amendement 2026-11-18, issue #85, « Exports nommés ») —
        // alimentée à l'étape 2.e pour chaque step `Run` terminé en `Ok` ; un step ignoré n'y a
        // aucune entrée. Jamais dans la table de sortie du run ni dans `StepInvocation` ;
        // reconstituée par rejeu des sorties journalisées (`NFR-1`).
        let mut exports: HashMap<String, serde_json::Value> = HashMap::new();
        while completed.len() != dag.0.len() {
            let prepared = ready_steps(&dag.0, &completed);
            // Étape a' — décision d'exécution de chaque step de la couche par le code partagé
            // [`apply_decisions`] (`decide`, fonction pure) : un `Skip` est ajouté à `completed`
            // et `skipped`, n'est pas dispatché, n'entre pas dans `results`. Seuls les `Run`
            // passent à l'étape b ; les ignorés ne doivent qu'une trace d'un `step_id` et de la
            // raison — jamais une valeur lue.
            let (a_lancer, a_tracer) =
                apply_decisions(prepared, &mut completed, &mut skipped, &vars, &results, &exports);
            for (step_id, raison) in &a_tracer {
                match raison {
                    SkipReason::Dependency => tracing::info!(
                        step_id = %step_id,
                        raison = "dependency",
                        "step ignoré : une dépendance directe a été ignorée"
                    ),
                    SkipReason::When => tracing::info!(
                        step_id = %step_id,
                        raison = "when",
                        "step ignoré : sa condition `when` est fausse"
                    ),
                }
            }
            if a_lancer.is_empty() {
                // Couche entièrement ignorée : aucun `DurableFuturesUnordered` construit, la
                // boucle reprend à l'étape a — `completed` a grandi, la boucle avance.
                continue;
            }
            // Un `DurableFuturesUnordered` par couche, construit ici — jamais partagé entre deux
            // couches, jamais `join_all` (spike 2026-09-22 : ~60 s de blocage par couche).
            let mut futures = DurableFuturesUnordered::new();
            let mut ready_ids: Vec<String> = Vec::with_capacity(a_lancer.len());
            // Les steps poussés, dans le même ordre que `ready_ids` et que les `push` de
            // `futures` : l'étape 2.e retrouve l'exportation déclarée du step terminé par ce
            // même index (amendement 2026-11-18, issue #85).
            let mut poussees: Vec<&StepDefinition> = Vec::with_capacity(a_lancer.len());
            for step in &a_lancer {
                ready_ids.push(step.id.clone());
                poussees.push(step);
                // #26 : identité du run — clé de workflow Restate de cette invocation, `id`
                // du step, profondeur lue une fois à l'étape 1b. B4 (lot B, `FR-12`) : les
                // `vars` du run, lues une fois à l'étape 0, sont recopiées dans chaque
                // invocation par `build_invocation` (seul endroit de la crate où une
                // `StepInvocation` est assemblée). T4 (amendement 2026-11-02, `FR-10`) : le
                // `trigger` du run, lu à l'étape 0, y est recopié de même.
                let invocation = build_invocation(
                    step,
                    build_inputs(step, &results, &skipped),
                    ctx.key(),
                    depth,
                    &vars,
                    trigger.as_deref(),
                );
                futures.push(
                    ctx.request::<Json<StepInvocation>, Json<serde_json::Value>>(
                        RequestTarget::Service {
                            name: "StepDispatcher".into(),
                            // DEC-29 (amendement 2026-11-03, ./dispatcher.sdd) : routage par liste
                            // fixe — un kind de LONG_RUNNING_KINDS cible `execute_long`, sinon `execute`.
                            handler: super::dispatcher::handler_name(&step.kind).into(),
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
                // Étape 2.e (amendement 2026-11-18, issue #85) — collecte des exports nommés du
                // step terminé : un step `Run` couronné en `Ok` alimente la table `exports` ;
                // un step ignoré n'y parvient jamais (aucun résultat à collecter). Le repli sur
                // un `None` de `poussees.get` est défensif, hors contrat : `poussees` et les
                // poussées de `futures` sont construits dans la même boucle, même longueur.
                if let Some(etape) = poussees.get(idx).copied() {
                    for (nom, valeur) in collect_exports(etape, &output) {
                        exports.insert(nom, valeur);
                    }
                }
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

/// Décision d'exécution d'un step prêt (amendement 2026-11-18, issue #84, ./interpreter.sdd) :
/// exécuter, ou ignorer pour l'une des deux raisons de [`SkipReason`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StepDecision {
    /// Le step est dispatché.
    Run,
    /// Le step est ignoré — compte comme terminé pour l'ordonnancement, jamais dispatché, aucune
    /// clé dans la table de sortie du run.
    Skip(SkipReason),
}

/// Raison d'une ignorance (amendement 2026-11-18, issue #84).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SkipReason {
    /// Une dépendance directe a été ignorée et `run_if_skipped` est faux : propagation.
    Dependency,
    /// Le `when` du step rend `false`.
    When,
}

/// Décision d'exécution d'un step prêt : fonction pure, sans `ctx` — fonctions pures de `vars`
/// (journalisées avec l'entrée du run), de `results` (journalisés par Restate) et de `exports`
/// (la table des valeurs d'export déjà résolues, amendement 2026-11-18 issue #85 — extraites des
/// mêmes sorties, donc rejeu identique), donc rejeu identique après crash (`NFR-1`). Propagation
/// d'abord : une dépendance ignorée et `run_if_skipped` faux → `Skip(Dependency)`, sans même
/// évaluer le `when`. Un step qui s'exécute malgré une dépendance ignorée (`run_if_skipped`)
/// évalue quand même son `when`, lequel lit une valeur absente (donc `false`) s'il vise le step
/// ignoré ou un export jamais résolu — `evaluate` est totale (./predicate.rs).
pub(crate) fn decide(
    step: &StepDefinition,
    skipped: &HashSet<String>,
    vars: &serde_json::Map<String, serde_json::Value>,
    results: &HashMap<String, serde_json::Value>,
    exports: &HashMap<String, serde_json::Value>,
) -> StepDecision {
    if !step.run_if_skipped && step.depends_on.iter().any(|dep| skipped.contains(dep.as_str())) {
        return StepDecision::Skip(SkipReason::Dependency);
    }
    if let Some(when) = &step.when
        && !when.evaluate(vars, results, exports)
    {
        return StepDecision::Skip(SkipReason::When);
    }
    StepDecision::Run
}

/// Applique les décisions d'exécution [`decide`] d'une couche de steps prêts — l'étape a' de la
/// boucle de [`DagInterpreter::run`], extraite ici (case « câblage de l'étape a' prouvé par du
/// code partagé », 2026-11-18, réserve du validator sur la case `when`) : `run` et la simulation
/// de test `marche_simulee` (bloc `mod tests` de ce fichier) partagent cette unique
/// implémentation, plus de copie parallèle de la boucle. Déterministe et sans `ctx` (mêmes
/// entrées, mêmes sorties — rejeu identique après crash, `NFR-1`) ; ses seuls effets sont les
/// insertions dans `completed` et `skipped`, dans l'ordre de la couche. Rend : les steps à
/// dispatcher (ordre de la couche conservé) et la liste `(step_id, raison)` des ignorés à tracer
/// par l'appelant — `step_id` et raison uniquement, jamais une valeur lue.
pub(crate) fn apply_decisions<'a>(
    prets: Vec<&'a StepDefinition>,
    completed: &mut HashSet<String>,
    skipped: &mut HashSet<String>,
    vars: &serde_json::Map<String, serde_json::Value>,
    results: &HashMap<String, serde_json::Value>,
    exports: &HashMap<String, serde_json::Value>,
) -> (Vec<&'a StepDefinition>, Vec<(String, SkipReason)>) {
    let mut a_lancer: Vec<&'a StepDefinition> = Vec::with_capacity(prets.len());
    let mut a_tracer: Vec<(String, SkipReason)> = Vec::new();
    for step in prets {
        if let StepDecision::Skip(raison) = decide(step, skipped, vars, results, exports) {
            completed.insert(step.id.clone());
            skipped.insert(step.id.clone());
            a_tracer.push((step.id.clone(), raison));
        } else {
            a_lancer.push(step);
        }
    }
    (a_lancer, a_tracer)
}

/// Valeurs des exports nommés d'un step terminé (amendement 2026-11-18, issue #85, DSD
/// `workflow` `FR-21`) : pour chaque `(nom, pointeur)` de `step.exports` (ordre déterministe du
/// `BTreeMap`), la valeur `output.pointer(pointeur)` clonée quand elle existe. Un pointeur sans
/// résultat ne produit **aucune** entrée — la valeur est absente, jamais un `Value::Null` de
/// fabrication. Fonction pure, sans `ctx` : les sorties étant journalisées par Restate, la table
/// `exports` d'un run se reconstruit à l'identique au rejeu (`NFR-1`).
pub(crate) fn collect_exports(
    step: &StepDefinition,
    output: &serde_json::Value,
) -> Vec<(String, serde_json::Value)> {
    step.exports
        .iter()
        .filter_map(|(nom, pointeur)| {
            output
                .pointer(pointeur)
                .map(|valeur| (nom.clone(), valeur.clone()))
        })
        .collect()
}

/// Une entrée par id de `step.depends_on`, valeur = la sortie collectée du step amont dans
/// `results` ; un id présent dans `skipped` donne l'entrée `Value::Null` — l'entrée existe,
/// aucune ne manque (la table `results` d'un run n'a, elle, aucune clé pour un step ignoré).
/// Jamais une entrée de plus (un résultat disponible mais absent de `depends_on` ne traverse
/// pas), jamais une de moins. `build_inputs` n'est appelé que depuis l'étape 2.b de
/// [`DagInterpreter::run`] : chaque dépendance est alors dans `completed`, donc soit dans
/// `results` (exécuté — `results.get(id)` y est un `Some` à ce point de la marche), soit dans
/// `skipped`. Un double `None` (violation de cette invariante, chemin hors contrat) ne fabrique
/// jamais une valeur inexistante et ne panique jamais : l'entrée est omise, et c'est le step
/// aval qui la réclame qui la paie, côté ./dispatcher.rs, en `StepError` terminal du kind —
/// jamais en panic dans ce fichier.
pub(crate) fn build_inputs(
    step: &StepDefinition,
    results: &HashMap<String, serde_json::Value>,
    skipped: &HashSet<String>,
) -> HashMap<String, serde_json::Value> {
    step.depends_on
        .iter()
        .filter_map(|id| {
            if skipped.contains(id.as_str()) {
                Some((id.clone(), serde_json::Value::Null))
            } else {
                results.get(id).map(|sortie| (id.clone(), sortie.clone()))
            }
        })
        .collect()
}

/// Assemble une `StepInvocation` depuis un step — fonction pure, extrait de l'étape 2.b de
/// [`DagInterpreter::run`] (amendement 2026-10-30, lot B : le passage des `vars` est la partie
/// décidable sans `WorkflowContext`, même rationale de séparaison que [`build_inputs`]).
pub(crate) fn build_invocation(
    step: &StepDefinition,
    inputs: HashMap<String, serde_json::Value>,
    run_key: &str,
    depth: u32,
    vars: &serde_json::Map<String, serde_json::Value>,
    trigger: Option<&str>,
) -> StepInvocation {
    StepInvocation {
        kind: step.kind.clone(),
        config: step.config.clone(),
        inputs,
        run_key: run_key.to_string(),
        step_id: step.id.clone(),
        depth,
        vars: serde_json::Value::Object(vars.clone()),
        // T4 (amendement 2026-11-02, lot 0 lymaille, FR-10/DEC-12) : le trigger du run, recopié
        // tel quel, `None` rendu `None`.
        trigger: trigger.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::RunInput;
    use super::SkipReason;
    use super::StepDecision;
    use super::apply_decisions;
    use super::build_inputs;
    use super::build_invocation;
    use super::collect_exports;
    use super::decide;
    use super::parse_depth;
    use super::ready_steps;
    use crate::workflow::definition::DagSteps;
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
            when: None,
            run_if_skipped: false,
            exports: std::collections::BTreeMap::new(),
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

        let inputs = build_inputs(&b, &results, &HashSet::new());

        assert_eq!(
            inputs,
            HashMap::from([("A".to_string(), json!(1))]),
            "exactement une entrée — `\"A\"` → `json!(1)`, jamais `\"C\"` bien que présent dans \
             `results`"
        );
    }

    /// Scenario « `parse_depth` lit un entier valide » (#26) : la valeur brute `"2"` de
    /// l'en-tête de profondeur rend `2`.
    #[test]
    fn parse_depth_lit_un_entier_valide() {
        assert_eq!(parse_depth(Some("2")), 2);
    }

    /// Scenario « `parse_depth` rend 0 pour un en-tête absent, non numérique ou hors plage »
    /// (#26) : `None`, `"abc"`, `"-1"` et `"99999999999"` (hors `u32`) rendent tous `0` — jamais
    /// de panic, jamais d'erreur. Un run déclenché par `client::trigger_run` n'porte pas
    /// l'en-tête : `0` est son cas normal, pas une anomalie.
    #[test]
    fn parse_depth_rend_0_pour_un_en_tete_absent_non_numerique_ou_hors_plage() {
        for raw in [None, Some("abc"), Some("-1"), Some("99999999999")] {
            assert_eq!(
                parse_depth(raw),
                0,
                "`{raw:?}` doit rendre 0, sans panic ni erreur"
            );
        }
    }

    /// Scenario « `build_invocation` recopie les vars sans les modifier » (2026-10-30, lot B,
    /// B4) : l'unique endroit où une `StepInvocation` est assemblée depuis un step rend tous ses
    /// champs identitaires tels qu'émis et les `vars` verbatim en `Value::Object` — le passage
    /// des variables du run est la seule partie du contrat B4 décidable sans `WorkflowContext`
    /// (la boucle de `run` elle-même reste hors unitaire, borne des `Tasks`).
    #[test]
    fn build_invocation_recopie_les_vars_sans_les_modifier() {
        let etape = StepDefinition {
            id: "b".to_string(),
            depends_on: vec!["a".to_string()],
            kind: "echo".to_string(),
            config: json!({"m": 1}),
            when: None,
            run_if_skipped: false,
            exports: std::collections::BTreeMap::new(),
        };
        let vars = json!({"project": "p1"})
            .as_object()
            .expect("les vars de la fixture sont un objet")
            .clone();

        let invocation = build_invocation(
            &etape,
            HashMap::from([("a".to_string(), json!(7))]),
            "r1",
            2,
            &vars,
            None,
        );

        assert_eq!(invocation.kind, "echo", "le `kind` du step, cloné");
        assert_eq!(invocation.config, json!({"m": 1}), "la `config` du step, clonée");
        assert_eq!(
            invocation.inputs,
            HashMap::from([("a".to_string(), json!(7))]),
            "les `inputs` transmis tels quels"
        );
        assert_eq!(invocation.run_key, "r1");
        assert_eq!(invocation.step_id, "b", "l'`id` du step, cloné");
        assert_eq!(invocation.depth, 2);
        assert_eq!(
            invocation.vars,
            json!({"project": "p1"}),
            "les vars recopiées en `Value::Object`, sans modification"
        );
    }

    /// Scenario « un corps sans vars se désérialise avec des vars vides » (2026-10-30, lot B,
    /// B4) : les trois volets du `Then` — `{"dag": [a]}` seul donne un `dag` d'un step `a` et
    /// `vars = {}` (`#[serde(default)]`) ; `"vars": {"project": "p1"}` porte cette valeur ;
    /// l'ancienne forme (tableau nu de steps, avant l'amendement `FR-12`) est **refusée** —
    /// changement cassant assumé, API instable avant 1.0.
    #[test]
    fn un_corps_sans_vars_se_deserialise_avec_des_vars_vides() {
        let sans_vars: RunInput = serde_json::from_value(json!({
            "dag": [{ "id": "a", "depends_on": [], "kind": "echo", "config": {} }]
        }))
        .expect("`{\"dag\": [a]}` doit se désérialiser en `RunInput`");
        assert_eq!(sans_vars.dag.0.len(), 1, "un seul step attendu");
        assert_eq!(sans_vars.dag.0[0].id, "a");
        assert!(
            sans_vars.vars.is_empty(),
            "un corps sans `vars` donne {{}}, rendu : {:?}",
            sans_vars.vars
        );

        let avec_vars: RunInput = serde_json::from_value(json!({
            "dag": [{ "id": "a", "depends_on": [], "kind": "echo", "config": {} }],
            "vars": { "project": "p1" }
        }))
        .expect("`{\"dag\": [a], \"vars\": {..}}` doit se désérialiser en `RunInput`");
        assert_eq!(
            avec_vars.vars.get("project"),
            Some(&json!("p1")),
            "les vars portées par le corps traversent telles quelles"
        );

        let corps_nu_refuse = serde_json::from_value::<RunInput>(json!([
            { "id": "a", "depends_on": [], "kind": "echo", "config": {} }
        ]))
        .is_err();
        assert!(
            corps_nu_refuse,
            "l'ancienne forme (tableau nu de steps) ne doit plus être acceptée"
        );
    }

    /// Scenario « `build_invocation` recopie le trigger » (2026-11-02, lot 0 lymaille, T4,
    /// `FR-10`/`DEC-12`) : le `trigger` passé à l'unique assembleur de `StepInvocation` traverse
    /// en `Option<String>` tel quel — `Some("sub-42")` rendu `Some("sub-42")`, `None` (run sans
    /// identité) rendu `None`, jamais une chaîne vide fabriquée.
    #[test]
    fn build_invocation_recopie_le_trigger() {
        let etape = StepDefinition {
            id: "b".to_string(),
            depends_on: Vec::new(),
            kind: "echo".to_string(),
            config: json!({}),
            when: None,
            run_if_skipped: false,
            exports: std::collections::BTreeMap::new(),
        };

        let declenche = build_invocation(
            &etape,
            HashMap::new(),
            "r1",
            0,
            &serde_json::Map::new(),
            Some("sub-42"),
        );
        assert_eq!(
            declenche.trigger.as_deref(),
            Some("sub-42"),
            "le trigger du run doit être recopié tel quel dans l'invocation"
        );

        let neutre = build_invocation(&etape, HashMap::new(), "r1", 0, &serde_json::Map::new(), None);
        assert_eq!(
            neutre.trigger, None,
            "un run sans trigger donne une invocation sans trigger, jamais une chaîne vide"
        );
    }

    /// Scenario « le trigger du `RunInput` est facultatif et omis quand il est absent »
    /// (2026-11-02, lot 0 lymaille, T4, `FR-10`/`DEC-12`) : les volets du `Then` — un corps
    /// `{"dag": [a]}` sans clé `trigger` se désérialise en `None` (`#[serde(default)]`, les corps
    /// d'avant T4 et ceux de `trigger_run` avant T5 restent acceptés) ; `"trigger": "sub-42"`
    /// porte cette valeur ; et un `RunInput` de `trigger: None` ne réécrit pas la clé
    /// (`skip_serializing_if`), un `Some` l'écrit telle quelle.
    #[test]
    fn le_trigger_du_run_input_est_facultatif_et_omis_quand_il_est_absent() {
        let sans_trigger: RunInput = serde_json::from_value(json!({
            "dag": [{ "id": "a", "depends_on": [], "kind": "echo", "config": {} }]
        }))
        .expect("`{\"dag\": [a]}` doit se désérialiser en `RunInput`");
        assert_eq!(
            sans_trigger.trigger, None,
            "un corps sans clé `trigger` donne None"
        );

        let avec_trigger: RunInput = serde_json::from_value(json!({
            "dag": [{ "id": "a", "depends_on": [], "kind": "echo", "config": {} }],
            "trigger": "sub-42"
        }))
        .expect("`{\"dag\": [a], \"trigger\": \"sub-42\"}` doit se désérialiser en `RunInput`");
        assert_eq!(
            avec_trigger.trigger.as_deref(),
            Some("sub-42"),
            "le `trigger` du corps doit traverser tel quel"
        );

        let corps_sans_declencheur = serde_json::to_value(RunInput {
            dag: DagSteps(vec![etape("a", &[])]),
            vars: serde_json::Map::new(),
            trigger: None,
        })
        .expect("un `RunInput` se sérialise");
        assert!(
            corps_sans_declencheur.get("trigger").is_none(),
            "un `trigger: None` ne doit pas être écrit : {corps_sans_declencheur}"
        );

        let corps_declenche = serde_json::to_value(RunInput {
            dag: DagSteps(vec![etape("a", &[])]),
            vars: serde_json::Map::new(),
            trigger: Some("sub-42".to_string()),
        })
        .expect("un `RunInput` se sérialise");
        assert_eq!(
            corps_declenche.get("trigger"),
            Some(&json!("sub-42")),
            "un `Some` s'écrit sous la clé `trigger`, verbatim : {corps_declenche}"
        );
    }

    // ---------------------------------------------------------------------------
    // Conditions d'exécution `when` (issue #84, DSD `workflow` `FR-20`/`DEC-14`) —
    // `decide` et `build_inputs` se testent sans contexte Restate (mêmes bornes de
    // tranchage que les `Scenario` précédents : la boucle de `run` elle-même reste couverte
    // par les intégrations `#[ignore]` de ../../tests/workflow_restate.rs).
    // ---------------------------------------------------------------------------

    /// `when` de fixture, construit par sa forme JSON réelle (./predicate.rs).
    fn quand(forme: serde_json::Value) -> crate::workflow::predicate::Predicate {
        serde_json::from_value(forme).expect("la fixture `when` est un prédicat désérialisable")
    }

    fn sans_quand(id: &str, depends_on: &[&str]) -> StepDefinition {
        etape(id, depends_on)
    }

    /// `skipped` de fixture, à partir d'ids.
    fn skipped(ids: &[&str]) -> HashSet<String> {
        ids.iter().copied().map(String::from).collect()
    }

    /// Marche simulée de l'étape 2 de `run` par les seules fonctions pures : `ready_steps`,
    /// l'application des décisions par [`apply_decisions`] et la collecte des exports par
    /// [`collect_exports`] — le code partagé de `run` lui-même, plus de copie parallèle de la
    /// boucle (cases « câblage de l'étape a' prouvé par du code partagé » et « Exports nommés »,
    /// 2026-11-18). Le seul pas propre à la simulation est l'exécution elle-même : une identité
    /// journalisable (`{ "sortie": <id> }`), seule la décision des steps compte ici.
    fn marche_simulee(
        dag: &[StepDefinition],
        vars: &serde_json::Map<String, serde_json::Value>,
    ) -> (HashSet<String>, HashMap<String, serde_json::Value>, usize) {
        let mut completed: HashSet<String> = HashSet::new();
        let mut ignores: HashSet<String> = HashSet::new();
        let mut results: HashMap<String, serde_json::Value> = HashMap::new();
        // Table des exports résolus, alimentée par `collect_exports` comme dans `run` (#85).
        let mut exports: HashMap<String, serde_json::Value> = HashMap::new();
        let mut couches_dispatchees = 0usize;
        while completed.len() != dag.len() {
            let prepares = ready_steps(dag, &completed);
            let (a_lancer, _a_tracer) =
                apply_decisions(prepares, &mut completed, &mut ignores, vars, &results, &exports);
            if !a_lancer.is_empty() {
                couches_dispatchees = couches_dispatchees.saturating_add(1);
                for etape in &a_lancer {
                    let _ = build_inputs(etape, &results, &ignores);
                    let sortie = json!({ "sortie": etape.id });
                    // Les exports sont collectés par `collect_exports` — la même fonction que
                    // `run` (case « Exports nommés » de ./interpreter.sdd, #85).
                    for (nom, valeur) in collect_exports(etape, &sortie) {
                        exports.insert(nom, valeur);
                    }
                    results.insert(etape.id.clone(), sortie);
                    completed.insert(etape.id.clone());
                }
            }
        }
        (completed, results, couches_dispatchees)
    }

    /// Scenario « un step dont le `when` est faux est ignoré, sans dispatch » (#84) : `B` dépend
    /// de `"A"` et lit `/status eq "ok"` dans sa sortie — `Skip(When)` sur `"ko"`, `Run` sur
    /// `"ok"`.
    #[test]
    fn un_step_dont_le_when_est_faux_est_ignore_sans_dispatch() {
        let mut b = sans_quand("B", &["A"]);
        b.when = Some(quand(
            json!({ "step": "A", "path": "/status", "op": "eq", "value": "ok" }),
        ));
        let vars = serde_json::Map::new();
        let vide = HashSet::new();

        let ko: HashMap<String, serde_json::Value> =
            HashMap::from([("A".to_string(), json!({ "status": "ko" }))]);
        assert_eq!(
            decide(&b, &vide, &vars, &ko, &HashMap::new()),
            StepDecision::Skip(SkipReason::When),
            "`when` faux : ignorance, pas de dispatch"
        );

        let ok: HashMap<String, serde_json::Value> =
            HashMap::from([("A".to_string(), json!({ "status": "ok" }))]);
        assert_eq!(
            decide(&b, &vide, &vars, &ok, &HashMap::new()),
            StepDecision::Run,
            "`when` vrai : exécution"
        );
    }

    /// Scenario « l'ignorance se propage aux dépendants par défaut » (#84) : `C` dépend de `"B"`
    /// ignoré, aucun `when` — `Skip(Dependency)`.
    #[test]
    fn l_ignorance_se_propage_aux_dependants_par_defaut() {
        let c = sans_quand("C", &["B"]);
        let vars = serde_json::Map::new();
        let results = HashMap::new();
        assert_eq!(
            decide(&c, &skipped(&["B"]), &vars, &results, &HashMap::new()),
            StepDecision::Skip(SkipReason::Dependency),
            "`run_if_skipped` faux par défaut : l'ignorance se propage, même sans `when`"
        );
    }

    /// Scenario « `run_if_skipped` fait s'exécuter le dépendant malgré l'ignorance » (#84) : `C`
    /// avec `run_if_skipped: true` sur dépendance ignorée rend `Run`, et ses `inputs` portent
    /// l'entrée `"B"` à `Value::Null` — l'entrée existe, aucune ne manque.
    #[test]
    fn run_if_skipped_fait_s_executer_le_dependant_malgre_l_ignorance() {
        let mut c = sans_quand("C", &["B"]);
        c.run_if_skipped = true;
        let vars = serde_json::Map::new();
        let results = HashMap::new();
        let ignores = skipped(&["B"]);

        assert_eq!(
            decide(&c, &ignores, &vars, &results, &HashMap::new()),
            StepDecision::Run,
            "`run_if_skipped: true` : le dépendant s'exécute malgré l'ignorance de `\"B\"`"
        );
        assert_eq!(
            build_inputs(&c, &results, &ignores),
            HashMap::from([("B".to_string(), serde_json::Value::Null)]),
            "l'entrée de la dépendance ignorée existe, à `Value::Null`"
        );
    }

    /// Scenario « un step ignoré ne figure pas dans la table de sortie du run » (#84) : DAG
    /// `A → B → C`, `B` ignoré par un `when` faux, `C` par propagation — la simulation de la
    /// boucle rend une `results` réduite à `"A"` et un `completed` des trois steps. (Le
    /// câblage réel dans `run` relève des intégrations `#[ignore]` — borne des `Tasks`.)
    #[test]
    fn un_step_ignore_ne_figure_pas_dans_la_table_de_sortie_du_run() {
        let mut b = sans_quand("B", &["A"]);
        b.when = Some(quand(
            json!({ "step": "A", "path": "/statut_inexistant", "op": "eq", "value": "ok" }),
        ));
        let dag = [sans_quand("A", &[]), b, sans_quand("C", &["B"])];

        let (completed, results, _) = marche_simulee(&dag, &serde_json::Map::new());

        assert_eq!(
            results.keys().map(String::as_str).collect::<HashSet<_>>(),
            HashSet::from(["A"]),
            "aucune clé pour un step ignoré dans la table de sortie"
        );
        assert_eq!(
            completed,
            skipped(&["A", "B", "C"]),
            "`completed` a bien contenu les trois steps (ignorés comme terminés)"
        );
    }

    /// Scenario « les décisions sont déterministes au rejeu » (#84) : mêmes `vars`, `results` et
    /// `skipped` deux runs durant, `decide` rend la même décision pour chaque step, sans aucun
    /// `ctx` (`decide` ne prend pas de contexte).
    #[test]
    fn les_decisions_sont_deterministes_au_rejeu() {
        let mut b = sans_quand("B", &["A"]);
        b.when = Some(quand(json!({ "step": "A", "path": "/x", "op": "exists" })));
        let dag = [sans_quand("A", &[]), b];
        let vars = serde_json::Map::new();
        let vide = HashSet::new();
        let results: HashMap<String, serde_json::Value> = HashMap::from([("A".to_string(), json!({}))]);

        let premiere: Vec<StepDecision> = dag
            .iter()
            .map(|etape| decide(etape, &vide, &vars, &results, &HashMap::new()))
            .collect();
        let seconde: Vec<StepDecision> = dag
            .iter()
            .map(|etape| decide(etape, &vide, &vars, &results, &HashMap::new()))
            .collect();
        assert_eq!(
            premiere, seconde,
            "rejeu identique : les mêmes entrées rendent les mêmes décisions"
        );
        assert_eq!(
            premiere,
            vec![StepDecision::Run, StepDecision::Skip(SkipReason::When),],
            "et ces décisions sont exactement celles du contrat (`/x` absent de la sortie de `\"A\"`)"
        );
    }

    /// Scenario « une couche entièrement ignorée ne bloque pas la boucle » (#84) : DAG `A → B`,
    /// `B` ignoré (`when` faux) — la couche de `B` ne construit aucun futur et le run se termine.
    /// (Vérifié par la simulation des fonctions pures ; le non-constructeur de
    /// `DurableFuturesUnordered` dans `run` lui-même relève des intégrations `#[ignore]`.)
    #[test]
    fn une_couche_entierement_ignoree_ne_bloque_pas_la_boucle() {
        let mut b = sans_quand("B", &["A"]);
        b.when = Some(quand(json!({ "step": "A", "path": "/x", "op": "exists" })));
        let dag = [sans_quand("A", &[]), b];

        let (completed, results, couches) = marche_simulee(&dag, &serde_json::Map::new());

        assert_eq!(
            couches, 1,
            "une seule couche dispatchée : celle de `\"A\"`, celle de `\"B\"` aucune"
        );
        assert!(!results.contains_key("B"), "`B` ignoré n'a pas de sortie");
        assert_eq!(
            completed,
            skipped(&["A", "B"]),
            "le run se termine, les deux steps soldés"
        );
    }

    /// Case « Câblage de l'étape a' prouvé par du code partagé » (2026-11-18, réserve du
    /// validator sur la case `when`) : `apply_decisions` est le seul endroit qui applique les
    /// décisions d'une couche — appelé par `run` comme par [`marche_simulee`]. Ce test échoue si
    /// l'une des deux insertions disparaît de la fonction partagée (mutation vérifiée à
    /// l'implémentation) : sans `completed.insert`, la boucle de marche n'avance plus et
    /// l'assertion sur `completed` rougit ; sans `skipped.insert`, la propagation est aveugle et
    /// l'assertion sur `skipped` rougit.
    #[test]
    fn apply_decisions_enregistre_les_ignorances_dans_completed_et_skipped() {
        let mut b = sans_quand("B", &["A"]);
        b.when = Some(quand(json!({ "step": "A", "path": "/x", "op": "exists" })));
        let d = sans_quand("D", &[]);
        let mut completed = HashSet::from(["A".to_string()]);
        let mut ignores = HashSet::new();
        let results: HashMap<String, serde_json::Value> = HashMap::from([("A".to_string(), json!({}))]);

        let (a_lancer, a_tracer) = apply_decisions(
            vec![&b, &d],
            &mut completed,
            &mut ignores,
            &serde_json::Map::new(),
            &results,
            &HashMap::new(),
        );

        assert_eq!(
            ids(&a_lancer),
            vec!["D"],
            "seul le step `Run` repart vers le dispatch"
        );
        assert_eq!(
            completed,
            skipped(&["A", "B"]),
            "un step ignoré entre dans `completed` — la boucle de marche avance et se termine"
        );
        assert_eq!(
            ignores,
            skipped(&["B"]),
            "un step ignoré entre dans `skipped` — la propagation des ignorances en dépend"
        );
        assert_eq!(
            a_tracer,
            vec![("B".to_string(), SkipReason::When)],
            "la liste à tracer porte l'ignoré et sa raison, jamais le step exécuté"
        );
    }

    // ---------------------------------------------------------------------------
    // Exports nommés (issue #85, DSD `workflow` `FR-21`/`DEC-15`) — `collect_exports`, table
    // `exports` de la marche, paramètre `exports` de `decide` et `apply_decisions`. Mêmes bornes
    // de tranchage que `when` (#84) : la boucle de `run` elle-même relève des intégrations
    // `#[ignore]` de ../../tests/workflow_restate.rs ; la simulation `marche_simulee` passe par
    // les mêmes fonctions que `run`, dont `collect_exports`.
    // ---------------------------------------------------------------------------

    /// Step déclarant des exports de fixture (forme du `BTreeMap` de ./definition.rs).
    fn etape_avec_exports(id: &str, depends_on: &[&str], exports: &[(&str, &str)]) -> StepDefinition {
        let mut s = sans_quand(id, depends_on);
        s.exports = exports
            .iter()
            .map(|(nom, pointeur)| ((*nom).to_string(), (*pointeur).to_string()))
            .collect();
        s
    }

    /// Scenario « `collect_exports` extrait les valeurs par pointeur » (#85) : `{"tout": "",
    /// "verdict": "/analysis/verdict"}` sur la sortie `{"analysis": {"verdict": "ok"}}` rend
    /// `[("tout", la sortie entière), ("verdict", "ok")]`, dans l'ordre des noms.
    #[test]
    fn collect_exports_extrait_les_valeurs_par_pointeur() {
        let etape = etape_avec_exports("A", &[], &[("verdict", "/analysis/verdict"), ("tout", "")]);
        let sortie = json!({ "analysis": { "verdict": "ok" } });

        assert_eq!(
            collect_exports(&etape, &sortie),
            vec![
                ("tout".to_string(), sortie.clone()),
                ("verdict".to_string(), json!("ok"))
            ],
            "le pointeur vide rend la sortie entière, l'autre sa cible — dans l'ordre des noms"
        );
    }

    /// Scenario « un pointeur sans résultat ne produit aucun export » (#85) : `{"manquant":
    /// "/absent"}` sur `{"v": 1}` rend la liste vide — valeur absente, jamais une entrée à
    /// `Value::Null`.
    #[test]
    fn un_pointeur_sans_resultat_ne_produit_aucun_export() {
        let etape = etape_avec_exports("A", &[], &[("manquant", "/absent")]);
        assert_eq!(
            collect_exports(&etape, &json!({ "v": 1 })),
            Vec::new(),
            "un pointeur qui ne trouve rien ne produit aucune entrée d'export"
        );
    }

    /// Scenario « un step lit l'export d'un ancêtre non direct » (#85) : `decide` avec la table
    /// `{"verdict": "ok"}` rend `Run`, avec `{"verdict": "ko"}` et avec une table vide
    /// `Skip(When)` ; la marche simulée — `A` déclare `verdict` (`/sortie`), `C` ne dépend que
    /// de `B` — alimente la table par `collect_exports`, la même fonction que `run` : `C` s'y
    /// exerce quand l'export porte la valeur lue, jamais quand elle diffère.
    #[test]
    fn un_step_lit_lexport_d_un_ancetre_non_direct() {
        let mut c = sans_quand("C", &["B"]);
        c.when = Some(quand(json!({ "export": "verdict", "op": "eq", "value": "ok" })));
        let vars = serde_json::Map::new();
        let vide = HashSet::new();
        let results = HashMap::new();

        let ok: HashMap<String, serde_json::Value> = HashMap::from([("verdict".to_string(), json!("ok"))]);
        assert_eq!(
            decide(&c, &vide, &vars, &results, &ok),
            StepDecision::Run,
            "`verdict` vaut `\"ok\"` : le `when` est vrai, malgré un producteur non direct"
        );
        let ko: HashMap<String, serde_json::Value> = HashMap::from([("verdict".to_string(), json!("ko"))]);
        assert_eq!(
            decide(&c, &vide, &vars, &results, &ko),
            StepDecision::Skip(SkipReason::When),
            "`verdict` vaut `\"ko\"` : ignorance par `when`"
        );
        assert_eq!(
            decide(&c, &vide, &vars, &results, &HashMap::new()),
            StepDecision::Skip(SkipReason::When),
            "table vide : valeur absente, donc ignorance"
        );

        // ————— Volet marche simulée : la table d'exports est alimentée par `collect_exports` à
        // la fin de chaque step exécuté, comme dans `run` (étape 2.e).
        let mut c_qui_passe = sans_quand("C", &["B"]);
        c_qui_passe.when = Some(quand(json!({ "export": "verdict", "op": "eq", "value": "A" })));
        let dag = [
            etape_avec_exports("A", &[], &[("verdict", "/sortie")]),
            sans_quand("B", &["A"]),
            c_qui_passe,
        ];
        let (completed, results_run, _) = marche_simulee(&dag, &serde_json::Map::new());
        assert!(
            results_run.contains_key("C"),
            "`verdict` vaut `\"A\"` (sortie simulée de `\"A\"`) : `C` s'exécute — {results_run:?}"
        );
        assert_eq!(completed, skipped(&["A", "B", "C"]));

        let mut c_qui_tombe = sans_quand("C", &["B"]);
        c_qui_tombe.when = Some(quand(
            json!({ "export": "verdict", "op": "eq", "value": "autre" }),
        ));
        let dag_faux = [
            etape_avec_exports("A", &[], &[("verdict", "/sortie")]),
            sans_quand("B", &["A"]),
            c_qui_tombe,
        ];
        let (_, results_faux, _) = marche_simulee(&dag_faux, &serde_json::Map::new());
        assert!(
            !results_faux.contains_key("C"),
            "l'export simulé ne porte pas `\"autre\"` : `C` est ignoré par son `when`"
        );
    }

    /// Scenario « l'export d'un producteur ignoré est absent et ne propage rien » (#85) : `A`
    /// ignoré laisse la table `exports` sans entrée ; `C`, sans `when`, qui ne dépend que de `B`
    /// non ignoré, rend `Run` — un export n'est pas une `depends_on`, il ne propage pas
    /// l'ignorance.
    #[test]
    fn l_export_d_un_producteur_ignore_est_absent_et_ne_propage_rien() {
        let c = sans_quand("C", &["B"]);
        assert_eq!(
            decide(
                &c,
                &HashSet::new(),
                &serde_json::Map::new(),
                &HashMap::new(),
                &HashMap::new(),
            ),
            StepDecision::Run,
            "`B` non ignoré, export absent : `Run` — l'absence d'un export ne propage rien"
        );
    }
}
