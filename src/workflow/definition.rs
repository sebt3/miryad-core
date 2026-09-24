//! Entité `WorkflowDefinition` (feature `workflow`) — contrat porté par `./definition.sdd`.
//!
//! Table `miryad_workflow_definitions` : le seul endroit où un DAG de workflow est stocké et
//! administré — CRUD gratuit sur REST/GraphQL/MCP par le mécanisme générique de la crate. Porte la
//! forme stockée d'un step ([`StepDefinition`], enveloppe de colonne [`DagSteps`]) et
//! [`validate_dag`], la seule validation structurelle de DAG de toute la crate (`MRD-WORKFLOW-004`
//! via [`WorkflowError::InvalidDag`]). La validation est déclenchée par les hooks
//! [`MiryadResource`] de création et de mise à jour (rendu `422`, erreur applicative de l'admin
//! qui écrit le DAG) et jamais par `ActiveModelBehavior::before_save` (rendu `500`, panne
//! serveur). La politique d'accès effective se pose une seule fois au démarrage par
//! [`configure_policy`] ; sans appel, elle vaut `AdminOnly` en lecture comme en écriture.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::OnceLock;

use sea_orm::ActiveValue;
use sea_orm::entity::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::auth::AuthPrincipal;
use crate::resource::AccessPolicy;
use crate::resource::HookError;
use crate::resource::MiryadResource;
use crate::workflow::error::WorkflowError;

/// La forme d'un step de workflow tel qu'il est stocké dans [`DagSteps`] : identité, dépendances
/// amont, et le couple `kind`/`config` que seul un `impl MiryadWorkflowStep` du registre de
/// l'application interprète. Aucune validation portée par le type lui-même : tout
/// [`StepDefinition`] isolé est constructible, seule la liste complète d'un DAG est validable
/// ([`validate_dag`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepDefinition {
    /// Identifiant du step, unique dans son DAG — cible des arêtes `depends_on`.
    pub id: String,
    /// Identifiants des steps amont dont les sorties alimentent ce step.
    pub depends_on: Vec<String>,
    /// Clé du kind de step, en face du `kind()` enregistré par l'application — opaque ici.
    pub kind: String,
    /// Configuration opaque du step, transmise telle quelle au kind — jamais interprétée ici.
    pub config: serde_json::Value,
}

/// Enveloppe fine d'une liste de steps : le type de la colonne `steps` (JSON) de [`Model`].
/// Nouvelle-type sans logique — `FromJsonQueryResult` (`sea-orm` 2.0.2, vérifié source) s'applique
/// à un struct, pas directement à un `Vec<StepDefinition>` nu ; la sérialisation reste le tableau
/// JSON des steps, sans objet enveloppe.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, FromJsonQueryResult)]
pub struct DagSteps(
    /// Les steps du DAG, dans l'ordre d'origine — l'ordre de la liste est celui des messages
    /// d'erreur de [`validate_dag`].
    pub Vec<StepDefinition>,
);

