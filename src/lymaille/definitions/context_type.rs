//! Entité `lymaille_catalog_context_types` (feature `lymaille`) — contrat porté par
//! `./context_type.sdd`.
//!
//! Table `miryad_lymaille_catalog_context_types` : les types de contexte du catalogue, administrés
//! par l'admin par le CRUD REST et MCP générique. Un `lymaille::ContextType` est un schéma JSON
//! versionné des paramètres et des rôles MCP d'un environnement, vocabulaire partagé par les
//! auteurs de toolsets (DSD `DEC-20`). La colonne JSON (DSD `DEC-18` : `active` pour la version
//! active, `draft` pour le brouillon de la session de configuration) est la source de vérité ;
//! `name` et `version` en sont des copies dérivées par les hooks, et l'identifiant doit porter le
//! préfixe `G_` du catalogue (DSD `DEC-9`, `crate::lymaille::identity::classify`).

use sea_orm::ActiveValue;
use sea_orm::entity::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::auth::AuthPrincipal;
use crate::lymaille::identity;
use crate::resource::AccessPolicy;
use crate::resource::HookError;
use crate::resource::MiryadResource;

/// Type de contexte du catalogue persisté — la table `miryad_lymaille_catalog_context_types`
/// posée par `../migration.rs` (`m20261104_000001_lymaille_lot2`), colonnes du contrat `Must` de
/// `./context_type.sdd`.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "miryad_lymaille_catalog_context_types")]
pub struct Model {
    /// Clé primaire entière auto-incrémentée (convention `miryad_*` de la crate).
    #[sea_orm(primary_key)]
    pub id: i32,
    /// Identifiant du type de contexte (`G_…`), copie dérivée du `name` de ses versions par les
    /// hooks — libellé de la ressource, unique au catalogue (contrainte de la migration).
    pub name: String,
    /// Version sémantique de la version active (à défaut du brouillon), copie dérivée par les
    /// hooks.
    pub version: String,
    /// Nature des sessions ouvertes par ce type (`work` | `config`, DSD `DEC-28`) — colonne miryad,
    /// ces champs ne vivent pas dans l'objet `ContextType` de `lymaille` (`ContextTypeStore`
    /// vérifié, amendement R1). Les hooks refusent toute autre valeur, `sub` compris : un type du
    /// catalogue n'ouvre que du travail ou de la configuration.
    pub kind: String,
    /// Agent par défaut à l'ouverture d'une session dans un contexte de ce type (DSD `DEC-28`).
    /// Colonne nullable en base, exigée posée par les hooks — motif des toolsets : le contrôle est
    /// celui des hooks, pas une contrainte `CHECK`.
    pub default_agent: Option<String>,
    /// Ensemble des agents autorisés à l'ouverture, tableau JSON d'identifiants (DSD `DEC-28`) :
    /// l'agent par défaut doit en être, un agent explicite hors de l'ensemble est refusé à
    /// l'ouverture (`mcp_access`/`assemble`, hors de cette entité).
    pub allowed_agents: serde_json::Value,
    /// Le `lymaille::ContextType` actif, colonne JSON nullable — `None` tant qu'aucune version
    /// n'est activée (DSD `DEC-18`).
    pub active: Option<serde_json::Value>,
    /// Le `lymaille::ContextType` en brouillon, colonne JSON nullable — jamais lu comme version
    /// active (DSD `DEC-18`).
    pub draft: Option<serde_json::Value>,
    /// Horodatage de création, posé par le hook de création.
    pub created_at: DateTimeUtc,
}

