//! Entité `lymaille_catalog_contexts` (feature `lymaille`) — contrat porté par
//! `./catalog_context.sdd`.
//!
//! Table `miryad_lymaille_catalog_contexts` : les contextes de toolset partagés par toute
//! l'instance (DSD `DEC-20`, `DEC-28` ; le niveau `Shared` porte le nom amont
//! `ContextLevel::Shared` depuis `lymaille` 0.2.0 — miryad-core n'a pas d'entité projet),
//! administrés par l'admin par le CRUD REST et MCP générique. La colonne `context` (JSON, un
//! `lymaille::ToolsetContext` complet) est la source de vérité ; `name` en est une copie dérivée
//! par les hooks, et l'identifiant doit porter le préfixe `G_` du catalogue (DSD `DEC-9`,
//! `crate::lymaille::identity::classify`). Un contexte ne contient **aucun secret** : le
//! constructeur de `lymaille` applique son détecteur à la désérialisation. Les liaisons rôle →
//! serveur ne sont pas contrôlées ici (hook synchrone et sans connexion) : le droit d'usage est
//! contrôlé à l'assemblage et à l'appel (`../mcp_access.sdd`).

use sea_orm::ActiveValue;
use sea_orm::entity::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::auth::AuthPrincipal;
use crate::lymaille::identity;
use crate::resource::AccessPolicy;
use crate::resource::HookError;
use crate::resource::MiryadResource;

/// Contexte de toolset partagé persisté — la table `miryad_lymaille_catalog_contexts` posée par
/// `../migration.rs` (`m20261104_000001_lymaille_lot2`), colonnes du contrat `Must` de
/// `./catalog_context.sdd`.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS)]
#[ts(rename = "CatalogContextModel")]
#[sea_orm(table_name = "miryad_lymaille_catalog_contexts")]
pub struct Model {
    /// Clé primaire entière auto-incrémentée (convention `miryad_*` de la crate).
    #[sea_orm(primary_key)]
    pub id: i32,
    /// Identifiant du contexte (`G_…`), copie dérivée du `name` du contexte par les hooks —
    /// libellé de la ressource, unique au catalogue (contrainte de la migration).
    pub name: String,
    /// Le `lymaille::ToolsetContext`, colonne JSON : la source de vérité, validée par
    /// `before_create` et `before_update`. Ne contient aucun secret (le constructeur de
    /// `lymaille` refuse une valeur qui en a l'air).
    #[schema(value_type = Object)]
    #[ts(as = "::lymaille::ToolsetContext")]
    pub context: serde_json::Value,
    /// Référence par `name` à un type de contexte du catalogue (`../definitions/context_type.rs`
    /// — arbitrage `DEC-28` du 2026-11-05) : la nature, l'agent par défaut et les agents autorisés
    /// des sessions ouvertes sur ce contexte en viennent. `None` : le contexte ne précise aucun
    /// type, l'ouverture de session le refuse (`../interactive/manager.sdd`). L'**existence** du
    /// type est vérifiée à l'ouverture, pas au hook (hook synchrone sans connexion) ; le hook ne
    /// contrôle que la **forme** de la référence.
    pub context_type: Option<String>,
    /// Horodatage de création, posé par le hook de création.
    #[schema(value_type = String, format = DateTime)]
    #[ts(type = "string")]
    pub created_at: DateTimeUtc,
}

