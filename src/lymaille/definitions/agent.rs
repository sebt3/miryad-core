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
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
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
    pub definition: serde_json::Value,
    /// Horodatage de création, posé par le hook de création (aucune entité de la crate ne porte
    /// d'`updated_at`).
    pub created_at: DateTimeUtc,
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
    fn valider_et_deriver(
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
        active.profile = ActiveValue::Set(definition.profile().to_owned());
        match existing {
            Some(ligne) => active.created_at = ActiveValue::Set(ligne.created_at),
            None if matches!(active.created_at, ActiveValue::NotSet) => {
                active.created_at = ActiveValue::Set(chrono::Utc::now());
            }
            None => {}
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

    /// Valide la définition et réécrit les colonnes dérivées — voir [`Entity::valider_et_deriver`].
    fn before_create(
        active: Self::ActiveModel,
        _principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        Entity::valider_et_deriver(active, None)
    }

    /// Mêmes étapes que [`Self::before_create`] sur la nouvelle définition, plus l'identité :
    /// renommer un agent change son identité et est refusé ; `created_at` reste celui de la
    /// ligne existante.
    fn before_update(
        active: Self::ActiveModel,
        existing: &Self::Model,
        _principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        Entity::valider_et_deriver(active, Some(existing))
    }

    // `before_delete` volontairement non surdéclaré : aucun contrôle d'usage au lot 1
    // (./agent.sdd `Must`).
}

#[cfg(test)]
mod tests {
    use super::ActiveModel;
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
        serde_json::to_value(AgentDefinition::new(
            id.to_string(),
            kind,
            prompt.to_string(),
            profile.to_string(),
        ))
        .expect("la fixture se sérialise")
    }

    /// `ActiveModel` dans la forme qui atteint le hook : colonnes dérivées fausses (le hook doit
    /// les réécrire), `definition` posée, clé et horodatage non posés.
    fn active_avec(definition: serde_json::Value) -> ActiveModel {
        ActiveModel {
            id: NotSet,
            agent_id: Set("faux-agent".to_string()),
            kind: Set("faux-kind".to_string()),
            profile: Set("faux-profil".to_string()),
            definition: Set(definition),
            created_at: NotSet,
        }
    }

    /// Ligne existante des hooks `before_update`/`before_delete`.
    fn modele(agent_id: &str) -> Model {
        Model {
            id: 1,
            agent_id: agent_id.to_string(),
            kind: "sub".to_string(),
            profile: "qwen".to_string(),
            definition: definition(agent_id, AgentKind::Sub, "prompt d'origine", "qwen"),
            created_at: Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap(),
        }
    }

    /// Scenario « une définition valide du catalogue est acceptée et ses colonnes dérivées » :
    /// les colonnes fausses de l'entrée sont réécrites depuis la définition, et l'horodatage de
    /// création est posé (`created_at` était `NotSet`).
    #[test]
    fn before_create_accepte_une_definition_valide_et_derive_les_colonnes() {
        let rendu = Entity::before_create(
            active_avec(definition("G_triage", AgentKind::Sub, "trie", "qwen")),
            &principal(),
        )
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

    /// Scenario « un identifiant privé ou en casse réservée est refusé » : `triage` (privé) puis
    /// `g_triage` (casse réservée) sont rejetés par le même message, sans code `MRD-*`.
    #[test]
    fn before_create_refuse_identifiant_prive_puis_casse_reservee() {
        for id in ["triage", "g_triage"] {
            let rendu = Entity::before_create(
                active_avec(definition(id, AgentKind::Sub, "trie", "qwen")),
                &principal(),
            );
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
    #[test]
    fn before_create_refuse_une_definition_invalide_sans_citer_le_prompt() {
        let prompt = "prompt-a-ne-jamais-citer-9f3c";
        let refusee = AgentDefinition::new(
            "G_triage".to_string(),
            AgentKind::Sub,
            prompt.to_string(),
            "qwen".to_string(),
        )
        .with_delegable(vec!["G_sous".to_string()]);
        // `validate` refuse : un sous-agent ne peut déclarer de sous-agent délégable.
        assert!(
            refusee.validate().is_err(),
            "précondition : la fixture est bien refusée par `AgentDefinition::validate`"
        );
        let valeur = serde_json::to_value(refusee).expect("la fixture se sérialise");
        match Entity::before_create(active_avec(valeur), &principal()) {
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
    #[test]
    fn before_create_refuse_une_definition_absente_ou_non_objet() {
        let sans = ActiveModel {
            definition: NotSet,
            ..active_avec(serde_json::Value::Null)
        };
        match Entity::before_create(sans, &principal()) {
            Err(erreur) => assert_eq!(erreur.message, "agent definition is required"),
            Ok(_) => panic!("une `definition` `NotSet` doit être refusée"),
        }
        match Entity::before_create(active_avec(json!(["pas", "un", "objet"])), &principal()) {
            Err(erreur) => assert_eq!(erreur.message, "agent definition is required"),
            Ok(_) => panic!("une `definition` non objet doit être refusée"),
        }
    }

    /// Scenario « renommer un agent est refusé » : la nouvelle définition d'id `G_other` ne doit
    /// pas être acceptée sur la ligne `G_triage`.
    #[test]
    fn before_update_refuse_un_renommage() {
        let rendu = Entity::before_update(
            active_avec(definition("G_other", AgentKind::Sub, "trie", "qwen")),
            &modele("G_triage"),
            &principal(),
        );
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
    #[test]
    fn before_update_derive_les_colonnes_et_conserve_la_date_de_creation() {
        let existing = modele("G_triage");
        let rendu = Entity::before_update(
            active_avec(definition("G_triage", AgentKind::Sub, "trie affiné", "qwen")),
            &existing,
            &principal(),
        )
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
    #[test]
    fn before_delete_defaut_du_trait() {
        let resultat: Result<(), HookError> = Entity::before_delete(&modele("G_triage"), &principal());
        assert!(resultat.is_ok(), "le défaut `Ok(())` s'applique : {resultat:?}");
    }
}