/// Seule validation structurelle de DAG de toute la crate, dans cet ordre exact, interrompue au
/// premier échec (aucun cumul) — chaque étape suppose les précédentes passées :
///
/// 1. liste vide → « le DAG ne contient aucun step » ;
/// 2. premier `id` déjà rencontré → « step id dupliqué: » ;
/// 3. première dépendance pointant hors du DAG → « le step {step} dépend de {dep}, absent de ce
///    DAG » ;
/// 4. cycle, par tri topologique de Kahn (degré entrant = taille de `depends_on`, file initiale =
///    steps de degré `0` dans l'ordre de la liste d'origine) ; le premier step de la liste
///    d'origine resté non traité identifie le message « cycle détecté impliquant le step {id} ».
///    L'auto-dépendance en est le cas particulier de cycle de longueur un, sans branche séparée.
///
/// Une chaîne vide est un `id` comme un autre, sans traitement spécial. `Ok(())` au-delà : la
/// forme du `kind` et du `config` de chaque step reste hors du périmètre de cette fonction.
pub(crate) fn validate_dag(steps: &[StepDefinition]) -> Result<(), WorkflowError> {
    if steps.is_empty() {
        return Err(WorkflowError::InvalidDag(
            "le DAG ne contient aucun step".to_string(),
        ));
    }

    // Étape 2 — parcours dans l'ordre de la liste, un `id` par ligne.
    let mut ids_vus: HashSet<&str> = HashSet::with_capacity(steps.len());
    for etape in steps {
        if !ids_vus.insert(etape.id.as_str()) {
            return Err(WorkflowError::InvalidDag(format!(
                "step id dupliqué: {}",
                etape.id
            )));
        }
    }

    // Étape 3 — première dépendance absente de l'ensemble des id, dans l'ordre de la liste puis
    // de `depends_on` (cet ensemble est sans doublon depuis l'étape 2).
    for etape in steps {
        for dep in &etape.depends_on {
            if !ids_vus.contains(dep.as_str()) {
                return Err(WorkflowError::InvalidDag(format!(
                    "le step {} dépend de {dep}, absent de ce DAG",
                    etape.id
                )));
            }
        }
    }

    // Étape 4 — tri topologique de Kahn. Une dépendance citée deux fois dans un même
    // `depends_on` compte deux arêtes entrantes et deux décrémenteurs : le degré ne revient
    // jamais sous zéro, `saturating_sub` porte seulement la garantie d'arithmétique du harnais.
    let mut degres: HashMap<&str, usize> = HashMap::with_capacity(steps.len());
    let mut aval: HashMap<&str, Vec<&str>> = HashMap::with_capacity(steps.len());
    for etape in steps {
        degres.insert(etape.id.as_str(), etape.depends_on.len());
    }
    for etape in steps {
        for dep in &etape.depends_on {
            aval.entry(dep.as_str()).or_default().push(etape.id.as_str());
        }
    }

    let mut file: VecDeque<&str> = VecDeque::new();
    for etape in steps {
        if degres.get(etape.id.as_str()) == Some(&0) {
            file.push_back(etape.id.as_str());
        }
    }

    // Un pas ne sort de la file qu'une fois (son degré n'atteint `0` qu'une seule fois) :
    // `traites` compte exactement les steps traités par l'algorithme.
    let mut traites: HashSet<&str> = HashSet::with_capacity(steps.len());
    while let Some(id) = file.pop_front() {
        traites.insert(id);
        if let Some(successeurs) = aval.get(id) {
            for successeur in successeurs {
                if let Some(degre) = degres.get_mut(successeur) {
                    *degre = degre.saturating_sub(1);
                    if *degre == 0 {
                        file.push_back(*successeur);
                    }
                }
            }
        }
    }

    // Steps restés non traités après épuisement de la file : le premier d'entre eux dans
    // l'ordre de la liste d'origine identifie le cycle.
    if traites.len() < steps.len()
        && let Some(etape) = steps.iter().find(|etape| !traites.contains(etape.id.as_str()))
    {
        return Err(WorkflowError::InvalidDag(format!(
            "cycle détecté impliquant le step {}",
            etape.id
        )));
    }

    Ok(())
}

/// Définition de workflow persistée — la table `miryad_workflow_definitions` posée par la
/// migration `m20260923_000001`, cinq colonnes du contrat `Must` de ./definition.sdd.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "miryad_workflow_definitions")]
pub struct Model {
    /// Clé primaire entière auto-incrémentée (convention `miryad_*` de la crate).
    #[sea_orm(primary_key)]
    pub id: i32,
    /// Nom unique de la définition — colonne de filtre et de libellé de la ressource.
    #[sea_orm(unique)]
    pub name: String,
    /// Le DAG lui-même, colonne JSON.
    pub steps: DagSteps,
    /// Propriétaire nullable, sans contrainte `FOREIGN KEY` — couplage lâche, même choix que
    /// `ApiToken::subject` (roadmap 2b).
    pub owner_id: Option<i32>,
    /// Horodatage de création (aucune entité de la crate ne porte d'`updated_at`).
    pub created_at: DateTimeUtc,
}

/// Aucune relation : `owner_id` n'est pas une clé étrangère, et un DAG ne référence que ses
/// propres steps, stockés dans la même ligne.
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// Politique d'accès effective de la ressource `workflows`. Le défaut — `AdminOnly` en lecture
/// comme en écriture — n'est qu'un défaut : une application fille le resserre ou l'assouplit
/// (ex. `OwnerOnly` : chaque utilisateur possède ses propres workflows) par [`configure_policy`]
/// au démarrage, jamais par une seconde implémentation de [`MiryadResource`] que les règles
/// orphelines de Rust lui refusent.
#[derive(Debug, Clone, Copy)]
pub struct WorkflowPolicy {
    /// Politique appliquée aux lectures de la ressource.
    pub read: AccessPolicy,
    /// Politique appliquée aux écritures de la ressource.
    pub write: AccessPolicy,
}