/// Aucune relation : les liaisons rôle → serveur sont des noms portés par le JSON, sans clé
/// étrangère (contrôle à l'assemblage et à l'appel, `../mcp_access.sdd`).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Entity {
    /// Jeton JSON (`snake_case`, lymaille 0.2.0) du niveau `Shared` — le nom amont de ce niveau
    /// est `ContextLevel::Shared` depuis `lymaille` 0.2.0 ; l'ancien jeton `project` reste lu par
    /// alias amont, jamais écrit (DSD `DEC-28`, réalignement R3 de `./catalog_context.sdd`).
    const NIVEAU_PARTAGE: &'static str = "shared";

    /// Nom et niveau lus dans la forme sérialisée : `ToolsetContext` ne rend ni `name` ni
    /// `level` par un accesseur public (lymaille 0.1.0, `name` en
    /// `pub(in crate::toolsets)`, `level` sans accesseur) — la colonne `name` est la copie de
    /// la clé de l'objet JSON, source de vérité, comme dans `./context_type.rs`. La
    /// désérialisation a réussi avant cet appel, les clés y sont donc requises ; le refus
    /// n'est qu'un garde-fou sans `unwrap`.
    fn champ_texte(brute: &serde_json::Value, champ: &str) -> Result<String, HookError> {
        brute
            .get(champ)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                HookError::new(format!(
                    "invalid catalog context: no `{champ}` in the stored form"
                ))
            })
    }

    /// Le `context` se désérialise en `lymaille::ToolsetContext` — le constructeur de la
    /// bibliothèque applique son détecteur de secrets à chaque paramètre, rôle et surcharge
    /// (lymaille 0.1.0, `toolsets/toolset_context.rs`). Un refus pour secret détecté porte le
    /// message fixe du contrat, jamais la valeur ; les autres causes traversent en texte serde,
    /// qui ne cite que des noms de champ.
    fn charger(brute: &serde_json::Value) -> Result<(), HookError> {
        serde_json::from_value::<::lymaille::ToolsetContext>(brute.clone())
            .map(|_| ())
            .map_err(|cause| {
                let cause = cause.to_string();
                if cause.contains("looks like a secret") {
                    HookError::new("catalog context holds a secret-like value")
                } else {
                    HookError::new(format!("invalid catalog context: {cause}"))
                }
            })
    }

    /// Porte commune de `before_create` (`existing` absent) et `before_update`, dans l'ordre du
    /// `Must` de `./catalog_context.sdd`, interrompue au premier échec : `context` objet JSON
    /// présent, désérialisable en `lymaille::ToolsetContext` (le constructeur refuse un secret,
    /// sans jamais le citer), `level` du contexte égal à `Shared` (jeton JSON `shared`),
    /// `name` du catalogue (`G_` exact via `classify`, DSD `DEC-9` — tout autre rendu, y
    /// compris une erreur de `classify`, est le même refus), et sur une mise à jour `name` ne
    /// change pas. `name` est toujours réécrit depuis le contexte ; `created_at` est posé à la
    /// création s'il manque, et reste celui de la ligne existante à la mise à jour.
    fn valider_et_deriver(
        mut active: ActiveModel,
        existing: Option<&Model>,
    ) -> Result<ActiveModel, HookError> {
        let brute = match &active.context {
            ActiveValue::Set(brute) | ActiveValue::Unchanged(brute) if brute.is_object() => brute.clone(),
            ActiveValue::Set(_) | ActiveValue::Unchanged(_) | ActiveValue::NotSet => {
                return Err(HookError::new("catalog context is required"));
            }
        };
        Self::charger(&brute)?;
        if Self::champ_texte(&brute, "level")? != Self::NIVEAU_PARTAGE {
            return Err(HookError::new("catalog contexts have level shared"));
        }
        let name = Self::champ_texte(&brute, "name")?;
        if !matches!(identity::classify(&name), Ok(identity::IdKind::Catalog)) {
            return Err(HookError::new(
                "catalog context name must start with G_ (catalog)",
            ));
        }
        if let Some(ligne) = existing
            && name != ligne.name
        {
            return Err(HookError::new("catalog context name cannot change"));
        }
        let reference = match &active.context_type {
            ActiveValue::Set(reference) | ActiveValue::Unchanged(reference) => reference.as_deref(),
            ActiveValue::NotSet => None,
        };
        if reference
            .is_some_and(|reference| !matches!(identity::classify(reference), Ok(identity::IdKind::Catalog)))
        {
            // Issue #74 (amendement du 2026-11-13, écart spec ↔ code corrigé) : la référence
            // désigne un type de contexte du catalogue — un objet `G_…` (`./context_type.sdd`,
            // `DEC-28` révisé le 2026-11-05 ; l'arbitrage (a) du 2026-11-04 est renversé). Un nom
            // privé ou `H_…` est refusé. L'existence n'est vérifiée qu'à l'ouverture : le hook
            // ne juge que la forme.
            return Err(HookError::new(
                "catalog context type reference is not a valid name",
            ));
        }
        active.name = ActiveValue::Set(name);
        match existing {
            None => {
                if matches!(active.created_at, ActiveValue::NotSet) {
                    active.created_at = ActiveValue::Set(chrono::Utc::now());
                }
            }
            Some(ligne) => active.created_at = ActiveValue::Set(ligne.created_at),
        }
        Ok(active)
    }
}

