//! Entité `lymaille_catalog_agents` (feature `lymaille`) — contrat porté par `./agent.sdd`.
//!
//! Table `miryad_lymaille_catalog_agents` : les définitions d'agent du catalogue, administrées
//! par l'admin au fil de l'eau par le CRUD REST et MCP générique. La colonne `definition` (JSON,
//! une `lymaille::AgentDefinition` complète) est la source de vérité (DSD `DEC-17`) ; `agent_id`,
//! `kind` et `profile` en sont des copies dérivées par les hooks de validation pour le filtrage
//! et le libellé (DSD `DEC-17`), et l'identifiant doit porter le préfixe `G_` du catalogue
//! (DSD `DEC-9`, `crate::lymaille::identity::classify`).

use sea_orm::ActiveValue;
use sea_orm::entity::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::auth::AuthPrincipal;
use crate::lymaille::identity;
use crate::resource::AccessPolicy;
use crate::resource::HookError;
use crate::resource::MiryadResource;

/// Définition d'agent du catalogue persistée — la table `miryad_lymaille_catalog_agents` posée
/// par `../migration.rs`, colonnes du contrat `Must` de ./agent.sdd.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS)]
#[ts(rename = "AgentModel")]
#[sea_orm(table_name = "miryad_lymaille_catalog_agents")]
pub struct Model {
    /// Clé primaire entière auto-incrémentée (convention `miryad_*` de la crate).
    #[sea_orm(primary_key)]
    pub id: i32,
    /// Identifiant de l'agent (`G_…`), copie dérivée de `definition.id` par les hooks — libellé
    /// de la ressource, unique au catalogue (contrainte de la migration).
    pub agent_id: String,
    /// Nature de l'agent (`work`, `config` ou `sub`), copie dérivée de `definition` par les
    /// hooks — colonne de filtre de la ressource.
    pub kind: String,
    /// Nom du profil de modèle désigné, copie dérivée de `definition` par les hooks.
    pub profile: String,
    /// L'`lymaille::AgentDefinition` complète, colonne JSON : la source de vérité (DSD
    /// `DEC-17`), validée par `before_create` et `before_update`.
    #[schema(value_type = Object)]
    #[ts(as = "::lymaille::AgentDefinition")]
    pub definition: serde_json::Value,
    /// Horodatage de création, posé par le hook de création (aucune entité de la crate ne porte
    /// d'`updated_at`).
    #[schema(value_type = String, format = DateTime)]
    #[ts(type = "string")]
    pub created_at: DateTimeUtc,
    /// Marque de la source hôte qui publie cette ligne (`../migration.rs`, issue #73 ; lue par
    /// `../store/publish.rs`) : `None` = ligne non gérée. Jamais écrite par le CRUD — les hooks
    /// ignorent la valeur reçue à la création et préservent celle de la ligne à la mise à jour
    /// (`./agent.sdd`, amendement du 2026-11-13).
    pub managed_by: Option<String>,
    /// Version du catalogue hôte qui a publié ou mis à jour cette ligne (`None` = non gérée).
    pub managed_version: Option<String>,
    /// Empreinte SHA-256 (hex) du contenu publié (`definition` telle que stockée) : recalculée à
    /// chaque comparaison par `../store/publish.rs`, jamais faire par le CRUD.
    pub managed_digest: Option<String>,
}