impl Default for WorkflowPolicy {
    /// Le défaut documentaire de ./definition.sdd : `AdminOnly` en lecture comme en écriture.
    fn default() -> Self {
        Self {
            read: AccessPolicy::AdminOnly,
            write: AccessPolicy::AdminOnly,
        }
    }
}

/// Cellule de configuration au démarrage — lue uniquement par les politiques de la ressource et
/// posée uniquement par [`configure_policy`] (les trois points d'accès de ./definition.sdd
/// `Must not`). Jamais posée, `read_policy`/`write_policy` retombent sur
/// `WorkflowPolicy::default()` ; `configure_policy` non appelée est un usage valide.
static POLICY: OnceLock<WorkflowPolicy> = OnceLock::new();

/// Fixe la politique d'accès effective de la ressource `workflows` — la seule façon de changer
/// le défaut `AdminOnly`/`AdminOnly`, à appeler au plus une fois avant que l'application ne serve
/// des requêtes (même idiome de configuration au démarrage que
/// [`crate::workflow::client::register_deployment`]).
///
/// # Errors
///
/// [`WorkflowError::PolicyAlreadySet`] (`MRD-WORKFLOW-005`) si une politique a déjà été posée : un
/// second appel est une erreur de configuration du démarrage de l'application (deux composants
/// qui tentent de fixer la politique), jamais un remplacement silencieux — la première politique
/// reste effective. Même traitement que la collision d'enregistrement de
/// `StepRegistry::register`.
pub fn configure_policy(policy: WorkflowPolicy) -> Result<(), WorkflowError> {
    POLICY.set(policy).map_err(|_| WorkflowError::PolicyAlreadySet)
}

/// Porte commune des hooks `before_create` et `before_update` : lit `steps` à la position
/// contractuelle du `Must` de ./definition.sdd — toujours `Set` ou `Unchanged` après
/// `rest::core::mark_all_set`, avant toute écriture — et traduit `WorkflowError::InvalidDag` en
/// [`HookError`] **sans code**, la partie libre du message seule : le préfixe
/// `MRD-WORKFLOW-004:` de la `Display` de [`WorkflowError`] ne traverse jamais un `HookError`
/// (contrat transverse). La branche `NotSet` est hors contrat à ce point de la séquence : no-op,
/// jamais un panic.
fn rejeter_si_dag_invalide(steps: &ActiveValue<DagSteps>) -> Result<(), HookError> {
    let etapes = match steps {
        ActiveValue::Set(dag) | ActiveValue::Unchanged(dag) => &dag.0,
        ActiveValue::NotSet => return Ok(()),
    };
    validate_dag(etapes).map_err(|erreur| {
        HookError::new(match erreur {
            WorkflowError::InvalidDag(message) => message,
            // Atteignable en pratique jamais : `validate_dag` ne construit que `InvalidDag`
            // (`Returns` de ./definition.sdd). L'alternative — ignorer l'erreur ou panic — serait
            // une faute ; ce repli garde le `HookError` sans code, contrat transverse.
            autre => autre.to_string(),
        })
    })
}

