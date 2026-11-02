//! Entité `lymaille_profiles` (feature `lymaille`) — contrat porté par `./profile.sdd`.
//!
//! Table `miryad_lymaille_profiles` : les profils de modèle, configuration globale administrée
//! par l'admin — un profil nomme un provider et un modèle (une `lymaille::ModelProfile`), et une
//! définition d'agent le désigne par son nom. Ni propriétaire ni préfixe `G_` (DSD `DEC-14`).

use sea_orm::ActiveValue;
use sea_orm::entity::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::auth::AuthPrincipal;
use crate::resource::AccessPolicy;
use crate::resource::HookError;
use crate::resource::MiryadResource;

/// Profil de modèle persisté — la table `miryad_lymaille_profiles` posée par `../migration.rs`,
/// colonnes du contrat `Must` de ./profile.sdd.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "miryad_lymaille_profiles")]
pub struct Model {
    /// Clé primaire entière auto-incrémentée (convention `miryad_*` de la crate).
    #[sea_orm(primary_key)]
    pub id: i32,
    /// Nom du profil, copie dérivée de `profile.name` par les hooks — libellé de la ressource,
    /// unique (contrainte de la migration).
    pub name: String,
    /// L'`lymaille::ModelProfile` complète, colonne JSON : la source de vérité, désérialisée et
    /// validée par les hooks.
    pub profile: serde_json::Value,
    /// Horodatage de création, posé par le hook de création.
    pub created_at: DateTimeUtc,
}

/// Aucune relation : la référence au provider est un nom, résolu à l'exécution, sans clé
/// étrangère.
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Entity {
    /// Porte commune de `before_create` et `before_update`, dans l'ordre du `Must` de
    /// ./profile.sdd : `profile` est un objet JSON, se désérialise en `lymaille::ModelProfile`
    /// (la cause traverse), porte un nom non vide, et ce nom ne change pas sur une mise à jour.
    /// `name` est toujours la copie dérivée du profil. L'existence du provider nommé n'est pas
    /// vérifiée en base : résolue à l'exécution (`Must not`).
    fn valider_et_deriver(
        mut active: ActiveModel,
        existing: Option<&Model>,
    ) -> Result<ActiveModel, HookError> {
        let brute = match &active.profile {
            ActiveValue::Set(valeur) | ActiveValue::Unchanged(valeur) if valeur.is_object() => Some(valeur),
            ActiveValue::Set(_) | ActiveValue::Unchanged(_) | ActiveValue::NotSet => None,
        };
        let Some(brute) = brute else {
            return Err(HookError::new("profile is required"));
        };
        let profil: ::lymaille::ModelProfile = serde_json::from_value(brute.clone())
            .map_err(|cause| HookError::new(format!("invalid profile: {cause}")))?;
        if profil.name().trim().is_empty() {
            return Err(HookError::new("invalid profile: the name is empty"));
        }
        if let Some(ligne) = existing
            && profil.name() != ligne.name.as_str()
        {
            return Err(HookError::new("profile name cannot change"));
        }
        active.name = ActiveValue::Set(profil.name().to_owned());
        if existing.is_none() && matches!(active.created_at, ActiveValue::NotSet) {
            active.created_at = ActiveValue::Set(chrono::Utc::now());
        }
        Ok(active)
    }
}

