//! Entité `lymaille_catalog_toolsets` (feature `lymaille`) — contrat porté par `./toolset.sdd`.
//!
//! Table `miryad_lymaille_catalog_toolsets` : chaque ligne porte la version **active** et le
//! **brouillon** d'un toolset (DSD `DEC-18`). Au lot 1 l'admin écrit `active` et `draft`
//! directement par le CRUD REST et MCP ; l'activation d'un brouillon est l'écriture de `active`
//! par un admin, et le store ne lit jamais `draft` pour son version active. Une version est une
//! `lymaille::Toolset` sérialisée ; les hooks la valident ([`check_toolset`], contrat partagé
//! avec `../store/toolset.rs`) et en dérivent `toolset_id` (le `name`, préfixé `G_` au catalogue,
//! DSD `DEC-9`) et `version`.

use sea_orm::ActiveValue;
use sea_orm::entity::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::auth::AuthPrincipal;
use crate::lymaille::identity;
use crate::resource::AccessPolicy;
use crate::resource::HookError;
use crate::resource::MiryadResource;

/// Plafond de rendu d'un corps ou d'une annexe de skill, en caractères — la valeur du plafond
/// par défaut de `lymaille::ScratchpadLimits::new()` (source lymaille 0.1.0,
/// `scratchpad/limits.rs` : `DEFAULT_RENDER_CAP_CHARS = 8_000`), relu à l'implémentation de
/// ./toolset.sdd.
pub const DEFAULT_RENDER_CAP_CHARS: u64 = 8_000;

/// Toolset du catalogue persisté — la table `miryad_lymaille_catalog_toolsets` posée par
/// `../migration.rs`, colonnes du contrat `Must` de ./toolset.sdd.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "miryad_lymaille_catalog_toolsets")]
pub struct Model {
    /// Clé primaire entière auto-incrémentée (convention `miryad_*` de la crate).
    #[sea_orm(primary_key)]
    pub id: i32,
    /// Identifiant du toolset (`G_…`), copie dérivée du `name` de ses versions par les hooks —
    /// libellé de la ressource, unique au catalogue (contrainte de la migration).
    pub toolset_id: String,
    /// Version sémantique de la version active (à défaut du brouillon), copie dérivée par les
    /// hooks.
    pub version: String,
    /// La `lymaille::Toolset` active, colonne JSON nullable — `None` tant qu'aucune version
    /// n'est activée (DSD `DEC-18`).
    pub active: Option<serde_json::Value>,
    /// La `lymaille::Toolset` en brouillon, colonne JSON nullable — jamais lue par le store
    /// comme version active (DSD `DEC-18`).
    pub draft: Option<serde_json::Value>,
    /// Horodatage de création, posé par le hook de création.
    pub created_at: DateTimeUtc,
}

/// Aucune relation : les versions vivent dans la même ligne, la référence entre toolsets est un
/// nom, sans clé étrangère.
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Entity {
    /// Version réellement présente d'une colonne `Option<Json>` : posée (ou inchangée), non
    /// `None`, non `null` JSON — `Must` étape 1 de ./toolset.sdd.
    fn version_presents(valeur: &ActiveValue<Option<serde_json::Value>>) -> Option<&serde_json::Value> {
        match valeur {
            ActiveValue::Set(alhee) | ActiveValue::Unchanged(alhee) => {
                alhee.as_ref().filter(|version| !version.is_null())
            }
            ActiveValue::NotSet => None,
        }
    }

    /// Étape 2 du `Must` : la version se désérialise en `lymaille::Toolset` et passe les
    /// contrôles partagés [`check_toolset`] ; l'échec cite la cause, que la bibliothèque n'établit
    /// jamais depuis un fragment de source (`Must not`).
    fn charger(brute: &serde_json::Value) -> Result<::lymaille::Toolset, HookError> {
        let toolset: ::lymaille::Toolset = serde_json::from_value(brute.clone())
            .map_err(|cause| HookError::new(format!("invalid toolset: {cause}")))?;
        check_toolset(&toolset).map_err(|cause| HookError::new(format!("invalid toolset: {cause}")))?;
        Ok(toolset)
    }

    /// Porte commune de `before_create` et `before_update`, dans l'ordre du `Must` de
    /// ./toolset.sdd : au moins une version, chacune valide, les deux présentes portent le même
    /// `name`, cet `name` est du catalogue (`G_` exact, DSD `DEC-9`), et sur une mise à jour il
    /// ne change pas. `toolset_id` et `version` (celle de l'active, à défaut du brouillon) sont
    /// toujours réécrits depuis les versions chargées.
    fn valider_et_deriver(
        mut active: ActiveModel,
        existing: Option<&Model>,
    ) -> Result<ActiveModel, HookError> {
        let brute_active = Self::version_presents(&active.active);
        let brute_draft = Self::version_presents(&active.draft);
        if brute_active.is_none() && brute_draft.is_none() {
            return Err(HookError::new("a toolset needs an active version or a draft"));
        }
        let active_chargee = brute_active.map(Self::charger).transpose()?;
        let draft_charge = brute_draft.map(Self::charger).transpose()?;
        if let (Some(version_active), Some(version_brouillon)) = (&active_chargee, &draft_charge)
            && version_active.name() != version_brouillon.name()
        {
            return Err(HookError::new(
                "the active and draft versions must carry the same name",
            ));
        }
        let reference = active_chargee
            .as_ref()
            .or(draft_charge.as_ref())
            .ok_or_else(|| HookError::new("a toolset needs an active version or a draft"))?;
        if !matches!(
            identity::classify(reference.name()),
            Ok(identity::IdKind::Catalog)
        ) {
            return Err(HookError::new("toolset id must start with G_ (catalog)"));
        }
        if let Some(ligne) = existing
            && reference.name() != ligne.toolset_id.as_str()
        {
            return Err(HookError::new("toolset id cannot change"));
        }
        active.toolset_id = ActiveValue::Set(reference.name().to_owned());
        active.version = ActiveValue::Set(reference.version().to_string());
        if existing.is_none() && matches!(active.created_at, ActiveValue::NotSet) {
            active.created_at = ActiveValue::Set(chrono::Utc::now());
        }
        Ok(active)
    }
}