/// Métadonnées de la ressource `lymaille_catalog_contexts`, lue à l'identique par REST et MCP.
/// Catalogue (DSD `DEC-14`, `DEC-20`) : lecture par tout authentifié, écriture par l'admin seul,
/// sans propriétaire.
impl MiryadResource for Entity {
    fn resource_name() -> &'static str {
        "lymaille_catalog_contexts"
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

    /// Valide le contexte et réécrit `name` — voir `Entity::valider_et_deriver`.
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

    /// Mêmes étapes que [`Self::before_create`], plus l'identité : un `name` qui change est
    /// refusé (`Must` de `./catalog_context.sdd`, précédent `./agent.rs`).
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

    // `before_delete` volontairement non surdéclaré : aucun contrôle d'usage au lot 2, le défaut
    // identité du trait s'applique.
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
    use crate::resource::MiryadResource;
    use ::lymaille::ContextLevel;
    use ::lymaille::ToolsetContext;
    use chrono::TimeZone as _;
    use chrono::Utc;
    use sea_orm::ActiveValue::NotSet;
    use sea_orm::ActiveValue::Set;
    use serde_json::json;

    // ── Fixtures — purement mémoire : hooks purs sur l'`ActiveModel`, aucune connexion. La forme
    // sérialisée de base est produite par la bibliothèque elle-même (`ToolsetContext::new` puis
    // `to_value`). Contrairement à `ContextType` (voir la ligne `[?]` de `./context_type.sdd`),
    // `ToolsetContext::new` ne refuse que le nom vide (lymaille 0.1.0,
    // `toolsets/toolset_context.rs`) : un nom `G_…` sort du constructeur et se désérialise.

