//! Entité `lymaille_catalog_context_types` (feature `lymaille`) — contrat porté par
//! `./context_type.sdd`.
//!
//! Table `miryad_lymaille_catalog_context_types` : les types de contexte du catalogue, administrés
//! par l'admin par le CRUD REST et MCP générique. Un `lymaille::ContextType` est un schéma JSON
//! versionné des paramètres et des rôles MCP d'un environnement, vocabulaire partagé par les
//! auteurs de toolsets (DSD `DEC-20`). La colonne JSON (DSD `DEC-18` : `active` pour la version
//! active, `draft` pour le brouillon de la session de configuration) est la source de vérité ;
//! `name` et `version` en sont des copies dérivées par les hooks. Le nom suit la grammaire de
//! `lymaille` 0.2.0 (`[A-Za-z0-9_-]+`, majuscules et `_` admis — ticket amont #5) et porte le
//! préfixe `G_` du catalogue (arbitrage du 2026-11-04 renversé le 2026-11-05 : la porte `classify`
//! redevient faisable). Nature et agent par défaut sont portés par le `ContextType` amont
//! (`session_kind`, `default_agent`) dans `active`/`draft` : les colonnes miryad-core sont
//! `allowed_agents` et, depuis l'amendement du 2026-11-11 (DSD `scripts-hote` `DEC-2`),
//! `script_grants` (`./context_type.sdd`).

use sea_orm::ActiveValue;
use sea_orm::entity::prelude::*;
use sea_orm::sea_query::{Alias, Expr, ExprTrait, Query};
use serde::Deserialize;
use serde::Serialize;

use crate::auth::AuthPrincipal;
use crate::lymaille::host_module;
use crate::lymaille::identity;
use crate::resource::AccessPolicy;
use crate::resource::HookError;
use crate::resource::MiryadResource;

/// Table des toolsets du catalogue, relue par la connexion du hook pour contrôler l'existence des
/// toolsets accordés aux scripts (`./context_type.sdd`, amendement du 2026-11-11, DSD
/// `scripts-hote` `DEC-2` ; table de `../migration.rs`, motif des constantes de
/// `../interactive/activation.rs`).
const CATALOG_TOOLSETS_TABLE: &str = "miryad_lymaille_catalog_toolsets";

/// Texte fixe du refus de forme de `script_grants` : objet à exactement les deux clés `toolsets`
/// et `modules`, tableaux de chaînes sans doublon (`Must`, amendement du 2026-11-11).
const GRANTS_FORME: &str =
    "context type script grants must be an object with `toolsets` and `modules` arrays of unique strings";

/// Type de contexte du catalogue persisté — la table `miryad_lymaille_catalog_context_types`
/// posée par `../migration.rs` (`m20261104_000001_lymaille_lot2`), colonnes du contrat `Must` de
/// `./context_type.sdd`.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "miryad_lymaille_catalog_context_types")]
pub struct Model {
    /// Clé primaire entière auto-incrémentée (convention `miryad_*` de la crate).
    #[sea_orm(primary_key)]
    pub id: i32,
    /// Identifiant du type de contexte (grammaire `lymaille` 0.2.0 `[A-Za-z0-9_-]+`, préfixe `G_`
    /// du catalogue — arbitrage du 2026-11-05), copie dérivée du `name` de ses versions par les
    /// hooks — libellé de la ressource, unique au catalogue (contrainte de la migration).
    pub name: String,
    /// Version sémantique de la version active (à défaut du brouillon), copie dérivée par les
    /// hooks.
    pub version: String,
    /// Ensemble des agents autorisés à l'ouverture, tableau JSON d'identifiants (DSD `DEC-28`) :
    /// colonne miryad-core des champs du type — depuis `lymaille` 0.2.0 le
    /// `ContextType` porte lui-même `default_agent` et `session_kind`, source de vérité unique
    /// dans `active`/`draft`. L'agent par défaut doit être membre de cet ensemble ; un agent
    /// explicite hors de l'ensemble est refusé à l'ouverture (`mcp_access`/`assemble`, hors de
    /// cette entité).
    pub allowed_agents: serde_json::Value,
    /// Droits accordés aux scripts de ce type, objet `{"toolsets":[…],"modules":[…]}` (DSD
    /// `scripts-hote` `DEC-2`, amendement du 2026-11-11) : toolsets du catalogue (`G_…`) et noms
    /// de modules d'hôte que le `script_runner` peut monter. Validé en forme par les hooks — les
    /// toolsets sont relus en base par la connexion du hook, les noms de module ne sont PAS
    /// vérifiés ici (le registre `HostModules` est hors de portée d'un hook, la vérification a
    /// lieu à l'exécution, `../script_runner.sdd`).
    pub script_grants: serde_json::Value,
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