/// Métadonnées et hooks de la ressource `lymaille_catalog_toolsets`, lue à l'identique par REST
/// et MCP. Catalogue (DSD `DEC-14`) : lecture par tout authentifié, écriture par l'admin seul.
impl MiryadResource for Entity {
    fn resource_name() -> &'static str {
        "lymaille_catalog_toolsets"
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
        Some(Column::ToolsetId)
    }

    /// Valide les versions présentes et dérive `toolset_id` et `version` — voir
    /// [`Entity::valider_et_deriver`].
    fn before_create(
        active: Self::ActiveModel,
        _principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        Entity::valider_et_deriver(active, None)
    }

    /// Mêmes étapes que [`Self::before_create`], plus l'identité : un `toolset_id` qui change est
    /// refusé (étape 5 du `Must`).
    fn before_update(
        active: Self::ActiveModel,
        existing: &Self::Model,
        _principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        Entity::valider_et_deriver(active, Some(existing))
    }

    // `before_delete` volontairement non surdéclaré : aucun contrôle d'usage au lot 1, le défaut
    // identité du trait s'applique (./toolset.sdd).
}

/// Les contrôles de la version d'un toolset, partagés avec `../store/toolset.rs` (`put_draft`
/// appelle **cette fonction**, ./toolset.sdd `Must`) : contrat extractible du module Rhai
/// (`lymaille::extract_contract`, qui compile sans jamais l'exécuter), gabarits de skill
/// compilables ([`lymaille::check_template`]) et plafond de rendu [`DEFAULT_RENDER_CAP_CHARS`]
/// ([`lymaille::Skill::check_size`]). L'`Err` est la variante typée de `lymaille` rendue par ces
/// fonctions amont — le store la propage, le hook [`Entity`] la convertit en `HookError` depuis
/// son affichage, que la bibliothèque n'établit jamais à partir d'un fragment de source.
pub(crate) fn check_toolset(toolset: &::lymaille::Toolset) -> ::lymaille::ToolsetsResult<()> {
    ::lymaille::extract_contract(toolset.source())?;
    for skill in toolset.skills().values() {
        skill.check_size(DEFAULT_RENDER_CAP_CHARS)?;
        ::lymaille::check_template(
            &format!("skill {} description", skill.name()),
            skill.description(),
        )?;
        ::lymaille::check_template(&format!("skill {} body", skill.name()), skill.body())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ActiveModel;
    use super::Column;
    use super::DEFAULT_RENDER_CAP_CHARS;
    use super::Entity;
    use super::Model;
    use super::check_toolset;
    use crate::auth::AuthPrincipal;
    use crate::auth::PrincipalSource;
    use crate::resource::AccessPolicy;
    use crate::resource::MiryadResource;
    use ::lymaille::Toolset;
    use chrono::TimeZone as _;
    use sea_orm::ActiveValue::NotSet;
    use sea_orm::ActiveValue::Set;
    use serde_json::json;
    use vynil_core::Semver;

    // ── Fixtures — purement mémoire : hooks purs sur l'`ActiveModel`, aucune connexion. Les
    // versions sont sérialisées depuis un `lymaille::Toolset` construit par la bibliothèque
    // elle-même, pour exercer la forme JSON réelle (la désérialisation exige tous les champs).

    /// Source Rhai de fixture : un module qui compile (le contrat est alors la fonction `ping`).
    const SOURCE_VALIDE: &str = "fn ping() { 1 }";

    fn principal() -> AuthPrincipal {
        AuthPrincipal {
            subject: "sujet-de-test".to_string(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::ApiToken { token_id: 1 },
        }
    }

    /// Version sérialisée d'un toolset nommé `name`, version sémantique `version`, module `SOURCE_VALIDE`.
    fn version_json(name: &str, version: &str) -> serde_json::Value {
        serde_json::to_value(toolset(name, version, SOURCE_VALIDE)).expect("la fixture se sérialise")
    }

    fn toolset(name: &str, version: &str, source: &str) -> Toolset {
        Toolset::new(
            name.to_string(),
            "toolset de test".to_string(),
            Semver::parse(version).expect("la fixture porte une version valide"),
            source.to_string(),
        )
        .expect("la fixture est un toolset de forme valide")
    }

    /// `ActiveModel` dans la forme qui atteint le hook : colonnes dérivées fausses, versions
    /// voulues, clé et horodatage non posés.
    fn active_avec(active: Option<serde_json::Value>, draft: Option<serde_json::Value>) -> ActiveModel {
        ActiveModel {
            id: NotSet,
            toolset_id: Set("fauc".to_string()),
            version: Set("0.0.0".to_string()),
            active: Set(active),
            draft: Set(draft),
            created_at: NotSet,
        }
    }

    fn modele(toolset_id: &str) -> Model {
        Model {
            id: 1,
            toolset_id: toolset_id.to_string(),
            version: "1.0.0".to_string(),
            active: Some(version_json(toolset_id, "1.0.0")),
            draft: None,
            created_at: chrono::Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap(),
        }
    }

    /// Scenario « un toolset sans version est refusé » : `active` et `draft` tous deux absents
    /// (`None` comme colonne vide ou JSON `null`).
    #[test]
    fn before_create_refuse_un_toolset_sans_version() {
        let rendus = vec![
            Entity::before_create(active_avec(None, None), &principal()),
            Entity::before_create(
                active_avec(Some(serde_json::Value::Null), Some(serde_json::Value::Null)),
                &principal(),
            ),
        ];
        for rendu in rendus {
            match rendu {
                Err(erreur) => {
                    assert!(erreur.code.is_none(), "aucun code sur un `HookError` de la crate");
                    assert_eq!(erreur.message, "a toolset needs an active version or a draft");
                }
                Ok(_) => panic!("un toolset sans aucune version doit être refusé"),
            }
        }
    }

    /// Scenario « un brouillon seul est accepté, colonnes dérivées » : `toolset_id` et `version`
    /// viennent du brouillon, `active` reste vide.
    #[test]
    fn before_create_accepte_un_brouillon_seul_et_derive_les_colonnes() {
        let rendu = Entity::before_create(
            active_avec(None, Some(version_json("G_git", "1.0.0"))),
            &principal(),
        )
        .expect("un brouillon valide du catalogue est accepté");
        assert_eq!(rendu.toolset_id, Set("G_git".to_string()));
        assert_eq!(rendu.version, Set("1.0.0".to_string()));
        assert_eq!(rendu.active, Set(None), "`active` reste vide");
        assert!(
            matches!(rendu.created_at, Set(_)),
            "`created_at` est posé par le hook de création"
        );
    }

    /// Scenario « active + draft présents → `version` pris de l'`active` » : les deux versions
    /// présentes portent le même nom ; `version` est dérivé de l'`active`, non du brouillon
    /// (`Must` étape 4).
    #[test]
    fn before_create_derive_version_de_l_active_quand_le_brouillon_est_present() {
        let rendu = Entity::before_create(
            active_avec(
                Some(version_json("G_git", "2.0.0")),
                Some(version_json("G_git", "1.0.0")),
            ),
            &principal(),
        )
        .expect("`active` et `draft` du même nom sont acceptés");
        assert_eq!(rendu.toolset_id, Set("G_git".to_string()));
        assert_eq!(
            rendu.version,
            Set("2.0.0".to_string()),
            "`version` vient de l'`active`, pas du brouillon"
        );
    }

    /// Scenario « un nom privé ou deux noms différents sont refusés » : le brouillon `git` (sans
    /// préfixe) est refusé pour son identifiant, le couple `G_a`/`G_b` pour ses noms discordants.
    #[test]
    fn before_create_refuse_un_nom_prive_puis_deux_noms_differents() {
        match Entity::before_create(
            active_avec(None, Some(version_json("git", "1.0.0"))),
            &principal(),
        ) {
            Err(erreur) => assert_eq!(erreur.message, "toolset id must start with G_ (catalog)"),
            Ok(_) => panic!("le nom `git` n'est pas du catalogue"),
        }
        match Entity::before_create(
            active_avec(
                Some(version_json("G_a", "1.0.0")),
                Some(version_json("G_b", "1.0.0")),
            ),
            &principal(),
        ) {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "aucun code sur un `HookError` de la crate");
                assert_eq!(
                    erreur.message, "the active and draft versions must carry the same name",
                    "message exact du refus de noms discordants"
                );
            }
            Ok(_) => panic!("`G_a` actif avec `G_b` en brouillon est incohérent"),
        }
    }

    /// Scenario « un toolset invalide est refusé sans fuite de source » : un module qui ne
    /// compile pas est refusé sur sa forme, et le texte de la source ne traverse pas.
    #[test]
    fn before_create_refuse_un_toolset_invalide_sans_fuir_le_source() {
        let marque = "SOURCE-A-NE-JAMAIS-CITER-7c21";
        let valeur = serde_json::to_value(toolset("G_casse", "1.0.0", &format!("let x = {marque}(((")))
            .expect("la fixture se sérialise");
        match Entity::before_create(active_avec(Some(valeur), None), &principal()) {
            Err(erreur) => {
                assert!(
                    erreur.message.starts_with("invalid toolset:"),
                    "la cause est citée : {erreur:?}"
                );
                assert!(
                    !erreur.message.contains(marque),
                    "le source Rhai ne traverse jamais un `HookError` : {erreur:?}"
                );
            }
            Ok(_) => panic!("un module qui ne compile pas doit être refusé"),
        }
    }

    /// `Must` étape 4 : avec les deux versions présentes, `version` vient de l'active ;
    /// `before_update` refuse un changement d'identifiant (étape 5).
    #[test]
    fn before_update_refuse_un_changement_d_identifiant() {
        let rendu = Entity::before_update(
            active_avec(Some(version_json("G_autre", "2.0.0")), None),
            &modele("G_git"),
            &principal(),
        );
        match rendu {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "aucun code sur un `HookError` de la crate");
                assert_eq!(erreur.message, "toolset id cannot change");
            }
            Ok(_) => panic!("renommer un toolset est refusé, comme pour les agents"),
        }
    }

    /// `Must` étape 2 : la fonction [`check_toolset`] est le contrat partagé de `put_draft` — elle
    /// rend la variante typée de `lymaille`, le plafond de rendu est celui de la bibliothèque, un
    /// toolset sain passe, un module qui ne compile pas est refusé sans citer sa source.
    #[test]
    fn check_toolset_applique_le_contrat_partage() {
        assert_eq!(
            DEFAULT_RENDER_CAP_CHARS, 8_000,
            "relu en source de `lymaille` 0.1.0"
        );
        check_toolset(&toolset("G_sain", "1.0.0", SOURCE_VALIDE))
            .expect("un module qui compile et un plafond respecté passent");
        let marque = "SOURCE-EXPURSEE-4d20";
        let cause = check_toolset(&toolset("G_casse", "1.0.0", &format!("let x = {marque}(((")))
            .expect_err("un module qui ne compile pas est refusé");
        assert!(
            !cause.to_string().contains(marque),
            "le source ne figure pas dans la cause : {cause}"
        );
    }

    /// Scenario « politiques du catalogue » : les métadonnées du `Must`, exactes.
    #[test]
    fn politiques_du_catalogue() {
        assert_eq!(Entity::resource_name(), "lymaille_catalog_toolsets");
        assert_eq!(Entity::read_policy(), AccessPolicy::Public);
        assert_eq!(Entity::write_policy(), AccessPolicy::AdminOnly);
        assert!(
            Entity::owner_column().is_none(),
            "le catalogue n'a pas de propriétaire"
        );
        assert!(Entity::filter_column().is_none());
        assert!(matches!(Entity::label_column(), Some(Column::ToolsetId)));
    }

    /// Exercice de la fixture discordante : `json!` est bien utilisé pour une version non objet,
    /// refusée comme version invalide (`Must` étape 2).
    #[test]
    fn before_create_refuse_une_version_non_objet() {
        let rendu = Entity::before_create(active_avec(Some(json!([1, 2])), None), &principal());
        match rendu {
            Err(erreur) => assert!(
                erreur.message.starts_with("invalid toolset:"),
                "une version non objet est une version invalide : {erreur:?}"
            ),
            Ok(_) => panic!("une version non objet doit être refusée"),
        }
    }
}