    fn principal() -> AuthPrincipal {
        AuthPrincipal {
            subject: "sujet-de-test".to_string(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::ApiToken { token_id: 1 },
        }
    }

    /// Forme sérialisée d'un contexte nommé `name`, de niveau `niveau`, paramètres `parameters`
    /// injectés directement — `with_parameter` refuse un secret, ce que certaines fixtures
    /// veulent justement porter jusqu'au hook.
    fn contexte(name: &str, niveau: ContextLevel, parameters: serde_json::Value) -> serde_json::Value {
        let construit = ToolsetContext::new(name.to_string(), niveau).expect("la fixture se construit");
        let mut valeur = serde_json::to_value(construit).expect("la fixture se sérialise");
        let Some(map) = valeur.as_object_mut() else {
            panic!("la forme sérialisée d'un `ToolsetContext` est un objet");
        };
        map.insert("parameters".to_string(), parameters);
        valeur
    }

    /// `ActiveModel` dans la forme qui atteint le hook : colonne dérivée fausse (le hook doit la
    /// réécrire), `context` posé, clé et horodatage non posés.
    fn active_avec(context: serde_json::Value) -> ActiveModel {
        ActiveModel {
            id: NotSet,
            name: Set("faux-nom".to_string()),
            context: Set(context),
            context_type: NotSet,
            created_at: NotSet,
        }
    }

    /// Ligne existante des hooks `before_update`.
    fn modele(name: &str) -> Model {
        Model {
            id: 1,
            name: name.to_string(),
            context: contexte(name, ContextLevel::Shared, json!({"namespace": "prod"})),
            context_type: None,
            created_at: Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap(),
        }
    }

    /// Scenario « un contexte Shared valide est accepté » : `G_prod` de niveau `shared` avec
    /// un paramètre `namespace` passe, et `name` est réécrit depuis le contexte.
    #[tokio::test]
    async fn before_create_accepte_un_contexte_shared_valide_et_derive_le_nom() {
        let rendu = Entity::before_create(
            &hook_db(),
            active_avec(contexte(
                "G_prod",
                ContextLevel::Shared,
                json!({"namespace": "prod"}),
            )),
            &principal(),
        )
        .await
        .expect("un contexte `G_prod` de niveau shared est accepté");
        assert_eq!(
            rendu.name,
            Set("G_prod".to_string()),
            "`name` est réécrit depuis le contexte"
        );
        assert!(
            matches!(rendu.created_at, Set(_)),
            "`created_at` est posé par le hook de création quand il est absent"
        );
    }

    /// Scenario « un secret est refusé sans l'écho » : le message porte la règle, jamais la
    /// valeur (`Must not` : citer une valeur de paramètre dans une erreur).
    #[tokio::test]
    async fn before_create_refuse_un_secret_sans_le_citer() {
        let rendu = Entity::before_create(
            &hook_db(),
            active_avec(contexte(
                "G_prod",
                ContextLevel::Shared,
                json!({"api_key": "sk-abcdef"}),
            )),
            &principal(),
        )
        .await;
        match rendu {
            Err(erreur) => {
                assert!(
                    erreur.code.is_none(),
                    "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                );
                assert!(
                    erreur.message.contains("secret-like value"),
                    "le refus nomme la règle : {erreur:?}"
                );
                assert!(
                    !erreur.message.contains("sk-abcdef"),
                    "la valeur ne traverse jamais un `HookError` : {erreur:?}"
                );
            }
            Ok(_) => panic!("un paramètre `sk-abcdef` devait être refusé"),
        }
    }

    /// Scenario « un niveau user est refusé au catalogue » : `level` doit être `shared`.
    #[tokio::test]
    async fn before_create_refuse_un_niveau_user() {
        let rendu = Entity::before_create(
            &hook_db(),
            active_avec(contexte("G_prod", ContextLevel::User, json!({}))),
            &principal(),
        )
        .await;
        match rendu {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "aucun code `MRD-*` sur un `HookError`");
                assert_eq!(erreur.message, "catalog contexts have level shared");
            }
            Ok(_) => panic!("un contexte de niveau user n'a rien au catalogue"),
        }
    }

    /// Scenario « un non-admin n'écrit pas au catalogue » — précédent lot 1 (`./agent.rs`) : la
    /// politique `AdminOnly` du moteur est verrouillée par les métadonnées exactes de la
    /// ressource (le refus d'écriture par un principal non admin est prouvé par le harnais du
    /// moteur, `src/resource.rs`/`src/rest`).
    #[test]
    fn politiques_du_catalogue() {
        assert_eq!(Entity::resource_name(), "lymaille_catalog_contexts");
        assert_eq!(Entity::read_policy(), AccessPolicy::Public);
        assert_eq!(Entity::write_policy(), AccessPolicy::AdminOnly);
        assert!(
            Entity::owner_column().is_none(),
            "le catalogue n'a pas de propriétaire"
        );
        assert!(Entity::filter_column().is_none(), "aucune colonne de filtre");
        assert!(matches!(Entity::label_column(), Some(Column::Name)));
    }