/// Métadonnées et hooks de la ressource `workflows`, lue à l'identique par REST, GraphQL et MCP.
/// L'asymétrie documentée (./../resource.sdd, amendement 2026-09-23) vaut ici comme partout :
/// `before_update` et `before_delete` ne se déclenchent que sur REST et MCP, pas sur GraphQL.
impl MiryadResource for Entity {
    fn resource_name() -> &'static str {
        "workflows"
    }

    fn read_policy() -> AccessPolicy {
        POLICY.get().copied().unwrap_or_default().read
    }

    fn write_policy() -> AccessPolicy {
        POLICY.get().copied().unwrap_or_default().write
    }

    /// Toujours `Some`, y compris sous le défaut `AdminOnly` : `rest::core` n'injecte ou ne filtre
    /// par cette colonne que sous une politique effective `OwnerOnly` — la déclarer sans l'agir
    /// est un no-op vérifié par la spec amont, pas un comportement non défini.
    fn owner_column() -> Option<<Self as EntityTrait>::Column> {
        Some(Column::OwnerId)
    }

    fn filter_column() -> Option<<Self as EntityTrait>::Column> {
        Some(Column::Name)
    }

    fn label_column() -> Option<<Self as EntityTrait>::Column> {
        Some(Column::Name)
    }

    /// Valide le DAG avant insertion, sans mutation de l'`ActiveModel` (contrairement à un hook
    /// applicatif typique qui dériverait un champ) ; see `rejeter_si_dag_invalide` pour la
    /// traduction de l'erreur.
    fn before_create(
        active: Self::ActiveModel,
        _principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        rejeter_si_dag_invalide(&active.steps)?;
        Ok(active)
    }

    /// Même traitement que [`Self::before_create`], appliqué au nouveau DAG dans l'absolu :
    /// `existing` n'est jamais lu, la validation ne compare pas l'ancien et le nouveau graphe —
    /// une correction d'un DAG invalide hérité passe exactement comme une première écriture.
    fn before_update(
        active: Self::ActiveModel,
        _existing: &Self::Model,
        _principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        rejeter_si_dag_invalide(&active.steps)?;
        Ok(active)
    }

    // `before_delete` volontairement non surdéclaré : aucune règle métier ne s'oppose à la
    // suppression d'une définition de workflow dans cette itération — le défaut identité du trait
    // (`Ok(())`) s'applique tel quel (./definition.sdd `Must`, `before_delete` `[ ]`).
}

#[cfg(test)]
mod tests {
    use super::ActiveModel;
    use super::Column;
    use super::DagSteps;
    use super::Entity;
    use super::Model;
    use super::StepDefinition;
    use super::validate_dag;
    use crate::auth::AuthPrincipal;
    use crate::auth::PrincipalSource;
    use crate::resource::AccessPolicy;
    use crate::resource::MiryadResource;
    use crate::workflow::error::WorkflowError;
    use chrono::Utc;
    use sea_orm::ActiveValue;
    use sea_orm::ActiveValue::NotSet;
    use sea_orm::ActiveValue::Set;

    // ── Fixtures — purement mémoire : `validate_dag` est une fonction pure et les hooks ne
    // touchent jamais la base (position contractuelle après `mark_all_set`, avant toute
    // écriture). La sobriété demandée par la tâche « Convertir les `Scenario` » s'applique :
    // aucune fixture sqlite n'est nécessaire aux `Then` de ces scenarios.

    /// Step de fixture : `kind` et `config` restent opaques — ./definition.rs ne les interprète
    /// jamais (le kind de `./rhai_step.sdd` n'est pas référencé ici, `Must not`).
    fn etape(id: &str, depends_on: &[&str]) -> StepDefinition {
        StepDefinition {
            id: id.to_string(),
            depends_on: depends_on.iter().copied().map(String::from).collect(),
            kind: "noop".to_string(),
            config: serde_json::Value::Null,
        }
    }

    /// Cycle minimal `"A" -> "B" -> "A"` — pré-condition de tout scenario de cycle.
    fn dag_cycle() -> Vec<StepDefinition> {
        vec![etape("A", &["B"]), etape("B", &["A"])]
    }

    /// DAG valide : `"A"` sans dépendance, `"B"` qui en dépend.
    fn dag_valide() -> Vec<StepDefinition> {
        vec![etape("A", &[]), etape("B", &["A"])]
    }

    fn principal() -> AuthPrincipal {
        AuthPrincipal {
            subject: "sujet-de-test".to_string(),
            email: None,
            source: PrincipalSource::ApiToken { token_id: 1 },
        }
    }

    /// `ActiveModel` dans la forme que produit `rest::core::mark_all_set` juste avant un hook :
    /// `steps` en `ActiveValue::Set`, clé et propriétaire non posés.
    fn active_avec(steps: Vec<StepDefinition>) -> ActiveModel {
        ActiveModel {
            id: NotSet,
            name: Set("définition-de-test".to_string()),
            steps: Set(DagSteps(steps)),
            owner_id: NotSet,
            created_at: Set(Utc::now()),
        }
    }

    /// Ligne lue (`existing` des hooks `before_update`/`before_delete`) avec les `steps` voulus.
    fn modele_avec(steps: Vec<StepDefinition>) -> Model {
        Model {
            id: 1,
            name: "définition-de-test".to_string(),
            steps: DagSteps(steps),
            owner_id: None,
            created_at: Utc::now(),
        }
    }