/// Aucune relation : les versions vivent dans la même ligne, l'inclusion d'un autre type est un
/// nom porté par le JSON, sans clé étrangère.
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Entity {
    /// Version réellement présente d'une colonne `Option<Json>` : posée (ou inchangée), non
    /// `None`, non `null` JSON — `Must` étape 1 de `./context_type.sdd`.
    fn version_presents(valeur: &ActiveValue<Option<serde_json::Value>>) -> Option<&serde_json::Value> {
        match valeur {
            ActiveValue::Set(alhee) | ActiveValue::Unchanged(alhee) => {
                alhee.as_ref().filter(|version| !version.is_null())
            }
            ActiveValue::NotSet => None,
        }
    }

    /// Étape 2 du `Must` : la version se désérialise en `lymaille::ContextType` — le constructeur
    /// compile le schéma, un schéma invalide est donc refusé ici. La cause est la variante typée
    /// de `lymaille` rendue par ses affichages fixes, qui ne citent jamais le schéma (`Must
    /// not`).
    fn charger(brute: &serde_json::Value) -> Result<::lymaille::ContextType, HookError> {
        serde_json::from_value(brute.clone())
            .map_err(|cause| HookError::new(format!("invalid context type: {cause}")))
    }

    /// Nom et version lus dans la forme sérialisée : `ContextType` ne rend pas ses accès par un
    /// accesseur public (lymaille 0.1.0, `pub(in crate::toolsets)`) — la colonne `name` est la
    /// copie de la clé de l'objet JSON, source de vérité, comme dans `../store/toolset.rs`. La
    /// désérialisation a réussi avant cet appel, les deux clés y sont donc requises ; le refus
    /// n'est qu'un garde-fou sans `unwrap`.
    fn champ_texte(brute: &serde_json::Value, champ: &str) -> Result<String, HookError> {
        brute
            .get(champ)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| HookError::new(format!("invalid context type: no `{champ}` in the stored form")))
    }

    /// Porte des trois colonnes DEC-28 (réalignement R1 de `./context_type.sdd`) : nature parmi
    /// `work`/`config` (`sub` et toute autre valeur sont refusés), agent par défaut posé, agent
    /// par défaut membre de l'ensemble autorisé. Les valeurs `NotSet` comptent comme absentes :
    /// la porte refuse, le `NotSet` n'atteint jamais la base. Elle est placée avant les portes
    /// JSON : les colonnes sont indépendantes de `active`/`draft`, et l'arbitrage `[?]` du
    /// nommage `G_` ouvert rendrait la moindre des portes JSON infranchissable avec un nom du
    /// catalogue.
    fn valider_champs_pilotes(active: &ActiveModel) -> Result<(), HookError> {
        let nature = match &active.kind {
            ActiveValue::Set(nature) | ActiveValue::Unchanged(nature) => nature.as_str(),
            ActiveValue::NotSet => "",
        };
        if !matches!(nature, "work" | "config") {
            return Err(HookError::new("context type kind must be work or config"));
        }
        let defaut = match &active.default_agent {
            ActiveValue::Set(defaut) | ActiveValue::Unchanged(defaut) => defaut.as_ref(),
            ActiveValue::NotSet => None,
        };
        let Some(defaut) = defaut else {
            return Err(HookError::new("a context type needs a default agent"));
        };
        let autorise = match &active.allowed_agents {
            ActiveValue::Set(agents) | ActiveValue::Unchanged(agents) => agents
                .as_array()
                .is_some_and(|agents| agents.iter().any(|agent| agent.as_str() == Some(defaut.as_str()))),
            ActiveValue::NotSet => false,
        };
        if !autorise {
            return Err(HookError::new(
                "context type default agent must be in the allowed agents",
            ));
        }
        Ok(())
    }

    /// Porte commune de `before_create` (`existing` absent) et `before_update`, dans l'ordre du
    /// `Must` de `./context_type.sdd`, interrompue au premier échec : au moins une version, les
    /// trois colonnes pilotes DEC-28 (réalignement R1), chacune des versions désérialisée en
    /// `lymaille::ContextType` (le constructeur compile le schéma), `name` du catalogue (`G_`
    /// exact via `classify`, DSD `DEC-9` — tout autre rendu, y compris une erreur de `classify`,
    /// est le même refus), et sur une mise à jour `name` ne change pas. `name` et `version`
    /// (celle de l'active, à défaut du brouillon) sont toujours réécrits.
    fn valider_et_deriver(
        mut active: ActiveModel,
        existing: Option<&Model>,
    ) -> Result<ActiveModel, HookError> {
        let brute_active = Self::version_presents(&active.active);
        let brute_draft = Self::version_presents(&active.draft);
        if brute_active.is_none() && brute_draft.is_none() {
            return Err(HookError::new(
                "a context type needs an active version or a draft",
            ));
        }
        Self::valider_champs_pilotes(&active)?;
        brute_active.map(Self::charger).transpose()?;
        brute_draft.map(Self::charger).transpose()?;
        let brute = brute_active
            .or(brute_draft)
            .ok_or_else(|| HookError::new("a context type needs an active version or a draft"))?;
        let name = Self::champ_texte(brute, "name")?;
        let version = Self::champ_texte(brute, "version")?;
        if !matches!(identity::classify(&name), Ok(identity::IdKind::Catalog)) {
            return Err(HookError::new("context type name must start with G_ (catalog)"));
        }
        if let Some(ligne) = existing
            && name != ligne.name
        {
            return Err(HookError::new("context type name cannot change"));
        }
        active.name = ActiveValue::Set(name);
        active.version = ActiveValue::Set(version);
        if existing.is_none() && matches!(active.created_at, ActiveValue::NotSet) {
            active.created_at = ActiveValue::Set(chrono::Utc::now());
        }
        Ok(active)
    }
}