/// Aucune relation : la référence au profil est un nom, résolu à l'exécution, sans clé étrangère
/// (couplage lâche, comme `owner_id` de `miryad_workflow_definitions`).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Entity {
    /// Porte commune de `before_create` (`existing` absent) et `before_update`, dans l'ordre du
    /// `Must` de ./agent.sdd, interrompue au premier échec : la `definition` doit être un objet
    /// JSON, se désérialiser en `lymaille::AgentDefinition` et passer sa `validate` (la cause
    /// traverse, jamais le prompt) ; l'identifiant doit être du catalogue (`G_` exact, DSD
    /// `DEC-9`) — toute autre classification, y compris un identifiant Rhai invalide, est le
    /// même refus ; sur une mise à jour, l'identifiant ne change pas et `created_at` reste celui
    /// de la ligne existante. Les colonnes dérivées sont toujours réécrites (`Must not` : aucune
    /// ligne incohérente ne sort du hook). Le profil référencé n'est pas vérifié en base : un
    /// hook est synchrone et sans connexion.
    ///
    /// `pub(crate)` (issue #73, ./store/publish.sdd) : le chemin de publication du catalogue
    /// applique **la même** validation avant toute écriture, sans dupliquer une règle des hooks ;
    /// logique strictement inchangée.
    pub(crate) fn valider_et_deriver(
        mut active: ActiveModel,
        existing: Option<&Model>,
    ) -> Result<ActiveModel, HookError> {
        let brute = match &active.definition {
            ActiveValue::Set(valeur) | ActiveValue::Unchanged(valeur) if valeur.is_object() => Some(valeur),
            ActiveValue::Set(_) | ActiveValue::Unchanged(_) | ActiveValue::NotSet => None,
        };
        let Some(brute) = brute else {
            return Err(HookError::new("agent definition is required"));
        };
        let definition: ::lymaille::AgentDefinition = serde_json::from_value(brute.clone())
            .map_err(|cause| HookError::new(format!("invalid agent definition: {cause}")))?;
        definition
            .validate()
            .map_err(|cause| HookError::new(format!("invalid agent definition: {cause}")))?;
        if !matches!(identity::classify(definition.id()), Ok(identity::IdKind::Catalog)) {
            return Err(HookError::new("agent id must start with G_ (catalog)"));
        }
        if let Some(ligne) = existing
            && definition.id() != ligne.agent_id.as_str()
        {
            return Err(HookError::new("agent id cannot change"));
        }
        let Ok(serde_json::Value::String(nature)) = serde_json::to_value(definition.kind()) else {
            return Err(HookError::new("invalid agent definition"));
        };
        active.agent_id = ActiveValue::Set(definition.id().to_owned());
        active.kind = ActiveValue::Set(nature);
        // Issue #75 (./agent.sdd, amendements des 2026-11-13 et 2026-11-14) : une définition sans
        // profil explicite pose la chaîne vide dans cette colonne — « suit le profil par défaut de
        // l'hôte » (`../interactive/assemble.rs`) ; un profil désigné est copié inchangé. La
        // colonne reste `String` NOT NULL : aucune évolution de schéma (option A arbitrée).
        active.profile = ActiveValue::Set(definition.profile().unwrap_or_default().to_owned());
        if let Some(ligne) = existing {
            active.created_at = ActiveValue::Set(ligne.created_at);
            // Issue #73 : la provenance de la ligne relue est préservée — le CRUD ne peut
            // ni la forger ni l'effacer (amendement du 2026-11-13 de ./agent.sdd).
            active.managed_by = ActiveValue::Set(ligne.managed_by.clone());
            active.managed_version = ActiveValue::Set(ligne.managed_version.clone());
            active.managed_digest = ActiveValue::Set(ligne.managed_digest.clone());
        } else {
            if matches!(active.created_at, ActiveValue::NotSet) {
                active.created_at = ActiveValue::Set(chrono::Utc::now());
            }
            // Issue #73 : à la création, la provenance reçue est ignorée — la ligne naît
            // « non gérée » ; seul `../store/publish.rs` la pose.
            active.managed_by = ActiveValue::Set(None);
            active.managed_version = ActiveValue::Set(None);
            active.managed_digest = ActiveValue::Set(None);
        }
        Ok(active)
    }
}