    /// `Must` : `classify(name)` = `Catalog` — un nom privé (`prod`) est refusé.
    #[tokio::test]
    async fn before_create_refuse_un_nom_prive() {
        let rendu = Entity::before_create(
            &hook_db(),
            active_avec(contexte("prod", ContextLevel::Shared, json!({}))),
            &principal(),
        )
        .await;
        match rendu {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "aucun code `MRD-*` sur un `HookError`");
                assert_eq!(
                    erreur.message,
                    "catalog context name must start with G_ (catalog)"
                );
            }
            Ok(_) => panic!("`prod` n'est pas un identifiant du catalogue"),
        }
    }

    /// `Must` étape 1 : `context` absent ou non objet est refusé avant toute désérialisation.
    #[tokio::test]
    async fn before_create_refuse_un_contexte_absent_ou_non_objet() {
        let sans = ActiveModel {
            context: NotSet,
            ..active_avec(serde_json::Value::Null)
        };
        match Entity::before_create(&hook_db(), sans, &principal()).await {
            Err(erreur) => assert_eq!(erreur.message, "catalog context is required"),
            Ok(_) => panic!("un `context` `NotSet` doit être refusé"),
        }
        match Entity::before_create(
            &hook_db(),
            active_avec(json!(["pas", "un", "objet"])),
            &principal(),
        )
        .await
        {
            Err(erreur) => assert_eq!(erreur.message, "catalog context is required"),
            Ok(_) => panic!("un `context` non objet doit être refusé"),
        }
    }

    /// Scenario « la référence à un type du catalogue est acceptée » (issue #74, amendement du
    /// 2026-11-13) : `context_type` posé doit être de forme catalogue — `classify` = `Catalog`
    /// (les types de contexte sont des objets du catalogue, `./context_type.sdd`). `None` (ou
    /// absent) traverse le hook, et une référence `G_…` traverse inchangée : l'existence du type
    /// est vérifiée à l'ouverture de session, pas ici. `before_update` sur la même référence,
    /// sans changement de nom, est accepté.
    #[tokio::test]
    async fn la_reference_a_un_type_du_catalogue_est_acceptee() {
        let sans = Entity::before_create(
            &hook_db(),
            active_avec(contexte("G_prod", ContextLevel::Shared, json!({}))),
            &principal(),
        )
        .await
        .expect("un contexte sans type est accepté par le hook");
        assert!(
            matches!(sans.context_type, NotSet),
            "`None` (ou absent) traverse le hook : {:?}",
            sans.context_type
        );
        let avec = Entity::before_create(
            &hook_db(),
            ActiveModel {
                context_type: Set(Some("G_home".to_string())),
                ..active_avec(contexte("G_home", ContextLevel::Shared, json!({})))
            },
            &principal(),
        )
        .await
        .expect("une référence `G_home` est acceptée sans contrôle d'existence");
        assert_eq!(
            avec.context_type,
            Set(Some("G_home".to_string())),
            "le hook ne réécrit pas la référence"
        );
        assert_eq!(avec.name, Set("G_home".to_string()));
        let mis_a_jour = Entity::before_update(
            &hook_db(),
            ActiveModel {
                context_type: Set(Some("G_home".to_string())),
                ..active_avec(contexte("G_home", ContextLevel::Shared, json!({})))
            },
            &modele("G_home"),
            &principal(),
        )
        .await
        .expect("une mise à jour sur une référence catalogue, nom inchangé, est acceptée");
        assert_eq!(
            mis_a_jour.name,
            Set("G_home".to_string()),
            "`before_update` sur une référence catalogue garde le nom"
        );
    }

    /// Scenario « une référence privée ou hôte est refusée » (issue #74) : un nom privé (`home`),
    /// un identifiant `H_…` ou de casse réservée (`g_home`), une forme invalide (`a-b`, vide) ne
    /// sont pas des références de type du catalogue. Le `HookError` ne porte aucun code `MRD-*`
    /// et n'échoe pas la valeur.
    #[tokio::test]
    async fn une_reference_privee_ou_hote_est_refusee() {
        for reference in ["", "home", "H_home", "g_home", "a-b", "h_cluster"] {
            let active = ActiveModel {
                context_type: Set(Some(reference.to_string())),
                ..active_avec(contexte("G_prod", ContextLevel::Shared, json!({})))
            };
            match Entity::before_create(&hook_db(), active, &principal()).await {
                Err(erreur) => {
                    assert!(
                        erreur.code.is_none(),
                        "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                    );
                    assert!(
                        erreur.message.contains("context type"),
                        "le refus nomme la règle en cause : {erreur:?}"
                    );
                    assert!(
                        reference.is_empty() || !erreur.message.contains(reference),
                        "le refus n'échoe pas la valeur : {erreur:?}"
                    );
                }
                Ok(_) => panic!("`{reference}` n'est pas une référence de type du catalogue"),
            }
        }
    }

    /// Scenario (`before_update`) : `name` ne change pas.
    #[tokio::test]
    async fn before_update_refuse_un_renommage() {
        let rendu = Entity::before_update(
            &hook_db(),
            active_avec(contexte(
                "G_autre",
                ContextLevel::Shared,
                json!({"namespace": "dev"}),
            )),
            &modele("G_prod"),
            &principal(),
        )
        .await;
        match rendu {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "aucun code `MRD-*` sur un `HookError`");
                assert_eq!(erreur.message, "catalog context name cannot change");
            }
            Ok(_) => panic!("renommer un contexte change son identité : la mise à jour est refusée"),
        }
    }

    /// `Must` (`before_update`) : une mise à jour qui garde le nom le réécrit depuis le contexte
    /// et laisse `created_at` à celui de la ligne existante.
    #[tokio::test]
    async fn before_update_derive_le_nom_et_conserve_la_date_de_creation() {
        let existing = modele("G_prod");
        let rendu = Entity::before_update(
            &hook_db(),
            active_avec(contexte(
                "G_prod",
                ContextLevel::Shared,
                json!({"namespace": "prod"}),
            )),
            &existing,
            &principal(),
        )
        .await
        .expect("une mise à jour qui garde le nom est acceptée");
        assert_eq!(rendu.name, Set("G_prod".to_string()));
        assert_eq!(
            rendu.created_at,
            Set(existing.created_at),
            "`created_at` reste celui de la ligne existante"
        );
    }

    /// Scenario « `resource_openapi` compile et décrit la ressource » (issue #79, amendement du
    /// 2026-11-13 de `./catalog_context.sdd`) : l'appel compile (le `Model` dérive
    /// `utoipa::ToSchema`), le schéma porte toutes les colonnes, `created_at` est un
    /// `string`/`date-time` et `context` est un objet libre sans propriétés.
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
        for colonne in ["id", "name", "context", "context_type", "created_at"] {
            assert!(
                proprietes.contains_key(colonne),
                "colonne {colonne} absente du schéma : {schema}"
            );
        }
        assert_eq!(proprietes["created_at"]["type"], "string");
        assert_eq!(proprietes["created_at"]["format"], "date-time");
        assert_eq!(
            proprietes["context"]["type"], "object",
            "`context` est décrit en objet libre"
        );
        assert!(
            proprietes["context"].get("properties").is_none(),
            "l'objet libre de `context` ne détaille aucune propriété : {}",
            proprietes["context"]
        );
    }

    /// Scenario « l'export TypeScript du Model correspond à la sérialisation » (issue #72,
    /// `./catalog_context.sdd`) : la déclaration `catalog_context.ts` écrite par `export_typescript` de
    /// `../interactive/event.sdd` porte exactement les colonnes du `Model` — chaque clé du JSON
    /// du `Model` sérialisé est une propriété, et aucune propriété n'est en trop — avec `number`
    /// pour les entiers, `string` pour la date et la colonne JSON typée par le type de
    /// `lymaille` correspondant. La sérialisation du `Model` n'a pas changé (mêmes clés, mêmes
    /// valeurs : le JSON est celui des colonnes, vérifié par l'égalité des ensembles).
    #[test]
    fn lexport_typescript_du_modele_correspond_a_la_serialisation() {
        let dir = crate::lymaille::interactive::event::tests::scratch_dir("export-catalog_context");
        let rendu = crate::lymaille::interactive::event::export_typescript(&dir)
            .expect("l'export TypeScript réussit");
        let chemin = dir.join("catalog_context.ts");
        assert!(
            rendu.contains(&chemin),
            "`catalog_context.ts` est dans les chemins rendus"
        );
        let texte = std::fs::read_to_string(&chemin).expect("`catalog_context.ts` s'écrit et se relit");
        assert!(
            texte.contains("export type CatalogContextModel = {"),
            "la déclaration porte le nom distinct `CatalogContextModel`, pas le simple `Model` : {texte}"
        );
        let modele = modele("c1");
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
            type_de("context"),
            "ToolsetContext",
            "colonne context typée ToolsetContext dans la déclaration"
        );
        assert_eq!(
            type_de("context_type"),
            "string | null",
            "colonne context_type typée string | null dans la déclaration"
        );
        std::fs::remove_dir_all(&dir).expect("le répertoire de test se nettoie");
    }
}