    /// La version se désérialise en `lymaille::ContextType` — le constructeur compile le schéma
    /// et vérifie la grammaire du nom (`[A-Za-z0-9_-]+` de `lymaille` 0.2.0), un schéma invalide
    /// est donc refusé ici. La cause est la variante typée de `lymaille` rendue par ses affichages
    /// fixes, qui ne citent jamais le schéma (`Must not`).
    fn charger(brute: &serde_json::Value) -> Result<::lymaille::ContextType, HookError> {
        serde_json::from_value(brute.clone())
            .map_err(|cause| HookError::new(format!("invalid context type: {cause}")))
    }

    /// Champ texte lu dans la forme sérialisée : `ContextType` ne rend `name` et `version` que
    /// partiellement (accesseurs `default_agent`/`session_kind` depuis `lymaille` 0.2.0) — la
    /// colonne `name` est la copie de la clé de l'objet JSON, source de vérité, comme dans
    /// `../store/toolset.rs`. Le refus n'est qu'un garde-fou sans `unwrap`.
    fn champ_texte(brute: &serde_json::Value, champ: &str) -> Result<String, HookError> {
        brute
            .get(champ)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| HookError::new(format!("invalid context type: no `{champ}` in the stored form")))
    }

    /// Porte `DEC-28` de la colonne restante : `allowed_agents` posé (`ActiveValue::NotSet` compte
    /// comme absent et refuse) et tableau JSON de chaînes — les identifiants nus, jamais un objet
    /// ni un mixte avec des nombres. Rend la liste pour le contrôle d'appartenance de l'agent par
    /// défaut (`./context_type.sdd`, `Must`).
    fn liste_autorisee(active: &ActiveModel) -> Result<Vec<String>, HookError> {
        let brute = match &active.allowed_agents {
            ActiveValue::Set(agents) | ActiveValue::Unchanged(agents) => agents,
            ActiveValue::NotSet => {
                return Err(HookError::new(
                    "context type allowed agents must be an array of agent names",
                ));
            }
        };
        brute
            .as_array()
            .filter(|agents| agents.iter().all(serde_json::Value::is_string))
            .map(|agents| {
                agents
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .ok_or_else(|| HookError::new("context type allowed agents must be an array of agent names"))
    }

    /// Liste d'un tableau JSON de chaînes, sans doublon (`Must` de la porte `script_grants`,
    /// amendement du 2026-11-11) : ni objet, ni nombre dans le tableau, ni deux occurrences de la
    /// même chaîne.
    fn liste_sans_doublon(valeur: &serde_json::Value) -> Result<Vec<String>, HookError> {
        let tableaux = valeur
            .as_array()
            .ok_or_else(|| HookError::new(GRANTS_FORME.to_string()))?;
        let mut rendues: Vec<String> = Vec::with_capacity(tableaux.len());
        for entree in tableaux {
            let chaine = entree
                .as_str()
                .ok_or_else(|| HookError::new(GRANTS_FORME.to_string()))?;
            if rendues.iter().any(|deja| deja == chaine) {
                return Err(HookError::new(
                    "context type script grants must not contain duplicates",
                ));
            }
            rendues.push(chaine.to_owned());
        }
        Ok(rendues)
    }

    /// Porte `script_grants` (DSD `scripts-hote` `DEC-2`, amendement du 2026-11-11) : posée
    /// (`ActiveValue::NotSet` compte comme absent et refuse) et objet à **exactement** les deux
    /// clés `toolsets` et `modules`, chacune un tableau de chaînes sans doublon. Rend les deux
    /// listes pour les portes d'existence et de grammaire qui suivent.
    fn grants_script(active: &ActiveModel) -> Result<(Vec<String>, Vec<String>), HookError> {
        let brute = match &active.script_grants {
            ActiveValue::Set(grants) | ActiveValue::Unchanged(grants) => grants,
            ActiveValue::NotSet => {
                return Err(HookError::new(GRANTS_FORME.to_string()));
            }
        };
        let Some(cartes) = brute.as_object() else {
            return Err(HookError::new(GRANTS_FORME.to_string()));
        };
        if cartes.len() != 2 || !cartes.contains_key("toolsets") || !cartes.contains_key("modules") {
            return Err(HookError::new(GRANTS_FORME.to_string()));
        }
        let toolsets = Self::liste_sans_doublon(
            cartes
                .get("toolsets")
                .ok_or_else(|| HookError::new(GRANTS_FORME.to_string()))?,
        )?;
        let modules = Self::liste_sans_doublon(
            cartes
                .get("modules")
                .ok_or_else(|| HookError::new(GRANTS_FORME.to_string()))?,
        )?;
        Ok((toolsets, modules))
    }

    /// Relecture d'existence d'un toolset du catalogue par la **connexion du hook** (`Must`,
    /// amendement du 2026-11-11) : une ligne de `miryad_lymaille_catalog_toolsets` porte le
    /// `toolset_id` (`UNIQUE` par la migration — « n'importe quelle ligne », sans filtre sur
    /// `active`). Une panne de lecture est un refus fermé au texte fixe du `Must`.
    async fn toolset_existe<C: sea_orm::ConnectionTrait>(
        db: &C,
        toolset_id: &str,
    ) -> Result<bool, HookError> {
        let mut select = Query::select();
        select
            .columns([Alias::new("toolset_id")])
            .from(Alias::new(CATALOG_TOOLSETS_TABLE))
            .and_where(Expr::col(Alias::new("toolset_id")).eq(Expr::val(toolset_id)));
        db.query_one(&select)
            .await
            .map(|ligne| ligne.is_some())
            .map_err(|_| HookError::new("authorization check unavailable"))
    }

    /// Porte commune de `before_create` (`existing` absent) et `before_update`, dans l'ordre du
    /// `Must` de `./context_type.sdd`, interrompue au premier échec : au moins une version ; la
    /// porte `DEC-28` (`allowed_agents` posé et tableau de chaînes) ; `classify(name) == Catalog`
    /// (préfixe `G_`, comme `./agent.sdd` — arbitrage du 2026-11-05, la grammaire amont 0.2.0
    /// rend les noms `G_…` désérialisables ; tout autre rendu de `classify`, y compris une
    /// erreur, est le même refus) ; la porte `script_grants` (amendement du 2026-11-11, DSD
    /// `scripts-hote` `DEC-2` : posée et bien formée, chaque toolset de classe `Catalog` **et**
    /// existant dans la table relue par la connexion du hook — panne de lecture : refus fermé au
    /// texte fixe, chaque nom de module un identifiant valide, existence non vérifiée) ; chaque
    /// version non nulle désérialisée en `lymaille::ContextType` (le constructeur compile le
    /// schéma et vérifie la grammaire du nom) et, si le type porte un `default_agent`, appartenance
    /// à l'ensemble autorisé par comparaison de chaînes exacte (un type sans agent par défaut est
    /// admis : l'ouverture exigera alors un agent explicite) ; sur une mise à jour `name` ne change
    /// pas. `name` et `version` (celle de l'active, à défaut du brouillon) sont toujours réécrits ;
    /// `script_grants` traverse l'`ActiveModel` inchangé, comme `allowed_agents`. L'existence de
    /// l'agent dans le catalogue et la cohérence de la nature (`session_kind` ↔ nature de l'agent)
    /// sont contrôlées à l'ouverture par `lymaille::resolve_start` (`../interactive/manager.sdd`),
    /// pas ici.
    async fn valider_et_deriver<C: sea_orm::ConnectionTrait>(
        db: &C,
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
        let autorises = Self::liste_autorisee(&active)?;
        let brute = brute_active
            .or(brute_draft)
            .ok_or_else(|| HookError::new("a context type needs an active version or a draft"))?;
        let name = Self::champ_texte(brute, "name")?;
        if !matches!(identity::classify(&name), Ok(identity::IdKind::Catalog)) {
            return Err(HookError::new("context type name must start with G_ (catalog)"));
        }
        let (toolsets, modules) = Self::grants_script(&active)?;
        for toolset in toolsets {
            if !matches!(identity::classify(&toolset), Ok(identity::IdKind::Catalog)) {
                return Err(HookError::new(
                    "context type script grant toolset must start with G_ (catalog)",
                ));
            }
            if !Self::toolset_existe(db, &toolset).await? {
                return Err(HookError::new(format!(
                    "context type script grant toolset `{toolset}` does not exist"
                )));
            }
        }
        for module in modules {
            // `is_module_name` de `../host_module.sdd` : seule copie de la grammaire ASCII ; pas de
            // vérification d'existence — le registre `HostModules` est hors de portée d'un hook, elle
            // a lieu à l'exécution par `../script_runner.sdd` (DSD `scripts-hote` `DEC-2`).
            if !host_module::is_module_name(&module) {
                return Err(HookError::new(
                    "context type script grant module must be a valid identifier",
                ));
            }
        }
        for brute_version in brute_active.into_iter().chain(brute_draft) {
            let type_valide = Self::charger(brute_version)?;
            if type_valide
                .default_agent()
                .is_some_and(|defaut| !autorises.iter().any(|agent| agent == defaut))
            {
                return Err(HookError::new(
                    "context type default agent must be in the allowed agents",
                ));
            }
        }
        let version = Self::champ_texte(brute, "version")?;
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
    /// [`Entity::valider_et_deriver`]. Les toolsets de `script_grants` sont relus en existence par
    /// la connexion `db` (amendement du 2026-11-11, DSD `scripts-hote` `DEC-2`).
    fn before_create<C>(
        db: &C,
        active: Self::ActiveModel,
        _principal: &AuthPrincipal,
    ) -> impl std::future::Future<Output = Result<Self::ActiveModel, HookError>> + Send
    where
        C: sea_orm::ConnectionTrait,
    {
        Entity::valider_et_deriver(db, active, None)
    }

    /// Mêmes étapes que [`Self::before_create`], plus l'identité : un `name` qui change est
    /// refusé (`Must` de `./context_type.sdd`, précédent `./toolset.rs`).
    fn before_update<C>(
        db: &C,
        active: Self::ActiveModel,
        existing: &Self::Model,
        _principal: &AuthPrincipal,
    ) -> impl std::future::Future<Output = Result<Self::ActiveModel, HookError>> + Send
    where
        C: sea_orm::ConnectionTrait,
    {
        Entity::valider_et_deriver(db, active, Some(existing))
    }

    // `before_delete` volontairement non surdéclaré : aucun contrôle d'usage au lot 2, le défaut
    // identité du trait s'applique.
}

#[cfg(test)]
mod tests {
    use super::ActiveModel;

    /// Connexion simulée à file vide : suffisante pour les hooks dont les `script_grants` sont
    /// vides ou mal formés avant la relecture — ils n'émettent alors aucune requête. Une requête
    /// inattendue bute sur un `DbErr` de file vide et fait échouer le test.
    fn hook_db() -> sea_orm::DatabaseConnection {
        sea_orm::MockDatabase::new(sea_orm::DbBackend::Sqlite).into_connection()
    }
    use super::Entity;
    use super::Model;
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
    // `to_value`), seule la clé `name` est corrigée au nom voulu — la grammaire du constructeur
    // (`lymaille` 0.2.0, `toolsets/context_type.rs::is_valid_name`) accepte lettres ASCII,
    // chiffres, tirets et underscores : un nom `G_…` se désérialise, la porte `classify` du
    // catalogue est faisable (arbitrage du 2026-11-05, `./context_type.sdd`).

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

    /// Forme sérialisée d'un type nommant un agent par défaut `defaut` (champ du `ContextType`
    /// amont depuis `lymaille` 0.2.0, plus une colonne miryad).
    fn type_json_avec_defaut(name: &str, defaut: &str) -> serde_json::Value {
        let mut value = type_json(name, "1.0.0", json!({ "type": "object" }));
        let Some(map) = value.as_object_mut() else {
            panic!("la forme sérialisée d'un `ContextType` est un objet");
        };
        map.insert("default_agent".to_string(), json!(defaut));
        value
    }

    /// `ActiveModel` dans la forme qui atteint le hook : colonnes dérivées fausses (le hook doit
    /// les réécrire), `allowed_agents` et versions voulus, `script_grants` vide (les Scenarios qui
    /// portent sur cette colonne la remplacent), clé et horodatage non posés.
    fn active_avec_agents(
        allowed_agents: serde_json::Value,
        active: Option<serde_json::Value>,
        draft: Option<serde_json::Value>,
    ) -> ActiveModel {
        ActiveModel {
            id: NotSet,
            name: Set("faux-nom".to_string()),
            version: Set("0.0.0".to_string()),
            allowed_agents: Set(allowed_agents),
            script_grants: Set(grants_vide()),
            active: Set(active),
            draft: Set(draft),
            created_at: NotSet,
        }
    }

    /// Valeur par défaut de `script_grants` (celle que pose la migration) : droits vides, aucune
    /// porte de relecture de toolset n'est franchie, la connexion du hook n'émet aucune requête.
    fn grants_vide() -> serde_json::Value {
        serde_json::json!({"toolsets": [], "modules": []})
    }

    /// `ActiveModel` avec un ensemble autorisé ordinaire non vide, pour que les Scenarios
    /// atteignent leurs propres portes.
    fn active_avec(active: Option<serde_json::Value>, draft: Option<serde_json::Value>) -> ActiveModel {
        active_avec_agents(json!(["G_triage"]), active, draft)
    }

    /// Scenario « un type valide du catalogue est accepté, ses colonnes dérivées » : nom `G_…`
    /// de la grammaire `lymaille` 0.2.0, porte `classify` franchie.
    #[tokio::test]
    async fn before_create_accepte_un_type_valide_du_catalogue_et_derive_les_colonnes() {
        let rendu = Entity::before_create(&hook_db(),
            active_avec(
                Some(type_json(
                    "G_cluster",
                    "1.0.0",
                    json!({"type": "object", "properties": {"cluster": {"type": "string"}}, "required": ["cluster"]}),
                )),
                None,
            ),
            &principal(),
        ).await
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
    #[tokio::test]
    async fn before_create_refuse_un_type_sans_version() {
        let rendus = vec![
            Entity::before_create(&hook_db(), active_avec(None, None), &principal()).await,
            Entity::before_create(
                &hook_db(),
                active_avec(Some(serde_json::Value::Null), Some(serde_json::Value::Null)),
                &principal(),
            )
            .await,
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
    #[tokio::test]
    async fn before_create_refuse_un_schema_invalide_sans_le_citer() {
        let marque = "SCHEMA-A-NE-JAMAIS-CITER-7c21";
        let rendu = Entity::before_create(
            &hook_db(),
            active_avec(
                Some(type_json("G_cluster", "1.0.0", json!({"type": marque}))),
                None,
            ),
            &principal(),
        )
        .await;
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

    /// Scenario (alignement `lymaille` 0.2.0) « un type sans agent par défaut est admis » : le
    /// JSON ne porte aucun `default_agent`, l'ensemble est non vide — l'ouverture exigera alors
    /// un agent explicite (`../interactive/manager.sdd`). La nature est `session_kind` amont,
    /// validée à la désérialisation : aucune porte « nature » ici.
    #[tokio::test]
    async fn before_create_accepte_un_type_sans_agent_par_defaut() {
        let rendu = Entity::before_create(
            &hook_db(),
            active_avec_agents(
                json!(["G_alpha"]),
                Some(type_json("G_cluster", "1.0.0", json!({"type": "object"}))),
                None,
            ),
            &principal(),
        )
        .await
        .expect("un type sans agent par défaut est admis");
        assert_eq!(rendu.name, Set("G_cluster".to_string()));
    }

    /// Scenario (alignement `lymaille` 0.2.0) « un nom sans préfixe G_ est refusé » : la porte
    /// `classify(name) == Catalog` est rétablie (arbitrage du 2026-11-05 — la grammaire amont
    /// `[A-Za-z0-9_-]+` de `lymaille` 0.2.0 rend les noms `G_…` désérialisables).
    #[tokio::test]
    async fn before_create_refuse_un_nom_sans_prefixe_g() {
        let rendu = Entity::before_create(
            &hook_db(),
            active_avec(
                Some(type_json("cluster", "1.0.0", json!({"type": "object"}))),
                None,
            ),
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
                    erreur.message.contains("G_"),
                    "le refus cite la règle du préfixe : {erreur:?}"
                );
            }
            Ok(_) => panic!("un type de contexte sans préfixe `G_` doit être refusé"),
        }
    }

    /// Scenario (alignement `lymaille` 0.2.0) « `allowed_agents` absent ou mal formé est refusé »
    /// : `ActiveValue::NotSet` compte comme absent et refuse ; un objet, ou un tableau contenant
    /// un nombre, ne sont pas un tableau de chaînes.
    #[tokio::test]
    async fn before_create_refuse_allowed_agents_absent_ou_mal_forme() {
        let valide = type_json("G_cluster", "1.0.0", json!({"type": "object"}));
        let mut mal_formes = vec![
            (NotSet, "absent (`NotSet`)"),
            (Set(json!({"0": "G_triage"})), "objet"),
            (Set(json!(["G_triage", 1])), "tableau contenant un nombre"),
        ];
        for (valeur, cause) in mal_formes.drain(..) {
            let mut active = active_avec(Some(valide.clone()), None);
            active.allowed_agents = valeur;
            let rendu = Entity::before_create(&hook_db(), active, &principal()).await;
            match rendu {
                Err(erreur) => {
                    assert!(
                        erreur.code.is_none(),
                        "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                    );
                    assert!(
                        erreur.message.contains("allowed agents"),
                        "le refus motive l'ensemble mal formé ({cause}) : {erreur:?}"
                    );
                }
                Ok(_) => panic!("un `allowed_agents` {cause} doit être refusé"),
            }
        }
    }

    /// Scenario (alignement `lymaille` 0.2.0) « un agent par défaut hors de l'ensemble autorisé
    /// est refusé », lu **dans le JSON** : la colonne `default_agent` n'existe plus, c'est le
    /// `ContextType` amont qui porte l'agent, comparé par chaînes exactes à `allowed_agents`.
    #[tokio::test]
    async fn before_create_refuse_un_agent_par_defaut_hors_ensemble_lu_dans_le_json() {
        let rendu = Entity::before_create(
            &hook_db(),
            active_avec_agents(
                json!(["G_beta"]),
                Some(type_json_avec_defaut("G_cluster", "G_alpha")),
                None,
            ),
            &principal(),
        )
        .await;
        match rendu {
            Err(erreur) => {
                assert!(
                    erreur.code.is_none(),
                    "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                );
                assert_eq!(
                    erreur.message, "context type default agent must be in the allowed agents",
                    "l'agent par défaut du JSON doit être membre de l'ensemble autorisé"
                );
            }
            Ok(_) => panic!("un `default_agent` du JSON hors de l'ensemble autorisé doit être refusé"),
        }
    }

    /// Scenario (alignement `lymaille` 0.2.0) « un agent par défaut membre de l'ensemble autorisé
    /// est accepté », lu dans le JSON.
    #[tokio::test]
    async fn before_create_accepte_un_agent_par_defaut_membre_de_l_ensemble() {
        Entity::before_create(
            &hook_db(),
            active_avec_agents(
                json!(["G_alpha", "G_beta"]),
                Some(type_json_avec_defaut("G_cluster", "G_alpha")),
                None,
            ),
            &principal(),
        )
        .await
        .expect("un agent par défaut membre de l'ensemble est accepté");
    }

    /// Scenario « le nom ne change pas à la mise à jour » : une ligne `G_cluster`, un
    /// `ActiveModel` dont le JSON porte `G_cluster2` — `before_update` refuse, le message cite la
    /// règle ; aucun code `MRD-*` sur un `HookError`.
    #[tokio::test]
    async fn before_update_refuse_un_changement_de_nom() {
        let existing = Model {
            id: 1,
            name: "G_cluster".to_string(),
            version: "1.0.0".to_string(),
            allowed_agents: json!(["G_triage"]),
            script_grants: grants_vide(),
            active: Some(type_json("G_cluster", "1.0.0", json!({"type": "object"}))),
            draft: None,
            created_at: chrono::Utc::now(),
        };
        let rendu = Entity::before_update(
            &hook_db(),
            active_avec(
                Some(type_json("G_cluster2", "1.1.0", json!({"type": "object"}))),
                None,
            ),
            &existing,
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
                    erreur.message.contains("name cannot change"),
                    "le refus cite la règle : {erreur:?}"
                );
            }
            Ok(_) => panic!("un changement de nom à la mise à jour doit être refusé"),
        }
    }

    /// Connexion simulée scriptant une réponse de relecture : une liste de jeux de lignes, vidée
    /// dans l'ordre des requêtes émises. Une requête de trop bute sur un `DbErr` de file vide —
    /// c'est aussi cela que le test verrouille quand il compte les requêtes.
    fn hook_db_avec_lignes(
        lignes: Vec<Vec<crate::lymaille::definitions::toolset::Model>>,
    ) -> sea_orm::DatabaseConnection {
        let mut mock = sea_orm::MockDatabase::new(sea_orm::DbBackend::Sqlite);
        for jeu in lignes {
            mock = mock.append_query_results(vec![jeu]);
        }
        mock.into_connection()
    }

    /// Scenario « `script_grants` mal formé ou référençant un toolset absent est refusé » (portion
    /// refus) : absent (`NotSet`), objet à clé manquante, doublon dans `modules`, toolset `G_…`
    /// absent de la table relue par la connexion du hook, et nom de module hors grammaire Rhai
    /// rendent chacun un `HookError` **sans code**, avant toute écriture. S'y ajoutent deux cas du
    /// `Must` que le Scenario implique : un toolset qui n'est pas de classe `Catalog` (refus sans
    /// relecture) et une panne de lecture (texte fixe `authorization check unavailable`).
    #[tokio::test]
    async fn before_create_refuse_script_grants_mal_forme_ou_toolset_absent() {
        let valide = type_json("G_cluster", "1.0.0", json!({"type": "object"}));
        // File vide : toute requête émise sur ces cas buterait sur un `DbErr`, alors qu'aucune
        // relecture n'est attendue (la forme refuse avant, ou l'identifiant n'est pas du catalogue).
        let vide = || sea_orm::MockDatabase::new(sea_orm::DbBackend::Sqlite).into_connection();
        // Une relecture scriptée, rendant zéro ligne : le toolset `G_inconnu` n'existe pas.
        let absent = || hook_db_avec_lignes(vec![Vec::new()]);
        let cas: Vec<(sea_orm::ActiveValue<serde_json::Value>, _, &str)> = vec![
            (NotSet, vide(), "absent (`NotSet`)"),
            (
                Set(json!({"toolsets": []})),
                vide(),
                "objet à clé `modules` manquante",
            ),
            (
                Set(json!({"toolsets": [], "modules": ["a", "a"]})),
                vide(),
                "doublon dans `modules`",
            ),
            (
                Set(json!({"toolsets": ["G_inconnu"], "modules": []})),
                absent(),
                "toolset absent du catalogue",
            ),
            (
                Set(json!({"toolsets": [], "modules": ["bad name"]})),
                vide(),
                "nom de module hors grammaire Rhai",
            ),
            (
                Set(json!({"toolsets": ["prive"], "modules": []})),
                vide(),
                "toolset hors catalogue (pas de classe Catalog)",
            ),
        ];
        for (valeur, db, cause) in cas {
            let mut active = active_avec(Some(valide.clone()), None);
            active.script_grants = valeur;
            let rendu = Entity::before_create(&db, active, &principal()).await;
            match rendu {
                Err(erreur) => {
                    assert!(
                        erreur.code.is_none(),
                        "aucun code `MRD-*` sur un `HookError` ({cause}) : {erreur:?}"
                    );
                    assert!(!erreur.message.is_empty(), "le refus motive la règle ({cause})");
                }
                Ok(_) => panic!("un `script_grants` {cause} doit être refusé"),
            }
        }
        // `Must` : la panne de lecture de la table des toolsets rend le texte fixe, sans code.
        let mut active = active_avec(Some(valide), None);
        active.script_grants = Set(json!({"toolsets": ["G_existant"], "modules": []}));
        let rendu = Entity::before_create(&vide(), active, &principal()).await;
        match rendu {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "aucun code `MRD-*` : {erreur:?}");
                assert_eq!(
                    erreur.message, "authorization check unavailable",
                    "la panne de lecture rend le texte fixe du `Must`"
                );
            }
            Ok(_) => panic!("une panne de relecture du catalogue doit refuser"),
        }
    }

    /// Scenario « `script_grants` mal formé ou référençant un toolset absent est refusé » (portion
    /// acceptation) : `{"toolsets":["G_existant"],"modules":["view"]}` est accepté **sans** que
    /// `view` soit cherché au registre (`HostModules` est hors de portée d'un hook, DSD
    /// `scripts-hote` `DEC-2` précision du 2026-11-11). Une seule requête est émise — la relecture
    /// d'existence du toolset — et la colonne traverse l'`ActiveModel` inchangée.
    #[tokio::test]
    async fn before_create_accepte_des_grants_d_un_toolset_existant_sans_chercher_les_modules() {
        let grants = json!({"toolsets": ["G_existant"], "modules": ["view"]});
        let db = hook_db_avec_lignes(vec![vec![crate::lymaille::definitions::toolset::Model {
            id: 1,
            toolset_id: "G_existant".to_string(),
            version: "1.0.0".to_string(),
            active: None,
            draft: None,
            created_at: chrono::Utc::now(),
        }]]);
        let mut active = active_avec(
            Some(type_json("G_cluster", "1.0.0", json!({"type": "object"}))),
            None,
        );
        active.script_grants = Set(grants.clone());
        let rendu = Entity::before_create(&db, active, &principal())
            .await
            .expect("des grants dont le toolset existe sont acceptés, le nom de module n'est pas vérifié");
        assert_eq!(rendu.name, Set("G_cluster".to_string()));
        assert_eq!(
            match &rendu.script_grants {
                Set(valeur) | sea_orm::ActiveValue::Unchanged(valeur) => Some(valeur.clone()),
                sea_orm::ActiveValue::NotSet => None,
            },
            Some(grants),
            "la colonne script_grants n'est pas réécrite par le hook"
        );
        let requetes: Vec<String> = db
            .into_transaction_log()
            .iter()
            .flat_map(sea_orm::Transaction::statements)
            .map(std::string::ToString::to_string)
            .collect();
        assert_eq!(
            requetes.len(),
            1,
            "une seule requête — la relecture du toolset, jamais le registre des modules : {requetes:?}"
        );
        assert!(
            requetes
                .first()
                .is_some_and(|stmt| stmt.contains("miryad_lymaille_catalog_toolsets")),
            "la requête porte sur la table des toolsets du catalogue : {requetes:?}"
        );
    }
}