    /// Extrait le texte libre de `WorkflowError::InvalidDag` ; toute autre issue est une faute
    /// distincte, marquée rouge explicitement.
    fn message_invalid_dag(resultat: Result<(), WorkflowError>) -> String {
        match resultat {
            Err(WorkflowError::InvalidDag(message)) => message,
            Ok(()) => panic!("attendu `Err(WorkflowError::InvalidDag(..))`, reçu `Ok(())`"),
            Err(autre) => panic!("attendu `WorkflowError::InvalidDag`, reçu {autre:?}"),
        }
    }

    /// Scenario « DAG vide est invalide » : la cause vide sort en premier, aucune autre
    /// vérification n'est atteinte.
    #[test]
    fn validate_dag_rejete_le_dag_vide() {
        let message = message_invalid_dag(validate_dag(&[]));
        assert_eq!(message, "le DAG ne contient aucun step");
    }

    /// Scenario « id dupliqué est détecté avant toute autre vérification » : le second `"A"`
    /// porte aussi une dépendance pendante, qui ne doit jamais être vue.
    #[test]
    fn validate_dag_rejete_id_duplique_avant_dependance_pendante() {
        let message = message_invalid_dag(validate_dag(&[etape("A", &[]), etape("A", &["fantôme"])]));
        assert_eq!(message, "step id dupliqué: A");
    }

    /// Scenario « dépendance pendante est détectée avant le tri topologique » : le premier `dep`
    /// absent de l'ensemble des id, dans l'ordre de la liste puis de `depends_on`, identifie le
    /// message avec l'id du step dépendant substitué.
    #[test]
    fn validate_dag_rejete_dependance_pendante_avant_le_tri_topologique() {
        let message = message_invalid_dag(validate_dag(&[etape("A", &[]), etape("B", &["absent"])]));
        assert_eq!(message, "le step B dépend de absent, absent de ce DAG");
    }

    /// Scenario « cycle direct entre deux steps » : jamais `Ok`, jamais de boucle infinie, et le
    /// message mentionne `"A"`, premier step de la liste d'origine resté non traité.
    #[test]
    fn validate_dag_rejete_cycle_direct_entre_deux_steps() {
        let message = message_invalid_dag(validate_dag(&dag_cycle()));
        assert_eq!(message, "cycle détecté impliquant le step A");
    }

    /// Scenario « auto-dépendance est un cycle de longueur un » : `"A"` qui dépend de `"A"` est
    /// bien une dépendance résolue (pas pendante), et le message ne porte qu'un seul id.
    #[test]
    fn validate_dag_rejete_auto_dependance_comme_cycle_de_longueur_un() {
        let message = message_invalid_dag(validate_dag(&[etape("A", &["A"])]));
        assert_eq!(message, "cycle détecté impliquant le step A");
        assert!(
            !message.contains("absent de ce DAG"),
            "l'auto-dépendance n'est jamais une dépendance pendante : \"{message}\""
        );
    }

    /// Scenario « DAG valide à plusieurs couches passe sans erreur » : losange `"A"` → `"B"`,
    /// `"C"` → `"D"`.
    #[test]
    fn validate_dag_accepte_dag_valide_a_quatre_niveaux() {
        let dag = vec![
            etape("A", &[]),
            etape("B", &["A"]),
            etape("C", &["A"]),
            etape("D", &["B", "C"]),
        ];
        let resultat = validate_dag(&dag);
        assert!(
            resultat.is_ok(),
            "un DAG acyclique sans doublon doit passer : {resultat:?}"
        );
    }

    /// Scenario « `before_create` traduit `InvalidDag` en `HookError` sans code » : seule la partie
    /// libre du message traverse, jamais le préfixe `MRD-WORKFLOW-004:` de la `Display`.
    #[test]
    fn before_create_traduit_invalid_dag_en_hookerror_sans_code() {
        let resultat = Entity::before_create(active_avec(dag_cycle()), &principal());
        match resultat {
            Err(erreur) => {
                assert!(
                    erreur.code.is_none(),
                    "un `HookError` ne porte jamais de code, a fortiori un code `MRD-*`"
                );
                assert_eq!(erreur.message, "cycle détecté impliquant le step A");
                assert!(
                    !erreur.message.contains("MRD-WORKFLOW-004"),
                    "le préfixe `MRD-WORKFLOW-004:` ne traverse jamais : {erreur:?}"
                );
            }
            Ok(_) => panic!("le DAG cyclique devait être rejeté par `before_create`"),
        }
    }