/// Métadonnées et hooks de la ressource `lymaille_profiles`, lue à l'identique par REST et MCP :
/// lecture par tout authentifié, écriture par l'admin seul, sans propriétaire.
impl MiryadResource for Entity {
    fn resource_name() -> &'static str {
        "lymaille_profiles"
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
        None
    }

    fn label_column() -> Option<<Self as EntityTrait>::Column> {
        Some(Column::Name)
    }

    /// Valide le profil et dérive `name` — voir [`Entity::valider_et_deriver`].
    fn before_create(
        active: Self::ActiveModel,
        _principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        Entity::valider_et_deriver(active, None)
    }

    /// Mêmes étapes que [`Self::before_create`], plus le nom : renommer un profil est refusé.
    fn before_update(
        active: Self::ActiveModel,
        existing: &Self::Model,
        _principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        Entity::valider_et_deriver(active, Some(existing))
    }

    // `before_delete` volontairement non surdéclaré : le défaut identité du trait s'applique
    // (./profile.sdd).
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
    use crate::resource::MiryadResource;
    use ::lymaille::ModelProfile;
    use chrono::TimeZone as _;
    use sea_orm::ActiveValue::NotSet;
    use sea_orm::ActiveValue::Set;
    use serde_json::json;

    // ── Fixtures — purement mémoire : hooks purs, aucune connexion.

    fn principal() -> AuthPrincipal {
        AuthPrincipal {
            subject: "sujet-de-test".to_string(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::ApiToken { token_id: 1 },
        }
    }

    fn profil_json(nom: &str, fournisseur: &str, modele: &str) -> serde_json::Value {
        serde_json::to_value(ModelProfile::new(
            nom.to_string(),
            fournisseur.to_string(),
            modele.to_string(),
        ))
        .expect("la fixture se sérialise")
    }

    /// `ActiveModel` dans la forme qui atteint le hook : `name` faux (le hook doit le réécrire).
    fn active_avec(profil: serde_json::Value) -> ActiveModel {
        ActiveModel {
            id: NotSet,
            name: Set("faux-nom".to_string()),
            profile: Set(profil),
            created_at: NotSet,
        }
    }

    fn modele(nom: &str) -> Model {
        Model {
            id: 1,
            name: nom.to_string(),
            profile: profil_json(nom, "local", "qwen3"),
            created_at: chrono::Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap(),
        }
    }

    /// Scenario « un profil valide dérive son nom » : le `name` faux de l'entrée est réécrit
    /// depuis le profil, l'horodatage de création est posé.
    #[test]
    fn before_create_accepte_un_profil_valide_et_derive_son_nom() {
        let rendu = Entity::before_create(active_avec(profil_json("qwen", "local", "qwen3")), &principal())
            .expect("un profil valide est accepté");
        assert_eq!(rendu.name, Set("qwen".to_string()), "`name` est dérivé du profil");
        assert!(
            matches!(rendu.created_at, Set(_)),
            "`created_at` est posé par le hook de création"
        );
    }

    /// Scenario « un profil invalide est refusé » : sans `provider`, la désérialisation échoue et
    /// sa cause est citée.
    #[test]
    fn before_create_refuse_un_profil_invalide() {
        let sans_fournisseur = json!({ "name": "x", "model": "qwen3" });
        match Entity::before_create(active_avec(sans_fournisseur), &principal()) {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "aucun code sur un `HookError` de la crate");
                assert!(
                    erreur.message.starts_with("invalid profile:"),
                    "la cause de la désérialisation est citée : {erreur:?}"
                );
            }
            Ok(_) => panic!("un profil sans fournisseur devait être refusé"),
        }
    }

    /// `Must` : `profile` absent (`NotSet`) ou non objet est refusé avant toute désérialisation.
    #[test]
    fn before_create_refuse_un_profil_absent_ou_non_objet() {
        let sans = ActiveModel {
            profile: NotSet,
            ..active_avec(serde_json::Value::Null)
        };
        match Entity::before_create(sans, &principal()) {
            Err(erreur) => assert_eq!(erreur.message, "profile is required"),
            Ok(_) => panic!("un profil `NotSet` doit être refusé"),
        }
        match Entity::before_create(active_avec(json!("qwen")), &principal()) {
            Err(erreur) => assert_eq!(erreur.message, "profile is required"),
            Ok(_) => panic!("un profil non objet doit être refusé"),
        }
    }

    /// `Must` : un profil au nom vide est refusé (le nom dérivé serait inutilisable).
    #[test]
    fn before_create_refuse_un_profil_sans_nom() {
        let sans_nom = json!({ "name": " ", "provider": "local", "model": "qwen3" });
        match Entity::before_create(active_avec(sans_nom), &principal()) {
            Err(erreur) => assert!(
                erreur.message.starts_with("invalid profile:"),
                "le refus cite sa cause : {erreur:?}"
            ),
            Ok(_) => panic!("un profil au nom vide ne peut être nommé"),
        }
    }

    /// Scenario « renommer est refusé » : le nouveau profil nommé `autre` ne doit pas être
    /// accepté sur la ligne `qwen`.
    #[test]
    fn before_update_refuse_un_renommage() {
        let rendu = Entity::before_update(
            active_avec(profil_json("autre", "local", "qwen3")),
            &modele("qwen"),
            &principal(),
        );
        match rendu {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "aucun code sur un `HookError` de la crate");
                assert_eq!(erreur.message, "profile name cannot change");
            }
            Ok(_) => panic!("renommer un profil est refusé"),
        }
    }

    /// Scenario « politiques » : les métadonnées du `Must`, exactes.
    #[test]
    fn politiques() {
        assert_eq!(Entity::resource_name(), "lymaille_profiles");
        assert_eq!(Entity::read_policy(), AccessPolicy::Public);
        assert_eq!(Entity::write_policy(), AccessPolicy::AdminOnly);
        assert!(Entity::owner_column().is_none(), "aucun propriétaire");
        assert!(Entity::filter_column().is_none());
        assert!(matches!(Entity::label_column(), Some(Column::Name)));
    }
}