/// Métadonnées de la ressource `lymaille_catalog_context_types`, lue à l'identique par REST et
/// MCP. Catalogue (DSD `DEC-14`, `DEC-20`) : lecture par tout authentifié, écriture par l'admin
/// seul, sans propriétaire.
impl MiryadResource for Entity {
    fn resource_name() -> &'static str {
        "lymaille_catalog_context_types"
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

    /// Valide les versions présentes et dérive `name` et `version` — voir
    /// [`Entity::valider_et_deriver`].
    fn before_create(
        active: Self::ActiveModel,
        _principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        Entity::valider_et_deriver(active, None)
    }

    /// Mêmes étapes que [`Self::before_create`], plus l'identité : un `name` qui change est
    /// refusé (`Must` de `./context_type.sdd`, précédent `./toolset.rs`).
    fn before_update(
        active: Self::ActiveModel,
        existing: &Self::Model,
        _principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        Entity::valider_et_deriver(active, Some(existing))
    }

    // `before_delete` volontairement non surdéclaré : aucun contrôle d'usage au lot 2, le défaut
    // identité du trait s'applique.
}

#[cfg(test)]
mod tests {
    use super::ActiveModel;
    use super::Entity;
    use crate::auth::AuthPrincipal;
    use crate::auth::PrincipalSource;
    use crate::resource::MiryadResource;
    use ::lymaille::ContextType;
    use sea_orm::ActiveValue::NotSet;
    use sea_orm::ActiveValue::Set;
    use serde_json::json;
    use vynil_core::Semver;

    // ── Fixtures — purement mémoire : hooks purs sur l'`ActiveModel`, aucune connexion. La forme
    // sérialisée de base est produite par la bibliothèque elle-même (`ContextType::new` puis
    // `to_value`), seule la clé `name` est corrigée au nom voulu — `G_…` ne peut pas sortir du
    // constructeur, dont la grammaire n'accepte que minuscules, chiffres et tirets (lymaille
    // 0.1.0, `toolsets/context_type.rs::is_valid_name`).