/// Métadonnées et hooks de la ressource `lymaille_catalog_agents`, lue à l'identique par REST et
/// MCP. Catalogue (DSD `DEC-14`) : lecture par tout authentifié, écriture par l'admin seul, sans
/// propriétaire.
impl MiryadResource for Entity {
    fn resource_name() -> &'static str {
        "lymaille_catalog_agents"
    }

    fn read_policy() -> AccessPolicy {
        AccessPolicy::Public
    }

    fn write_policy() -> AccessPolicy {
        AccessPolicy::AdminOnly
    }

    fn owner_column() -> Option<<Self as EntityTrait>::Column> {
        None
    }

    fn filter_column() -> Option<<Self as EntityTrait>::Column> {
        Some(Column::Kind)
    }

    fn label_column() -> Option<<Self as EntityTrait>::Column> {
        Some(Column::AgentId)
    }

    /// Valide la définition et réécrit les colonnes dérivées — voir `Entity::valider_et_deriver`.
    fn before_create<C>(
        db: &C,
        active: Self::ActiveModel,
        _principal: &AuthPrincipal,
    ) -> impl std::future::Future<Output = Result<Self::ActiveModel, HookError>> + Send
    where
        C: sea_orm::ConnectionTrait,
    {
        let _ = db;
        async move { Entity::valider_et_deriver(active, None) }
    }

    /// Mêmes étapes que [`Self::before_create`] sur la nouvelle définition, plus l'identité :
    /// renommer un agent change son identité et est refusé ; `created_at` reste celui de la
    /// ligne existante.
    fn before_update<C>(
        db: &C,
        active: Self::ActiveModel,
        existing: &Self::Model,
        _principal: &AuthPrincipal,
    ) -> impl std::future::Future<Output = Result<Self::ActiveModel, HookError>> + Send
    where
        C: sea_orm::ConnectionTrait,
    {
        let _ = db;
        async move { Entity::valider_et_deriver(active, Some(existing)) }
    }

    // `before_delete` volontairement non surdéclaré : aucun contrôle d'usage au lot 1
    // (./agent.sdd `Must`).
}

#[cfg(test)]
mod tests {
    use super::ActiveModel;

    /// Connexion simulée pour appeler les hooks depuis les tests : la validation de ce module
    /// est pure et n'émet aucune requête ; une connexion à file vide suffit.
    fn hook_db() -> sea_orm::DatabaseConnection {
        sea_orm::MockDatabase::new(sea_orm::DbBackend::Sqlite).into_connection()
    }
    use super::Column;
    use super::Entity;
    use super::Model;
    use crate::auth::AuthPrincipal;
    use crate::auth::PrincipalSource;
    use crate::resource::AccessPolicy;
    use crate::resource::HookError;
    use crate::resource::MiryadResource;
    use ::lymaille::AgentDefinition;
    use ::lymaille::AgentKind;
    use chrono::TimeZone as _;
    use chrono::Utc;
    use sea_orm::ActiveValue::NotSet;
    use sea_orm::ActiveValue::Set;
    use serde_json::json;

    // ── Fixtures — purement mémoire : les hooks de ./agent.sdd sont des fonctions pures sur
    // l'`ActiveModel` (position contractuelle après `rest::core::mark_all_set`, avant toute
    // écriture, aucune connexion).