    /// Scenario « `before_create` laisse passer un DAG valide sans le muter » : `Ok(active)`
    /// strictement identique à l'entrée, même valeur et même variante `ActiveValue`.
    #[test]
    fn before_create_laisse_passer_un_dag_valide_sans_le_muter() {
        let active = active_avec(dag_valide());
        let entre = active.clone();
        match Entity::before_create(active, &principal()) {
            Ok(rendu) => {
                assert_eq!(
                    rendu, entre,
                    "aucune mutation : le rendu est l'entrée champ à champ"
                );
                assert!(
                    matches!(rendu.steps, ActiveValue::Set(_)),
                    "la variante `ActiveValue::Set` d'origine est préservée"
                );
            }
            Err(erreur) => panic!("un DAG structurellement valide ne doit pas être rejeté : {erreur:?}"),
        }
    }

    /// Scenario « `before_update` rejette une mise à jour vers un DAG invalide, `existing` ignoré » :
    /// `existing` contient justement l'id `"absent"` ; une validation qui comparerait avant/après
    /// aurait laissé passer la mise à jour. Le rejet prouve que seul le nouveau DAG est jugé,
    /// dans l'absolu, comme à la création.
    #[test]
    fn before_update_rejette_dag_invalide_en_ignorant_existing() {
        let existing = modele_avec(vec![etape("A", &[]), etape("absent", &[])]);
        let resultat = Entity::before_update(
            active_avec(vec![etape("A", &[]), etape("X", &["absent"])]),
            &existing,
            &principal(),
        );
        match resultat {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "hook de la crate : `HookError` sans code");
                assert_eq!(erreur.message, "le step X dépend de absent, absent de ce DAG");
            }
            Ok(_) => panic!("la mise à jour vers un DAG à dépendance pendante devait être rejetée"),
        }
    }

    /// Scenario « `before_update` laisse passer une mise à jour vers un DAG valide » : un
    /// `existing` lui-même cyclique (écrit avant l'existence de la validation) ne bloque pas la
    /// mise à jour qui le corrige ; l'`ActiveModel` est rendu inchangé.
    #[test]
    fn before_update_accepte_dag_valide_malgre_existing_invalide() {
        let existing = modele_avec(dag_cycle());
        let active = active_avec(dag_valide());
        let entre = active.clone();
        match Entity::before_update(active, &existing, &principal()) {
            Ok(rendu) => assert_eq!(rendu, entre, "aucune mutation, `existing` jamais lu"),
            Err(erreur) => {
                panic!("un `existing` invalide ne doit pas bloquer une mise à jour valide : {erreur:?}")
            }
        }
    }

    /// Scenario « `before_delete` n'est pas surdéclaré, le défaut du trait s'applique » : aucune
    /// surdéclaration n'existe dans ./definition.rs, le défaut identité atteint donc même une
    /// ligne au DAG structurellement invalide — la suppression d'une définition n'est gardée par
    /// aucune règle dans cette itération.
    #[test]
    fn before_delete_defaut_du_trait_autorise_meme_un_dag_invalide() {
        let existing = modele_avec(dag_cycle());
        let resultat = Entity::before_delete(&existing, &principal());
        assert!(
            matches!(resultat, Ok(())),
            "le défaut `Ok(())` s'applique : {resultat:?}"
        );
    }

    /// Scenario « conformité `MiryadResource` sous le défaut, sans `configure_policy` » : les
    /// six métadonnées sont exactement celles du `Must`. Ce binaire de tests n'appelle jamais
    /// `configure_policy` (isolation `OnceLock` décidée en `Tasks` de ./definition.sdd : les
    /// scenarios qui la posent vivent dans `tests/workflow_policy.rs` et
    /// `tests/workflow_policy_twice.rs`, processus dédiés) — la cellule `POLICY` n'est donc
    /// jamais posée ici et le défaut est déterministe.
    #[test]
    fn conformite_miryad_resource_sous_la_politique_defaut() {
        assert_eq!(Entity::resource_name(), "workflows");
        assert_eq!(Entity::read_policy(), AccessPolicy::AdminOnly);
        assert_eq!(Entity::write_policy(), AccessPolicy::AdminOnly);
        assert!(
            matches!(Entity::owner_column(), Some(Column::OwnerId)),
            "`owner_column` est toujours `Some`, y compris sous le défaut `AdminOnly`"
        );
        assert!(matches!(Entity::filter_column(), Some(Column::Name)));
        assert!(matches!(Entity::label_column(), Some(Column::Name)));
    }
}