    fn principal() -> AuthPrincipal {
        AuthPrincipal {
            subject: "sujet-de-test".to_string(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::ApiToken { token_id: 1 },
        }
    }

    fn semver(text: &str) -> Semver {
        Semver::parse(text).expect("la fixture porte une version valide")
    }

    /// Forme sérialisée d'un type de contexte nommé `name`, version `version`, schéma `schema`.
    fn type_json(name: &str, version: &str, schema: serde_json::Value) -> serde_json::Value {
        let built = ContextType::new(
            "cluster".to_string(),
            semver("1.0.0"),
            json!({ "type": "object" }),
        )
        .expect("la fixture de base se construit");
        let mut value = serde_json::to_value(built).expect("la fixture se sérialise");
        let Some(map) = value.as_object_mut() else {
            panic!("la forme sérialisée d'un `ContextType` est un objet");
        };
        map.insert("name".to_string(), json!(name));
        map.insert("version".to_string(), json!(version));
        map.insert("schema".to_string(), schema);
        value
    }

    /// `ActiveModel` dans la forme qui atteint le hook, champs du réalignement R1 réglés : colonnes
    /// dérivées fausses (le hook doit les réécrire), versions et champs DEC-28 voulus, clé et
    /// horodatage non posés.
    fn active_avec_champs(
        kind: &str,
        default_agent: Option<&str>,
        allowed_agents: serde_json::Value,
        active: Option<serde_json::Value>,
        draft: Option<serde_json::Value>,
    ) -> ActiveModel {
        ActiveModel {
            id: NotSet,
            name: Set("faux-nom".to_string()),
            version: Set("0.0.0".to_string()),
            kind: Set(kind.to_string()),
            default_agent: Set(default_agent.map(str::to_owned)),
            allowed_agents: Set(allowed_agents),
            active: Set(active),
            draft: Set(draft),
            created_at: NotSet,
        }
    }

    /// `ActiveModel` dans la forme qui atteint le hook : colonnes dérivées fausses (le hook doit
    /// les réécrire), versions voulues, clé et horodatage non posés. Champs DEC-28 valides
    /// (`work`, `G_triage` autorisé par lui-même) pour que les Scenarios existants atteignent
    /// leurs propres portes.
    fn active_avec(active: Option<serde_json::Value>, draft: Option<serde_json::Value>) -> ActiveModel {
        active_avec_champs("work", Some("G_triage"), json!(["G_triage"]), active, draft)
    }

    /// Scenario « un type valide du catalogue est accepté, ses colonnes dérivées ».
    ///
    /// RED sous `lymaille` 0.1.0, et prouvé infaisable en l'état : aucun `ContextType` nommé
    /// `G_cluster` ne peut exister — `is_valid_name` (lymaille 0.1.0, `toolsets/context_type.rs`)
    /// n'accepte que minuscules ASCII, chiffres et tirets, et la désérialisation passe par le même
    /// contrôle. Le contrat (`classify` exige `G_`, désérialisation l'interdit) est à arbitrer —
    /// voir la ligne `[?]` de `Tasks` de `context_type.sdd`.
    #[test]
    #[ignore = "arbitrage G_ en attente (Tasks [?] de context_type.sdd) : lymaille 0.1.0 refuse les noms G_ dans ContextType"]
    fn before_create_accepte_un_type_valide_du_catalogue_et_derive_les_colonnes() {
        let rendu = Entity::before_create(
            active_avec(
                Some(type_json(
                    "G_cluster",
                    "1.0.0",
                    json!({"type": "object", "properties": {"cluster": {"type": "string"}}, "required": ["cluster"]}),
                )),
                None,
            ),
            &principal(),
        )
        .expect("un type `G_cluster` 1.0.0 du catalogue est accepté");
        assert_eq!(
            rendu.name,
            Set("G_cluster".to_string()),
            "`name` est dérivé de `active`"
        );
        assert_eq!(
            rendu.version,
            Set("1.0.0".to_string()),
            "`version` est dérivée de `active`"
        );
    }

    /// Scenario « ni active ni draft » : les deux colonnes absentes (`None`) ou JSON `null`.
    #[test]
    fn before_create_refuse_un_type_sans_version() {
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
                    assert!(
                        erreur.code.is_none(),
                        "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                    );
                    assert!(!erreur.message.is_empty(), "le refus motive la règle manquante");
                }
                Ok(_) => panic!("un type de contexte sans aucune version doit être refusé"),
            }
        }
    }

    /// Scenario « un schéma invalide est refusé » : la cause de `lymaille` traverse, le message
    /// ne cite jamais le schéma (marque plantée dans le schéma pour le prouver).
    #[test]
    fn before_create_refuse_un_schema_invalide_sans_le_citer() {
        let marque = "SCHEMA-A-NE-JAMAIS-CITER-7c21";
        let rendu = Entity::before_create(
            active_avec(Some(type_json("cluster", "1.0.0", json!({"type": marque}))), None),
            &principal(),
        );
        match rendu {
            Err(erreur) => {
                assert!(
                    erreur.message.starts_with("invalid context type:"),
                    "la cause de lymaille est citée : {erreur:?}"
                );
                assert!(
                    !erreur.message.contains(marque),
                    "le schéma ne traverse jamais un `HookError` : {erreur:?}"
                );
            }
            Ok(_) => panic!("un schéma qui ne compile pas doit être refusé"),
        }
    }

    /// Scenario « un nom privé est refusé » : `classify` ne rend pas `Catalog` pour `cluster`.
    #[test]
    fn before_create_refuse_un_nom_prive() {
        let rendu = Entity::before_create(
            active_avec(
                Some(type_json("cluster", "1.0.0", json!({"type": "object"}))),
                None,
            ),
            &principal(),
        );
        match rendu {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "aucun code `MRD-*` sur un `HookError`");
                assert_eq!(erreur.message, "context type name must start with G_ (catalog)");
            }
            Ok(_) => panic!("`cluster` n'est pas un identifiant du catalogue"),
        }
    }

    /// Scenario (réalignement R1, DSD `DEC-28`) « un type sans agent par défaut est refusé » :
    /// la nature et l'ensemble sont conformes, seul l'agent par défaut manque.
    #[test]
    fn before_create_refuse_un_type_sans_agent_par_defaut() {
        let rendu = Entity::before_create(
            active_avec_champs(
                "work",
                None,
                json!(["G_triage"]),
                Some(type_json("cluster", "1.0.0", json!({"type": "object"}))),
                None,
            ),
            &principal(),
        );
        match rendu {
            Err(erreur) => {
                assert!(
                    erreur.code.is_none(),
                    "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                );
                assert_eq!(erreur.message, "a context type needs a default agent");
            }
            Ok(_) => panic!("un type de contexte sans agent par défaut doit être refusé"),
        }
    }

    /// Scenario (réalignement R1, DSD `DEC-28`) « un type sans agent par défaut est refusé »,
    /// branche `ActiveValue::NotSet` du `Must` : « un `ActiveValue::NotSet` compte comme absent
    /// et refuse ». Le `Must` est vérifié par construction directe de l'`ActiveModel` — la
    /// colonne n'est pas posée `None`, elle n'est jamais posée du tout.
    #[test]
    fn before_create_refuse_un_agent_par_defaut_non_pose() {
        let mut active = active_avec_champs(
            "work",
            Some("G_triage"),
            json!(["G_triage"]),
            Some(type_json("cluster", "1.0.0", json!({"type": "object"}))),
            None,
        );
        active.default_agent = NotSet;
        let rendu = Entity::before_create(active, &principal());
        match rendu {
            Err(erreur) => {
                assert!(
                    erreur.code.is_none(),
                    "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                );
                assert_eq!(erreur.message, "a context type needs a default agent");
            }
            Ok(_) => panic!("un `default_agent` `NotSet` compte comme absent et refuse : {rendu:?}"),
        }
    }

    /// Scenario (réalignement R1, DSD `DEC-28`) « un agent par défaut hors de l'ensemble autorisé
    /// est refusé » : `G_autre` n'est pas dans `["G_triage"]`.
    #[test]
    fn before_create_refuse_un_agent_par_defaut_hors_de_l_ensemble_autorise() {
        let rendu = Entity::before_create(
            active_avec_champs(
                "work",
                Some("G_autre"),
                json!(["G_triage"]),
                Some(type_json("cluster", "1.0.0", json!({"type": "object"}))),
                None,
            ),
            &principal(),
        );
        match rendu {
            Err(erreur) => {
                assert!(
                    erreur.code.is_none(),
                    "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                );
                assert_eq!(
                    erreur.message,
                    "context type default agent must be in the allowed agents"
                );
            }
            Ok(_) => panic!("un agent par défaut hors de l'ensemble autorisé doit être refusé"),
        }
    }

    /// Scenario (réalignement R1, DSD `DEC-28`) « une nature inconnue est refusée » : le type
    /// n'ouvre que du `work` ou du `config` — `sub` et toute autre valeur sont refusés. Les cinq
    /// cas du `Must` : `sub`, `travail`, `WORK`, vide, et `ActiveValue::NotSet` — un `NotSet`
    /// compte comme absent et refuse.
    #[test]
    fn before_create_refuse_une_nature_inconnue() {
        let natures = [
            Set("sub".to_string()),
            Set("travail".to_string()),
            Set("WORK".to_string()),
            Set(String::new()),
            NotSet,
        ];
        for nature in &natures {
            let mut active = active_avec_champs(
                "work",
                Some("G_triage"),
                json!(["G_triage"]),
                Some(type_json("cluster", "1.0.0", json!({"type": "object"}))),
                None,
            );
            active.kind = nature.clone();
            let rendu = Entity::before_create(active, &principal());
            match rendu {
                Err(erreur) => {
                    assert!(
                        erreur.code.is_none(),
                        "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                    );
                    assert_eq!(
                        erreur.message, "context type kind must be work or config",
                        "la nature {nature:?} est refusée pour la bonne raison"
                    );
                }
                Ok(_) => panic!("une nature {nature:?} inconnue doit être refusée"),
            }
        }
    }
}