    fn principal() -> AuthPrincipal {
        AuthPrincipal {
            subject: "sujet-de-test".to_string(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::ApiToken { token_id: 1 },
        }
    }

    /// `AgentDefinition` sérialisée — la colonne `definition` est construite par la bibliothèque
    /// elle-même, jamais à la main, pour que le test prouve l'aller-retour de sa forme réelle.
    fn definition(id: &str, kind: AgentKind, prompt: &str, profile: &str) -> serde_json::Value {
        serde_json::to_value(
            AgentDefinition::new(id.to_string(), kind, prompt.to_string()).with_profile(profile.to_string()),
        )
        .expect("la fixture se sérialise")
    }

    /// `AgentDefinition` **sans profil explicite** (`lymaille` 0.3.0 : `new` sans `with_profile`),
    /// sérialisée par la bibliothèque elle-même — fixture du Scenario « une définition sans profil
    /// donne une colonne profile vide » (issue #75).
    fn definition_sans_profil(id: &str, kind: AgentKind, prompt: &str) -> serde_json::Value {
        serde_json::to_value(AgentDefinition::new(id.to_string(), kind, prompt.to_string()))
            .expect("la fixture se sérialise")
    }

    /// `ActiveModel` dans la forme qui atteint le hook : colonnes dérivées fausses (le hook doit
    /// les réécrire), `definition` posée, clé et horodatage non posés, provenance non posée.
    fn active_avec(definition: serde_json::Value) -> ActiveModel {
        ActiveModel {
            id: NotSet,
            agent_id: Set("faux-agent".to_string()),
            kind: Set("faux-kind".to_string()),
            profile: Set("faux-profil".to_string()),
            definition: Set(definition),
            created_at: NotSet,
            managed_by: NotSet,
            managed_version: NotSet,
            managed_digest: NotSet,
        }
    }

    /// Ligne existante des hooks `before_update`/`before_delete`, non gée par défaut.
    fn modele(agent_id: &str) -> Model {
        modele_geree(agent_id, None, None, None)
    }

    /// Ligne existante avec provenance : les hooks de la tâche « Issue #73 » doivent la préserver
    /// à la mise à jour.
    fn modele_geree(
        agent_id: &str,
        managed_by: Option<&str>,
        managed_version: Option<&str>,
        managed_digest: Option<&str>,
    ) -> Model {
        Model {
            id: 1,
            agent_id: agent_id.to_string(),
            kind: "sub".to_string(),
            profile: "qwen".to_string(),
            definition: definition(agent_id, AgentKind::Sub, "prompt d'origine", "qwen"),
            created_at: Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap(),
            managed_by: managed_by.map(str::to_owned),
            managed_version: managed_version.map(str::to_owned),
            managed_digest: managed_digest.map(str::to_owned),
        }
    }

    /// Scenario « une définition valide du catalogue est acceptée et ses colonnes dérivées » :
    /// les colonnes fausses de l'entrée sont réécrites depuis la définition, et l'horodatage de
    /// création est posé (`created_at` était `NotSet`).
    #[tokio::test]
    async fn before_create_accepte_une_definition_valide_et_derive_les_colonnes() {
        let rendu = Entity::before_create(
            &hook_db(),
            active_avec(definition("G_triage", AgentKind::Sub, "trie", "qwen")),
            &principal(),
        )
        .await
        .expect("une définition `G_triage` valide du catalogue est acceptée");
        assert_eq!(
            rendu.agent_id,
            Set("G_triage".to_string()),
            "`agent_id` est la copie dérivée de `definition.id`"
        );
        assert_eq!(
            rendu.kind,
            Set("sub".to_string()),
            "`kind` est la copie dérivée de la nature de la définition"
        );
        assert_eq!(
            rendu.profile,
            Set("qwen".to_string()),
            "`profile` est la copie dérivée du profil désigné"
        );
        assert!(
            matches!(rendu.created_at, Set(_)),
            "`created_at` est posé par le hook de création quand il est absent"
        );
    }

    /// Scenario « une définition sans profil donne une colonne profile vide » (issue #75,
    /// amendement du 2026-11-13 de ./agent.sdd) : une définition sans profil explicite, avec une
    /// colonne `profile` fausse dans l'`ActiveModel`, rend `profile = ""` (« suit le profil par
    /// défaut de l'hôte ») et la définition JSON traverse inchangée. La moitié « avec profil » du
    /// Scenario est verrouillée par
    /// `before_create_accepte_une_definition_valide_et_derive_les_colonnes` (`profile = "qwen"`,
    /// copie inchangée).
    #[tokio::test]
    async fn une_definition_sans_profil_donne_une_colonne_profile_vide() {
        let brute = definition_sans_profil("G_triage", AgentKind::Sub, "trie");
        let rendu = Entity::before_create(&hook_db(), active_avec(brute.clone()), &principal())
            .await
            .expect("une définition du catalogue sans profil est acceptée");
        assert_eq!(
            rendu.agent_id,
            Set("G_triage".to_string()),
            "`agent_id` reste la copie dérivée de `definition.id`"
        );
        assert_eq!(
            rendu.profile,
            Set(String::new()),
            "sans profil explicite, la colonne dérivée est la chaîne vide (suit le défaut de l'hôte)"
        );
        assert_eq!(
            rendu.definition,
            Set(brute),
            "la colonne JSON `definition`, source de vérité, n'est pas modifiée"
        );
    }

    /// Scenario « un identifiant privé ou en casse réservée est refusé » : `triage` (privé) puis
    /// `g_triage` (casse réservée) sont rejetés par le même message, sans code `MRD-*`.
    #[tokio::test]
    async fn before_create_refuse_identifiant_prive_puis_casse_reservee() {
        for id in ["triage", "g_triage"] {
            let rendu = Entity::before_create(
                &hook_db(),
                active_avec(definition(id, AgentKind::Sub, "trie", "qwen")),
                &principal(),
            )
            .await;
            match rendu {
                Err(erreur) => {
                    assert!(
                        erreur.code.is_none(),
                        "un `HookError` de la crate ne porte aucun code : {erreur:?}"
                    );
                    assert_eq!(
                        erreur.message, "agent id must start with G_ (catalog)",
                        "l'identifiant {id} n'est pas du catalogue"
                    );
                }
                Ok(_) => panic!("la définition d'id {id} devait être refusée"),
            }
        }
    }

    /// Scenario « une définition invalide est refusée avec la cause de lymaille » : le message
    /// commence par la cause et ne cite jamais le prompt (ni aucun secret, contrat transverse).
    #[tokio::test]
    async fn before_create_refuse_une_definition_invalide_sans_citer_le_prompt() {
        let prompt = "prompt-a-ne-jamais-citer-9f3c";
        let refusee = AgentDefinition::new("G_triage".to_string(), AgentKind::Sub, prompt.to_string())
            .with_profile("qwen".to_string())
            .with_delegable(vec!["G_sous".to_string()]);
        // `validate` refuse : un sous-agent ne peut déclarer de sous-agent délégable.
        assert!(
            refusee.validate().is_err(),
            "précondition : la fixture est bien refusée par `AgentDefinition::validate`"
        );
        let valeur = serde_json::to_value(refusee).expect("la fixture se sérialise");
        match Entity::before_create(&hook_db(), active_avec(valeur), &principal()).await {
            Err(erreur) => {
                assert!(
                    erreur.message.starts_with("invalid agent definition:"),
                    "la cause de lymaille est citée : {erreur:?}"
                );
                assert!(
                    !erreur.message.contains(prompt),
                    "le prompt ne traverse jamais un `HookError` : {erreur:?}"
                );
            }
            Ok(_) => panic!("la définition refusée par `validate` devait être rejetée"),
        }
    }

    /// `Must` étape 1 : `definition` absente ou non objet est refusée avant toute désérialisation.
    #[tokio::test]
    async fn before_create_refuse_une_definition_absente_ou_non_objet() {
        let sans = ActiveModel {
            definition: NotSet,
            ..active_avec(serde_json::Value::Null)
        };
        match Entity::before_create(&hook_db(), sans, &principal()).await {
            Err(erreur) => assert_eq!(erreur.message, "agent definition is required"),
            Ok(_) => panic!("une `definition` `NotSet` doit être refusée"),
        }
        match Entity::before_create(
            &hook_db(),
            active_avec(json!(["pas", "un", "objet"])),
            &principal(),
        )
        .await
        {
            Err(erreur) => assert_eq!(erreur.message, "agent definition is required"),
            Ok(_) => panic!("une `definition` non objet doit être refusée"),
        }
    }

    /// Scenario « renommer un agent est refusé » : la nouvelle définition d'id `G_other` ne doit
    /// pas être acceptée sur la ligne `G_triage`.
    #[tokio::test]
    async fn before_update_refuse_un_renommage() {
        let rendu = Entity::before_update(
            &hook_db(),
            active_avec(definition("G_other", AgentKind::Sub, "trie", "qwen")),
            &modele("G_triage"),
            &principal(),
        )
        .await;
        match rendu {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "aucun code `MRD-*` sur un `HookError`");
                assert_eq!(erreur.message, "agent id cannot change");
            }
            Ok(_) => panic!("renommer un agent change son identité : la mise à jour est refusée"),
        }
    }

    /// `Must` (`before_update`, étapes 1 à 5) : une mise à jour qui garde l'id dérive les
    /// colonnes et laisse `created_at` à celui de la ligne existante.
    #[tokio::test]
    async fn before_update_derive_les_colonnes_et_conserve_la_date_de_creation() {
        let existing = modele("G_triage");
        let rendu = Entity::before_update(
            &hook_db(),
            active_avec(definition("G_triage", AgentKind::Sub, "trie affiné", "qwen")),
            &existing,
            &principal(),
        )
        .await
        .expect("une mise à jour qui garde l'id est acceptée");
        assert_eq!(rendu.agent_id, Set("G_triage".to_string()));
        assert_eq!(rendu.kind, Set("sub".to_string()));
        assert_eq!(rendu.profile, Set("qwen".to_string()));
        assert_eq!(
            rendu.created_at,
            Set(existing.created_at),
            "`created_at` reste celui de la ligne existante"
        );
    }

    /// Scenario « la provenance n'est pas écrite par le CRUD » (amendement du 2026-11-13, issue
    /// #73) : à la création, les trois colonnes posées par le corps de la requête sont ignorées —
    /// le hook les rend `None` quelle que soit la valeur reçue ; à la mise à jour, elles sont
    /// remplacées par celles de la ligne relue, y compris quand le corps en porte d'autres, et le
    /// contenu modifié passe (les colonnes dérivées suivent la nouvelle définition).
    #[tokio::test]
    async fn la_provenance_n_est_pas_ecrite_par_le_crud() {
        let forgee = ActiveModel {
            managed_by: Set(Some("vanyline".to_string())),
            managed_version: Set(Some("9.9.9".to_string())),
            managed_digest: Set(Some("a3b5c7".to_string())),
            ..active_avec(definition("G_triage", AgentKind::Sub, "trie", "qwen"))
        };
        let cree = Entity::before_create(&hook_db(), forgee, &principal())
            .await
            .expect("une définition valide du catalogue reste acceptée, provenance posée ou non");
        for (colonne, valeur) in [
            ("managed_by", &cree.managed_by),
            ("managed_version", &cree.managed_version),
            ("managed_digest", &cree.managed_digest),
        ] {
            assert!(
                matches!(valeur, sea_orm::ActiveValue::Unchanged(None) | NotSet)
                    || matches!(valeur, Set(None)),
                "`{colonne}` posée par le corps est ignorée à la création : {valeur:?}"
            );
        }

        let existing = modele_geree("G_triage", Some("vanyline"), Some("1.0.0"), Some("abc123"));
        let corps_d_other = ActiveModel {
            managed_by: Set(Some("autre-source".to_string())),
            managed_version: Set(Some("2.0.0".to_string())),
            managed_digest: Set(Some("000000".to_string())),
            ..active_avec(definition("G_triage", AgentKind::Sub, "trie affiné", "qwen"))
        };
        let mis_a_jour = Entity::before_update(&hook_db(), corps_d_other, &existing, &principal())
            .await
            .expect("une mise à jour qui garde l'id est acceptée, provenance posée ou non");
        assert_eq!(
            mis_a_jour.managed_by,
            Set(Some("vanyline".to_string())),
            "`managed_by` reste celui de la ligne relue"
        );
        assert_eq!(
            mis_a_jour.managed_version,
            Set(Some("1.0.0".to_string())),
            "`managed_version` reste celle de la ligne relue"
        );
        assert_eq!(
            mis_a_jour.managed_digest,
            Set(Some("abc123".to_string())),
            "`managed_digest` reste celle de la ligne relue"
        );
        assert_eq!(
            mis_a_jour.kind,
            Set("sub".to_string()),
            "le contenu modifié passe : les colonnes dérivées suivent la nouvelle définition"
        );
    }

    /// Scenario « politiques du catalogue » : les métadonnées du `Must`, exactes.
    #[test]
    fn politiques_du_catalogue() {
        assert_eq!(Entity::resource_name(), "lymaille_catalog_agents");
        assert_eq!(Entity::read_policy(), AccessPolicy::Public);
        assert_eq!(Entity::write_policy(), AccessPolicy::AdminOnly);
        assert!(
            Entity::owner_column().is_none(),
            "le catalogue n'a pas de propriétaire"
        );
        assert!(matches!(Entity::filter_column(), Some(Column::Kind)));
        assert!(matches!(Entity::label_column(), Some(Column::AgentId)));
    }

    /// `Must not` / contrat transverse : `before_delete` n'est pas surdéclaré (aucun contrôle
    /// d'usage au lot 1), le défaut du trait s'applique.
    #[tokio::test]
    async fn before_delete_defaut_du_trait() {
        let resultat: Result<(), HookError> =
            Entity::before_delete(&hook_db(), &modele("G_triage"), &principal()).await;
        assert!(resultat.is_ok(), "le défaut `Ok(())` s'applique : {resultat:?}");
    }

    /// Scenario « `resource_openapi` compile et décrit la ressource » (issue #79, amendement du
    /// 2026-11-13 de ./agent.sdd) : l'appel compile (le `Model` dérive `utoipa::ToSchema`), le
    /// schéma porte toutes les colonnes, `created_at` est un `string`/`date-time` et `definition`
    /// un objet libre sans propriétés (les types de `lymaille` n'ont pas encore de schéma).
    #[test]
    fn resource_openapi_compile_et_decrit_ressource() {
        let fragment = crate::rest::openapi::resource_openapi::<Entity>();
        let json = serde_json::to_value(&fragment).expect("le fragment OpenAPI se sérialise");
        let schema = json
            .get("components")
            .and_then(|c| c.get("schemas"))
            .and_then(|s| s.get("Model"))
            .unwrap_or_else(|| panic!("le schéma du `Model` figure dans les composants : {json}"));
        let proprietes = schema["properties"]
            .as_object()
            .expect("le schéma du `Model` porte ses propriétés");
        for colonne in [
            "id",
            "agent_id",
            "kind",
            "profile",
            "definition",
            "created_at",
            "managed_by",
            "managed_version",
            "managed_digest",
        ] {
            assert!(
                proprietes.contains_key(colonne),
                "colonne {colonne} absente du schéma : {schema}"
            );
        }
        assert_eq!(
            proprietes["created_at"]["type"], "string",
            "`created_at` (`DateTimeUtc`) est un `string`"
        );
        assert_eq!(
            proprietes["created_at"]["format"], "date-time",
            "`created_at` (`DateTimeUtc`) est au format RFC 3339 (`date-time`)"
        );
        assert_eq!(
            proprietes["definition"]["type"], "object",
            "`definition` est décrite en objet libre"
        );
        assert!(
            proprietes["definition"].get("properties").is_none(),
            "l'objet libre de `definition` ne détaille aucune propriété : {}",
            proprietes["definition"]
        );
    }

    /// Scenario « l'export TypeScript du Model correspond à la sérialisation » (issue #72,
    /// `./agent.sdd`) : la déclaration `agent.ts` écrite par `export_typescript` de
    /// `../interactive/event.sdd` porte exactement les colonnes du `Model` — chaque clé du JSON
    /// du `Model` sérialisé est une propriété, et aucune propriété n'est en trop — avec `number`
    /// pour les entiers, `string` pour la date et la colonne JSON typée par le type de
    /// `lymaille` correspondant. La sérialisation du `Model` n'a pas changé (mêmes clés, mêmes
    /// valeurs : le JSON est celui des colonnes, vérifié par l'égalité des ensembles).
    #[test]
    fn lexport_typescript_du_modele_correspond_a_la_serialisation() {
        let dir = crate::lymaille::interactive::event::tests::scratch_dir("export-agent");
        let rendu = crate::lymaille::interactive::event::export_typescript(&dir)
            .expect("l'export TypeScript réussit");
        let chemin = dir.join("agent.ts");
        assert!(rendu.contains(&chemin), "`agent.ts` est dans les chemins rendus");
        let texte = std::fs::read_to_string(&chemin).expect("`agent.ts` s'écrit et se relit");
        assert!(
            texte.contains("export type AgentModel = {"),
            "la déclaration porte le nom distinct `AgentModel`, pas le simple `Model` : {texte}"
        );
        let modele = modele("G_triage");
        let json = serde_json::to_value(&modele).expect("le Model sérialise");
        let champs = crate::lymaille::interactive::event::tests::decl_fields(&texte);
        let noms: std::collections::BTreeSet<&str> = champs.iter().map(|(nom, ..)| nom.as_str()).collect();
        let cles: std::collections::BTreeSet<&str> = json
            .as_object()
            .expect("le Model sérialise en objet")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            noms, cles,
            "les propriétés de la déclaration sont exactement les clés du JSON sérialisé"
        );
        let type_de = |nom: &str| -> &str {
            champs
                .iter()
                .find(|(champ, ..)| champ == nom)
                .map_or_else(|| panic!("la colonne {nom} est déclarée"), |(.., ty)| ty.as_str())
        };
        assert_eq!(type_de("id"), "number", "l'entier `id` est un number");
        assert_eq!(
            type_de("created_at"),
            "string",
            "la date `created_at` (`DateTimeUtc`) est une string RFC 3339"
        );
        assert_eq!(
            type_de("definition"),
            "AgentDefinition",
            "colonne definition typée AgentDefinition dans la déclaration"
        );
        assert_eq!(
            type_de("managed_by"),
            "string | null",
            "colonne managed_by typée string | null dans la déclaration (issue #73)"
        );
        std::fs::remove_dir_all(&dir).expect("le répertoire de test se nettoie");
    }
}
