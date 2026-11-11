//! Exécuteur de script Rhai sans boucle d'inférence (`src/lymaille/script_runner.sdd`).
//!
//! Fonction de bibliothèque seule : aucune route, aucun outil MCP, aucun cache. Le script est fourni
//! par l'hôte (`DEC-5`) et s'exécute sous l'identité de celui qui le regarde, à droits bornés par le
//! **type du contexte** (colonne `script_grants`, `lymaille#DEC-28`) intersectée avec le principal
//! relu à chaque appel de module (`FR-4`, `FR-5`). Les modules montés sont exactement ceux de
//! l'assemblage de session restreints aux droits : `resources` (avec écritures, sans journal), les
//! modules d'hôte nommés et les rôles MCP des toolsets accordés — ni les modules natifs de
//! `lymaille`, ni `fs`/`shell`, ni la configuration (DSD `DEC-3` révisé, `OI-5`). Le moteur est un
//! `rhai::Engine` dédié (`DEC-6`) qui évalue sur un thread dédié — le pont
//! `vynil_core::rt::block_on` des modules enrobés l'exige — sous plafonds de durée, d'opérations et
//! de taille (`FR-7`).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rhai::{Dynamic, Engine, EvalAltResult};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde_json::Value as JsonValue;
use zeroize::Zeroizing;

use ::lymaille::{
    Clock, ContextStore as _, EventSink, McpEnv, McpRetryPolicy, NullSink, SessionId, SystemClock, Toolset,
    ToolsetContext, ToolsetStore as _,
};

use crate::lymaille::definitions::{catalog_context, context_type, user_context};
use crate::lymaille::error::LymailleError;
use crate::lymaille::host::SharedHost;
use crate::lymaille::host_module::{HostModules, HostSelect, mount_host_modules};
use crate::lymaille::identity::{CallerPrincipal, IdKind, caller_of, classify};
use crate::lymaille::install_crypto_provider;
use crate::lymaille::interactive::assemble::{role_requirements, toolset_context_roles};
use crate::lymaille::mcp_mount::{McpConnectorFactory, MountDeps, RmcpConnectorFactory, mount_role};
use crate::lymaille::resource_module::{ResourceRegistry, dynamic_to_json, resources_module};
use crate::lymaille::secret::SecretResolver;
use crate::lymaille::store::LymailleStores;
use crate::lymaille::store::db_text;

/// Limites d'une exécution (`Must`, arbitrage du 2026-11-11, DSD `scripts-hote#FR-7`) : durée,
/// opérations Rhai et taille sérialisée du résultat. `Default` = 5 s, 1 000 000 opérations,
/// 256 KiB ; l'application les relève ou les resserre. Un dépassement est une erreur typée,
/// jamais une troncature silencieuse (`AC-5`).
#[derive(Debug, Clone)]
pub struct ScriptLimits {
    /// Durée maximale de l'évaluation : au-delà, la progression coupe (`ScriptTimeout`).
    pub max_duration: Duration,
    /// Plafond d'opérations Rhai (`ScriptOperationLimit`).
    pub max_operations: u64,
    /// Taille sérialisée maximale du résultat (`ScriptResultTooLarge`) ; ramenée aussi aux
    /// plafonds de chaîne, de tableau et de table de l'`Engine`.
    pub max_result_bytes: usize,
}

impl Default for ScriptLimits {
    fn default() -> Self {
        Self {
            max_duration: Duration::from_secs(5),
            max_operations: 1_000_000,
            max_result_bytes: 256 * 1024,
        }
    }
}

/// Ce que l'application fournit à l'exécuteur (`Must`) : ce que regroupe `AssembleDeps` pour le
/// montage — connexion, registres, hôte optionnel, secrets, coutume des connecteurs MCP et
/// horloge. Aucun hub d'événements : le script n'émet pas d'événements de session, les modules
/// MCP montés versent dans un puits (`NullSink`).
pub struct ScriptDeps {
    /// Connexion à la base de l'application (identité, type du contexte, module des ressources).
    pub db: DatabaseConnection,
    /// Registre des entités lues et écrites par les scripts (`../resource_module.rs`).
    pub readers: Arc<ResourceRegistry>,
    /// Registre des modules d'hôte enregistrés (`../host_module.rs`) : seuls ceux que
    /// `script_grants.modules` nomme sont montés. `Default` est vide.
    pub host_modules: Arc<HostModules>,
    /// Résolveur des références d'authentification des serveurs (`../secret.rs`).
    pub secrets: Arc<dyn SecretResolver>,
    /// Horloge des environnements MCP montés.
    pub clock: Arc<dyn Clock>,
    /// Hôte applicatif optionnel (DSD `lymaille#DEC-27`) : contextes `H_…` et décisions MCP.
    pub host: Option<SharedHost>,
    /// Couture des connecteurs MCP : [`RmcpConnectorFactory`] en production. Traité `pub(crate)`
    /// comme dans `AssembleDeps` : la couture n'est pas une surface publique, les tests injectent
    /// un espion.
    pub(crate) mcp: Arc<dyn McpConnectorFactory>,
}

impl ScriptDeps {
    /// Construit les dépendances de production : fabrique `rmcp` et horloge système, sans hôte
    /// (l'application ajoute `host` par construction des champs, comme pour `AssembleDeps`).
    #[must_use]
    pub fn new(
        db: DatabaseConnection,
        readers: Arc<ResourceRegistry>,
        host_modules: Arc<HostModules>,
        secrets: Arc<dyn SecretResolver>,
    ) -> Self {
        Self {
            db,
            readers,
            host_modules,
            secrets,
            clock: Arc::new(SystemClock::new()),
            host: None,
            mcp: Arc::new(RmcpConnectorFactory),
        }
    }
}

/// Exécuteur de scripts d'hôte (`FR-4`) : sans état, sans cache — même texte et deux sujets
/// donnent chacun le résultat de **leur** principal.
pub struct ScriptRunner {
    deps: ScriptDeps,
}

impl ScriptRunner {
    /// L'exécuteur branché sur les dépendances de montage.
    #[must_use]
    pub fn new(deps: ScriptDeps) -> Self {
        Self { deps }
    }

    /// Stores `SeaORM` liés au `subject` (base, hôte éventuel) — la même fabrique qu'à l'assemblage.
    fn stores_for(&self, subject: &str) -> LymailleStores {
        match &self.deps.host {
            Some(host) => {
                LymailleStores::new(self.deps.db.clone(), subject.to_owned()).with_host(Arc::clone(host))
            }
            None => LymailleStores::new(self.deps.db.clone(), subject.to_owned()),
        }
    }

    /// Exécute le script `source` sous l'identité du `subject`, dans le contexte `context`, sous
    /// `limits`. Le `token` n'est transmis qu'à `build_auth` (auth `forward`) et disparaît avec
    /// l'appel.
    ///
    /// # Errors
    ///
    /// - `LymailleError::UnknownSubject` : `subject` sans compte local.
    /// - `LymailleError::Forbidden` : contexte ou type absent, sans version active, ou
    ///   `script_grants` illisible (`"script grants unavailable"`) ; rôle **requis** refusé
    ///   (`"mcp server unavailable"`).
    /// - `LymailleError::ScriptTimeout` / `ScriptOperationLimit` / `ScriptResultTooLarge` :
    ///   dépassements typés (`AC-5`).
    /// - `LymailleError::ScriptFailed` : le script a échoué — message Rhai sans le source, sans
    ///   ligne, sans jeton ni URL.
    /// - `LymailleError::Store` / `Config` / `InvalidDefinition` : pannes des lectures de
    ///   résolution et du montage, comme à l'assemblage.
    pub async fn run(
        &self,
        context: &str,
        subject: &str,
        source: &str,
        limits: &ScriptLimits,
        token: Option<Zeroizing<String>>,
    ) -> Result<JsonValue, LymailleError> {
        install_crypto_provider();
        // Étape 1 : le principal, relu à l'appel (`../identity.rs`). Les modules relisent le leur
        // à chaque appel (enrobage `host_module`, `resources_module`) : aucun principal en cache.
        let caller = caller_of(&self.deps.db, subject).await?;
        let stores = self.stores_for(subject);
        // Étape 2 : contexte puis type, comme à l'étape 4 de `assemble_session` ; la version
        // active du type fournit `script_grants` (échec fermé : aucun droit sans type).
        let (context_object, grants) = resolve_grants(&self.deps.db, &stores, &caller, context).await?;
        // Étape 3 : les modules montés, et seulement ceux-là.
        let mut engine = Engine::new();
        // Script : écritures ouvertes (`DEC-3` révisé), `scope = None` — aucun journal (`./resource_module.sdd`).
        let resources = resources_module(
            Arc::clone(&self.deps.readers),
            self.deps.db.clone(),
            subject.to_owned(),
            None,
        );
        let _registered = engine.register_static_module("resources", Arc::new(resources));
        // Noms déjà accordés, pour la garde de collision de `../mcp_mount.rs`.
        let mut disabled: Vec<String> = vec!["resources".to_owned()];
        for (name, module) in mount_host_modules(
            &self.deps.host_modules,
            self.deps.db.clone(),
            subject,
            HostSelect::Named(&grants.modules),
        ) {
            let _registered = engine.register_static_module(name, Arc::new(module));
            disabled.push((*name).to_owned());
        }
        let toolsets = load_granted_toolsets(&stores, &grants.toolsets).await?;
        self.mount_mcp_roles(McpMountArgs {
            engine: &mut engine,
            caller: &caller,
            context,
            context_object: &context_object,
            toolsets: &toolsets,
            token: token.as_ref(),
            disabled: &mut disabled,
        })
        .await?;
        // Étape 4 : plafonds de l'`Engine` dédié et évaluation sur un thread dédié (le pont
        // `block_on` des modules enrobés l'exige ; le runtime tokio n'est pas bloqué).
        engine.set_max_operations(limits.max_operations);
        engine.set_max_string_size(limits.max_result_bytes);
        engine.set_max_array_size(limits.max_result_bytes);
        engine.set_max_map_size(limits.max_result_bytes);
        let deadline = Instant::now()
            .checked_add(limits.max_duration)
            .unwrap_or_else(Instant::now);
        engine.on_progress(move |_operations: u64| {
            (Instant::now() >= deadline).then(|| Dynamic::from("script time limit reached"))
        });
        let script = source.to_owned();
        let evaluation = tokio::task::spawn_blocking(move || engine.eval(&script)).await;
        match evaluation {
            // Panic du thread d'évaluation (y compris d'une closure de module) : confiné, message
            // fixe, le thread appelant ne panique jamais.
            Err(panic) => {
                tracing::warn!("lymaille script runner: evaluation thread failed: {panic}");
                Err(LymailleError::ScriptFailed(
                    "script evaluation thread failed".to_owned(),
                ))
            }
            Ok(Ok(rendered)) => render_value(&rendered, limits),
            Ok(Err(error)) => Err(script_error(*error)),
        }
    }

    /// Montée des rôles MCP des toolsets accordés, **identique à l'étape 3 de
    /// `assemble_session`** : mêmes `role_requirements`, mêmes `toolset_context_roles`, même
    /// `mount_role`, même politique (requis refusé → échec ; optionnel refusé — message exact
    /// `"mcp server unavailable"` → omis ; autres erreurs → fatal). Le `Redactor` porte le jeton
    /// forward pour la traçabilité du refus ; le jeton n'est jamais monté ni tracé.
    async fn mount_mcp_roles(&self, args: McpMountArgs<'_>) -> Result<(), LymailleError> {
        let McpMountArgs {
            engine,
            caller,
            context,
            context_object,
            toolsets,
            token,
            disabled,
        } = args;
        let stores = self.stores_for(&caller.subject);
        let (required, optional) = role_requirements(toolsets, &stores).await?;
        let bindings = toolset_context_roles(context_object);
        let mut redactor = ::lymaille::Redactor::default();
        if let Some(secret) = token {
            // Une valeur trop courte pour être masquée est ignorée, comme à l'assemblage.
            let _added = redactor.add_secret(secret.to_string());
        }
        let redactor = Arc::new(redactor);
        let sink: Arc<dyn EventSink> = Arc::new(NullSink::new());
        let session = SessionId::new("lymaille-script")
            .ok_or_else(|| LymailleError::Config("the script session label must be non-empty".to_owned()))?;
        let mounts = MountDeps {
            db: &self.deps.db,
            secrets: &self.deps.secrets,
            host: self.deps.host.as_ref(),
            mcp: self.deps.mcp.as_ref(),
        };
        for role in required.iter().chain(optional.iter()) {
            let is_required = required.contains(role);
            let registered: Vec<&str> = disabled.iter().map(String::as_str).collect();
            let env = McpEnv::new(
                Arc::clone(&redactor),
                Arc::clone(&sink),
                Arc::clone(&self.deps.clock),
                session.clone(),
                McpRetryPolicy::default(),
            );
            match mount_role(
                &mounts,
                caller,
                context,
                role,
                bindings.get(role.as_str()).map(String::as_str),
                token,
                &registered,
                env,
            )
            .await
            {
                Ok(module) => {
                    let _registered =
                        engine.register_static_module(role.as_str(), Arc::clone(module.module()));
                    disabled.push(role.clone());
                }
                Err(LymailleError::Forbidden(message))
                    if !is_required && message == "mcp server unavailable" => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

/// Les droits du script : `script_grants` de la version **active** du type du contexte, jamais un
/// brouillon, et le contexte lui-même (pour les liaisons rôle → serveur).
struct Grants {
    toolsets: Vec<String>,
    modules: BTreeSet<String>,
}

/// Les paramètres de la montée MCP — groupés en structure privée pour rester sous la limite de
/// sept arguments du harnais (`tooling.sdd`), sans `allow` hors spec.
struct McpMountArgs<'a> {
    engine: &'a mut Engine,
    caller: &'a CallerPrincipal,
    context: &'a str,
    context_object: &'a ToolsetContext,
    toolsets: &'a [Toolset],
    token: Option<&'a Zeroizing<String>>,
    disabled: &'a mut Vec<String>,
}

/// Texte fixe de l'échec fermé : aucun droit sans type lisible (`Must`).
fn grants_unavailable() -> LymailleError {
    LymailleError::Forbidden("script grants unavailable".to_owned())
}

/// Erreur d'un store de définitions (`ToolsetsError`) — même traduction qu'à l'assemblage.
fn from_toolsets(error: ::lymaille::ToolsetsError) -> LymailleError {
    match error {
        ::lymaille::ToolsetsError::Store(text) => LymailleError::Store(text),
        other => LymailleError::InvalidDefinition(other.to_string()),
    }
}

/// Étape 2 : le contexte est résolu comme à l'étape 4 de `assemble_session` — privé de l'appelant,
/// `G_…` du catalogue ou `H_…` de l'hôte, jamais celui d'un autre (`SeaContextStore`) — puis sa
/// référence de type, lue sur la **ligne** de ce contexte (la colonne `context_type` de la table
/// d'origine ; `ToolsetContext` ne la porte pas). La version active du type doit exister et son
/// `script_grants` se lire : contexte ou type absent, sans active, ou grants illisible →
/// l'échec fermé du `Must`. Un contexte `H_…` résolu par l'hôte n'a pas de type stocké en base :
/// même échec fermé.
async fn resolve_grants(
    db: &DatabaseConnection,
    stores: &LymailleStores,
    caller: &CallerPrincipal,
    context: &str,
) -> Result<(ToolsetContext, Grants), LymailleError> {
    let kind = classify(context).map_err(|_| grants_unavailable())?;
    let context_object: Option<ToolsetContext> = stores
        .contexts()
        .get_context(context)
        .await
        .map_err(from_toolsets)?;
    let Some(context_object) = context_object else {
        return Err(grants_unavailable());
    };
    let type_reference = match kind {
        IdKind::Catalog => catalog_context::Entity::find()
            .filter(catalog_context::Column::Name.eq(context))
            .one(db)
            .await
            .map_err(|error| LymailleError::Store(db_text(&error)))?
            .and_then(|row| row.context_type),
        IdKind::Private => user_context::Entity::find()
            .filter(user_context::Column::OwnerId.eq(caller.user_id))
            .filter(user_context::Column::Name.eq(context))
            .one(db)
            .await
            .map_err(|error| LymailleError::Store(db_text(&error)))?
            .and_then(|row| row.context_type),
        // Un contexte résolu par l'hôte n'a pas de type en base : échec fermé, aucun droit.
        IdKind::Host => None,
    };
    let Some(type_reference) = type_reference else {
        return Err(grants_unavailable());
    };
    let row = context_type::Entity::find()
        .filter(context_type::Column::Name.eq(type_reference.as_str()))
        .one(db)
        .await
        .map_err(|error| LymailleError::Store(db_text(&error)))?;
    let Some(row) = row else {
        return Err(grants_unavailable());
    };
    if row.active.as_ref().is_none_or(JsonValue::is_null) {
        return Err(grants_unavailable());
    }
    let (toolsets, modules) = parse_grants(&row.script_grants).ok_or_else(grants_unavailable)?;
    Ok((context_object, Grants { toolsets, modules }))
}

/// La porte de lecture des `script_grants` : objet à exactement `toolsets` et `modules`, chacun
/// un tableau de chaînes (la forme que le hook de `../definitions/context_type.rs` impose à
/// l'écriture) ; toute autre forme est « illisible », échec fermé.
fn parse_grants(value: &JsonValue) -> Option<(Vec<String>, BTreeSet<String>)> {
    let entries = value.as_object()?;
    if entries.len() != 2 || !entries.contains_key("toolsets") || !entries.contains_key("modules") {
        return None;
    }
    let toolsets = string_array(entries.get("toolsets")?)?;
    let modules = string_array(entries.get("modules")?)?;
    Some((toolsets, modules.into_iter().collect()))
}

/// Tableau JSON de chaînes, sinon `None`.
fn string_array(value: &JsonValue) -> Option<Vec<String>> {
    value
        .as_array()
        .filter(|values| values.iter().all(serde_json::Value::is_string))
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
}

/// Les toolsets accordés, en version **active** (`SeaToolsetStore`, jamais un brouillon). Un
/// toolset sans ligne active est un droit introuvable : échec fermé, la porte n'a pas à pardonner.
async fn load_granted_toolsets(
    stores: &LymailleStores,
    names: &[String],
) -> Result<Vec<Toolset>, LymailleError> {
    let mut toolsets = Vec::with_capacity(names.len());
    for name in names {
        let toolset = stores
            .toolsets()
            .get_active(name)
            .await
            .map_err(from_toolsets)?
            .ok_or_else(grants_unavailable)?;
        toolsets.push(toolset);
    }
    Ok(toolsets)
}

/// Le `Dynamic` rendu, converti par la conversion du module des ressources et contrôlé en taille
/// sérialisée — jamais tronqué (`AC-5`).
fn render_value(rendered: &Dynamic, limits: &ScriptLimits) -> Result<JsonValue, LymailleError> {
    let Some(value) = dynamic_to_json(rendered) else {
        return Err(LymailleError::ScriptFailed(
            "result is not a JSON value".to_owned(),
        ));
    };
    let Ok(serialized) = serde_json::to_vec(&value) else {
        return Err(LymailleError::Serialization(
            "script result could not be serialized".to_owned(),
        ));
    };
    if serialized.len() > limits.max_result_bytes {
        return Err(LymailleError::ScriptResultTooLarge);
    }
    Ok(value)
}

/// L'erreur de l'évaluation, typée (`Must` étape 5) : la coupure de progression est le délai,
/// `ErrorTooManyOperations` le plafond d'opérations, `ErrorDataTooLarge` un dépassement de taille ;
/// toute autre erreur est le message du script, expurgé par [`leaf_message`].
fn script_error(error: EvalAltResult) -> LymailleError {
    match error {
        EvalAltResult::ErrorTerminated(..) => LymailleError::ScriptTimeout,
        EvalAltResult::ErrorTooManyOperations(..) => LymailleError::ScriptOperationLimit,
        EvalAltResult::ErrorDataTooLarge(..) => LymailleError::ScriptResultTooLarge,
        other => LymailleError::ScriptFailed(leaf_message(&other)),
    }
}

/// Le message utile d'une erreur Rhai, **sans le source** : les enveloppes d'appel de fonction
/// (dont la Display porte le source de la fonction appelée) sont traversées jusqu'à la feuille —
/// message d'exécution porté par un module (`forbidden`, `internal error`, …) ou Display de la
/// feuille elle-même (aucune ne porte le source du script).
fn leaf_message(error: &EvalAltResult) -> String {
    let mut current = error;
    loop {
        match current {
            EvalAltResult::ErrorInFunctionCall(_, _, inner, _) => current = inner,
            EvalAltResult::ErrorRuntime(value, _) => return value.to_string(),
            other => return other.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ScriptDeps, ScriptLimits, ScriptRunner};
    use crate::lymaille::definitions::mcp_server;
    use crate::lymaille::error::LymailleError;
    use crate::lymaille::host_module::{HostAccess, HostFunction, HostModule, HostModules};
    use crate::lymaille::identity::{CallerPrincipal, Identity};
    use crate::lymaille::mcp_mount::{DynMcpAuth, DynMcpConnector, McpConnectorFactory};
    use crate::lymaille::resource_module::ResourceRegistry;
    use crate::lymaille::secret::SecretResolver;
    use crate::lymaille::store::test_support::test_db;
    use crate::resource::{AccessPolicy, MiryadResource};
    use crate::users::group;
    use crate::users::membership::sync_group_memberships;
    use crate::users::resolve_user;
    use ::lymaille::{
        ContextLevel, ContextType, McpConnector, McpError, McpLister, McpTool, RoleDecl, SystemClock,
        Toolset, ToolsetContext,
    };
    use sea_orm::entity::prelude::*;
    use sea_orm::sea_query::{Alias, Expr, Query};
    use sea_orm::{
        ActiveModelTrait,
        ActiveValue::{NotSet, Set},
        ConnectionTrait, Schema,
    };
    use serde::{Deserialize, Serialize};
    use serde_json::{Value as JsonValue, json};
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use vynil_core::Semver;
    use zeroize::Zeroizing;

    // ── Fixtures ────────────────────────────────────────────────────────────

    /// Ressource `recipes` `OwnerOnly` avec colonne propriétaire et booléen écrivable — fixture
    /// des Scenario « un spectateur… » et « un script agit… » (`script_runner.sdd`).
    mod recipes {
        use super::*;

        #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "llm_recipes")]
        #[serde(default)]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
            pub approved: bool,
            pub owner_id: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "recipes"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn owner_column() -> Option<Column> {
                Some(Column::OwnerId)
            }
        }
    }

    /// Module d'hôte factice : nom et politique paramétrés, une fonction `show(x)` d'arité 1 qui
    /// rend `x` — les Scenario prouvent que l'enrobage décide, jamais le module (forme de la
    /// fixture de `./host_module.rs`).
    struct ModuleFixture {
        name: &'static str,
        access: HostAccess,
    }

    impl HostModule for ModuleFixture {
        fn name(&self) -> &'static str {
            self.name
        }
        fn access(&self) -> HostAccess {
            self.access
        }
        fn functions(&self, _subject: &str) -> Vec<HostFunction> {
            vec![HostFunction {
                name: "show",
                params: 1,
                call: Arc::new(|args: &mut [rhai::Dynamic]| {
                    Ok(args.first().cloned().unwrap_or(rhai::Dynamic::UNIT))
                }),
            }]
        }
    }

    /// Registre d'hôte des fixtures : modules `view` et `act` (`Authenticated`), `guet`
    /// (`AdminOnly`).
    fn host_registry() -> Arc<HostModules> {
        let mut registry = HostModules::new();
        registry
            .register(Arc::new(ModuleFixture {
                name: "view",
                access: HostAccess::Authenticated,
            }))
            .expect("view se registre");
        registry
            .register(Arc::new(ModuleFixture {
                name: "act",
                access: HostAccess::Authenticated,
            }))
            .expect("act se registre");
        registry
            .register(Arc::new(ModuleFixture {
                name: "guet",
                access: HostAccess::AdminOnly,
            }))
            .expect("guet se registre");
        Arc::new(registry)
    }

    fn empty_host_registry() -> Arc<HostModules> {
        Arc::new(HostModules::new())
    }

    /// Base neuve migrée (tables lymaille comprises).
    async fn db_test() -> DatabaseConnection {
        test_db().await
    }

    /// Compte local (éventuellement admin par le groupe), principal relu par `Identity`.
    async fn compte(db: &DatabaseConnection, subject: &str, admin: bool) -> CallerPrincipal {
        let account = resolve_user(db, subject, None)
            .await
            .expect("compte local de test cree");
        if admin {
            sync_group_memberships(db, account.id, &[group::ADMIN_GROUP_NAME.to_owned()])
                .await
                .expect("appartenance au groupe admin");
        }
        Identity::new(db.clone())
            .resolve(subject)
            .await
            .expect("principal relu")
    }

    /// Retire toutes les appartenances d'un compte (retrait du groupe admin des Scenario).
    async fn retirer_groupes(db: &DatabaseConnection, user_id: i32) {
        sync_group_memberships(db, user_id, &[])
            .await
            .expect("appartenances retirees");
    }

    /// Table physique de la fixture `recipes`.
    async fn create_recipes_table(db: &DatabaseConnection) {
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(recipes::Entity))
            .await
            .expect("table recipes creee");
    }

    /// Ligne `recipes` d'un propriétaire, rend son `id`.
    async fn inserer_recette(db: &DatabaseConnection, owner_id: i32, title: &str, approved: bool) -> i32 {
        let inserted = recipes::ActiveModel {
            id: NotSet,
            title: Set(title.to_owned()),
            approved: Set(approved),
            owner_id: Set(owner_id),
        }
        .insert(db)
        .await
        .expect("ligne recipe inseree");
        inserted.id
    }

    /// Valeur de `script_grants` contractuelle : objet à exactement `toolsets` et `modules`.
    fn grants(toolsets: &[&str], modules: &[&str]) -> JsonValue {
        json!({"toolsets": toolsets, "modules": modules})
    }

    /// Ligne de type de contexte au catalogue. `active` : la version active est posée (sinon la
    /// ligne n'en porte aucune — le type est là mais sans droits activables).
    async fn inserer_type(db: &DatabaseConnection, name: &str, script_grants: JsonValue, active: bool) {
        let context_type = ContextType::new(
            name.to_owned(),
            Semver::parse("1.0.0").expect("la version de fixture se parse"),
            json!({}),
        )
        .expect("le type de fixture se construit");
        let active_value = if active {
            serde_json::to_value(context_type).expect("l'actif se serialise")
        } else {
            JsonValue::Null
        };
        let active_expr = if active {
            Expr::val(active_value)
        } else {
            Expr::null()
        };
        let mut insert = Query::insert();
        insert
            .into_table(Alias::new("miryad_lymaille_catalog_context_types"))
            .columns([
                Alias::new("name"),
                Alias::new("version"),
                Alias::new("allowed_agents"),
                Alias::new("script_grants"),
                Alias::new("active"),
                Alias::new("draft"),
                Alias::new("created_at"),
            ])
            .values(vec![
                Expr::val(name),
                Expr::val("1.0.0"),
                Expr::val(json!([])),
                Expr::val(script_grants),
                active_expr,
                Expr::null(),
                Expr::val(chrono::Utc::now()),
            ])
            .expect("les colonnes et valeurs de fixture correspondent");
        db.execute(&insert).await.expect("ligne de type inseree");
    }

    /// Ligne de contexte du catalogue (`miryad_lymaille_catalog_contexts`), référence de type
    /// optionnelle et liaisons rôle → serveur.
    async fn inserer_contexte(
        db: &DatabaseConnection,
        name: &str,
        type_ref: Option<&str>,
        roles: &[(&str, &str)],
    ) {
        let mut context = ToolsetContext::new(name.to_owned(), ContextLevel::Shared)
            .expect("le contexte de fixture se construit");
        for (role, server) in roles {
            context = context
                .with_role((*role).to_owned(), (*server).to_owned())
                .expect("la liaison de fixture est acceptee");
        }
        let value = serde_json::to_value(context).expect("le contexte se serialise");
        let type_expr = match type_ref {
            Some(reference) => Expr::val(reference.to_owned()),
            None => Expr::null(),
        };
        let mut insert = Query::insert();
        insert
            .into_table(Alias::new("miryad_lymaille_catalog_contexts"))
            .columns([
                Alias::new("name"),
                Alias::new("context"),
                Alias::new("context_type"),
                Alias::new("created_at"),
            ])
            .values(vec![
                Expr::val(name),
                Expr::val(value),
                type_expr,
                Expr::val(chrono::Utc::now()),
            ])
            .expect("les colonnes et valeurs de fixture correspondent");
        db.execute(&insert).await.expect("ligne de contexte inseree");
    }

    /// Toolset actif au catalogue, avec ses rôles déclarés (nom, requis).
    fn toolset(name: &str, roles: &[(&str, bool)]) -> Toolset {
        let mut toolset = Toolset::new(
            name.to_owned(),
            format!("le toolset {name}"),
            Semver::parse("1.0.0").expect("la version de fixture se parse"),
            "fn noop() { 1 }".to_owned(),
        )
        .expect("le toolset de fixture se construit");
        for (role, required) in roles {
            toolset = toolset
                .with_role(RoleDecl::new((*role).to_owned(), *required))
                .expect("le role de fixture est accepte");
        }
        toolset
    }

    /// Écrit la version **active** d'un toolset (et jamais un brouillon).
    async fn inserer_toolset_actif(db: &DatabaseConnection, toolset: &Toolset) {
        let active = serde_json::to_value(toolset).expect("l'actif se serialise");
        let mut insert = Query::insert();
        insert
            .into_table(Alias::new("miryad_lymaille_catalog_toolsets"))
            .columns([
                Alias::new("toolset_id"),
                Alias::new("version"),
                Alias::new("active"),
                Alias::new("draft"),
                Alias::new("created_at"),
            ])
            .values(vec![
                Expr::val(toolset.name()),
                Expr::val(toolset.version().to_string()),
                Expr::val(active),
                Expr::null(),
                Expr::val(chrono::Utc::now()),
            ])
            .expect("les colonnes et valeurs de fixture correspondent");
        db.execute(&insert)
            .await
            .expect("ligne active de toolset inseree");
    }

    /// Ligne de `miryad_lymaille_mcp_servers`.
    async fn inserer_serveur(
        db: &DatabaseConnection,
        name: &str,
        url: &str,
        auth_mode: &str,
        auth_ref: Option<&str>,
        groupes: JsonValue,
    ) {
        let actif = mcp_server::ActiveModel {
            id: NotSet,
            name: Set(name.to_owned()),
            url: Set(url.to_owned()),
            request_timeout_secs: Set(30),
            connect_timeout_secs: Set(5),
            auth_mode: Set(auth_mode.to_owned()),
            auth_ref: Set(auth_ref.map(str::to_owned)),
            allowed_groups: Set(groupes),
            created_at: Set(chrono::Utc::now()),
        };
        actif.insert(db).await.expect("ligne mcp_servers de test");
    }

    /// Résolveur de secrets de test : `FIXED_KEY` rend la valeur fixe, toute autre référence est
    /// `SecretUnresolved`.
    struct FixedSecret;

    impl SecretResolver for FixedSecret {
        fn resolve(&self, reference: &str) -> Result<Zeroizing<String>, LymailleError> {
            if reference == "FIXED_KEY" {
                Ok(Zeroizing::new("cle-fixe".to_owned()))
            } else {
                Err(LymailleError::SecretUnresolved(reference.to_owned()))
            }
        }
    }

    /// Fabrique MCP espionne : chaque connecteur enregistré loggue `url` et, au premier listing
    /// puis au premier appel d'outil, la présence d'un porteur dans les en-têtes rendus par
    /// l'auth du rôle (motif de `./interactive/assemble.rs`). `fail_text` rend tous les appels
    /// d'outils en échec au texte donné (fixture de la panne de connecteur).
    #[derive(Default)]
    struct SpyFactory {
        log: Arc<Mutex<Vec<(String, bool)>>>,
        tool_calls: Arc<Mutex<Vec<String>>>,
        calls: Arc<AtomicUsize>,
        fail_text: Arc<Mutex<Option<String>>>,
    }

    impl SpyFactory {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn fail(&self, text: &str) {
            *self.fail_text.lock().expect("verrou de lespion") = Some(text.to_owned());
        }

        fn log(&self) -> Vec<(String, bool)> {
            self.log.lock().expect("verrou de lespion").clone()
        }

        fn tool_calls(&self) -> Vec<String> {
            self.tool_calls.lock().expect("verrou de lespion").clone()
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl McpConnectorFactory for SpyFactory {
        fn make(
            &self,
            url: &str,
            _request_timeout: Duration,
            _connect_timeout: Duration,
            auth: Option<Arc<dyn DynMcpAuth>>,
        ) -> Arc<dyn DynMcpConnector> {
            let _ = self.calls.fetch_add(1, Ordering::SeqCst);
            Arc::new(SpyConnector {
                url: url.to_owned(),
                auth,
                log: Arc::clone(&self.log),
                tool_calls: Arc::clone(&self.tool_calls),
                probed_list: Arc::new(AtomicBool::new(false)),
                probed_call: Arc::new(AtomicBool::new(false)),
                fail_text: Arc::clone(&self.fail_text),
            })
        }
    }

    /// Connecteur espion : sans réseau, rend un outil unique `ping`, sonde l'auth au premier
    /// listing et au premier appel, échoue au texte `fail_text` à l'appel si posé.
    struct SpyConnector {
        url: String,
        auth: Option<Arc<dyn DynMcpAuth>>,
        log: Arc<Mutex<Vec<(String, bool)>>>,
        tool_calls: Arc<Mutex<Vec<String>>>,
        probed_list: Arc<AtomicBool>,
        probed_call: Arc<AtomicBool>,
        fail_text: Arc<Mutex<Option<String>>>,
    }

    impl SpyConnector {
        /// Presence d'un porteur dans les en-têtes de l'auth (la valeur n'est pas observable
        /// depuis `McpHeaders`, motif de `./interactive/assemble.rs`).
        async fn bearer(&self) -> bool {
            match &self.auth {
                Some(auth) => match auth.headers_dyn().await {
                    Ok(headers) => format!("{headers:?}").contains("bearer: true"),
                    Err(_) => false,
                },
                None => false,
            }
        }
    }

    impl McpLister for SpyConnector {
        fn list_tools(&self) -> impl Future<Output = Result<Vec<McpTool>, McpError>> + Send {
            let url = self.url.clone();
            let log = Arc::clone(&self.log);
            let probed = Arc::clone(&self.probed_list);
            async move {
                if !probed.swap(true, Ordering::SeqCst) {
                    let bearer = self.bearer().await;
                    log.lock().expect("verrou de lespion").push((url.clone(), bearer));
                }
                Ok(vec![McpTool::new("ping", "le ping", json!({"type": "object"}))])
            }
        }
    }

    impl McpConnector for SpyConnector {
        fn call_tool(
            &self,
            name: &str,
            _arguments: &JsonValue,
        ) -> impl Future<Output = Result<JsonValue, McpError>> + Send {
            let url = self.url.clone();
            let tool_calls = Arc::clone(&self.tool_calls);
            let probed = Arc::clone(&self.probed_call);
            let fail = self.fail_text.lock().expect("verrou de lespion").clone();
            async move {
                let bearer = if probed.swap(true, Ordering::SeqCst) {
                    false
                } else {
                    self.bearer().await
                };
                tool_calls
                    .lock()
                    .expect("verrou de lespion")
                    .push(format!("{url}|{name}|bearer:{bearer}"));
                match fail {
                    Some(text) => Err(McpError::Failed(text)),
                    None => Ok(json!({"pong": true})),
                }
            }
        }

        fn reconnect(&self) -> impl Future<Output = ()> + Send {
            std::future::ready(())
        }
    }

    /// Dépendances de test : registre de ressources, registre d'hôte, fabrique MCP espionne.
    fn deps_with(
        db: DatabaseConnection,
        readers: Arc<ResourceRegistry>,
        host_modules: Arc<HostModules>,
        mcp: Arc<SpyFactory>,
    ) -> ScriptDeps {
        ScriptDeps {
            db,
            readers,
            host_modules,
            secrets: Arc::new(FixedSecret),
            clock: Arc::new(SystemClock::new()),
            host: None,
            mcp,
        }
    }

    fn recipes_registry() -> Arc<ResourceRegistry> {
        let mut registry = ResourceRegistry::new();
        registry.register::<recipes::Entity>();
        Arc::new(registry)
    }

    fn empty_registry() -> Arc<ResourceRegistry> {
        Arc::new(ResourceRegistry::new())
    }

    /// Les titres des lignes rendues par `resources::list`, triés par position.
    fn listed_titles(rendu: &JsonValue) -> Vec<String> {
        rendu
            .get("items")
            .and_then(JsonValue::as_array)
            .expect("la cle items est rendue")
            .iter()
            .map(|item| {
                item.get("title")
                    .and_then(JsonValue::as_str)
                    .expect("le titre de la ligne")
                    .to_owned()
            })
            .collect()
    }

    // ── Scénarios ───────────────────────────────────────────────────────────

    /// Scenario « un spectateur n'obtient que ce que son principal peut faire » (AC-3) : même
    /// script, deux sujets, deux résultats ; un `subject` sans compte rend `UnknownSubject`.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_spectateur_n_obtient_que_ce_que_son_principal_peut_faire() {
        let db = db_test().await;
        create_recipes_table(&db).await;
        let alice = compte(&db, "alice", false).await;
        let bob = compte(&db, "bob", false).await;
        inserer_recette(&db, alice.user_id, "tarte", false).await;
        inserer_recette(&db, bob.user_id, "soupe", false).await;
        inserer_type(&db, "G_recettes", grants(&[], &[]), true).await;
        inserer_contexte(&db, "G_cuisine", Some("G_recettes"), &[]).await;
        let deps = deps_with(
            db.clone(),
            recipes_registry(),
            empty_host_registry(),
            SpyFactory::new(),
        );
        let runner = ScriptRunner::new(deps);
        let limites = ScriptLimits::default();

        let rendu_alice = runner
            .run(
                "G_cuisine",
                "alice",
                r#"resources::list("recipes", "")"#,
                &limites,
                None,
            )
            .await
            .expect("alice liste ses lignes");
        assert_eq!(listed_titles(&rendu_alice), vec!["tarte".to_owned()]);

        let rendu_bob = runner
            .run(
                "G_cuisine",
                "bob",
                r#"resources::list("recipes", "")"#,
                &limites,
                None,
            )
            .await
            .expect("bob liste ses lignes");
        assert_eq!(listed_titles(&rendu_bob), vec!["soupe".to_owned()]);

        let refus = runner
            .run("G_cuisine", "carol", "1 + 1", &limites, None)
            .await
            .expect_err("carol naucun compte");
        assert!(
            matches!(refus, LymailleError::UnknownSubject),
            "le sujet sans compte est refuse avant tout : {refus:?}"
        );
    }

    /// Scenario « un script agit sous les droits de son spectateur » : la ligne d'alice est mise
    /// à jour (hooks et `write_policy` de `rest::core` appliqués), celle de bob échoue en
    /// `ScriptFailed` (`forbidden`).
    #[tokio::test(flavor = "multi_thread")]
    async fn un_script_agit_sous_les_droits_de_son_spectateur() {
        let db = db_test().await;
        create_recipes_table(&db).await;
        let alice = compte(&db, "alice", false).await;
        let bob = compte(&db, "bob", false).await;
        let ligne_alice = inserer_recette(&db, alice.user_id, "tarte", false).await;
        let ligne_bob = inserer_recette(&db, bob.user_id, "soupe", false).await;
        inserer_type(&db, "G_recettes", grants(&[], &[]), true).await;
        inserer_contexte(&db, "G_cuisine", Some("G_recettes"), &[]).await;
        let deps = deps_with(
            db.clone(),
            recipes_registry(),
            empty_host_registry(),
            SpyFactory::new(),
        );
        let runner = ScriptRunner::new(deps);
        let limites = ScriptLimits::default();

        let script_alice = format!(r#"resources::update("recipes", {ligne_alice}, #{{ approved: true }})"#);
        let rendu = runner
            .run("G_cuisine", "alice", &script_alice, &limites, None)
            .await
            .expect("alice approuve sa ligne");
        assert_eq!(rendu.get("approved").and_then(JsonValue::as_bool), Some(true));
        let relue = recipes::Entity::find_by_id(ligne_alice)
            .one(&db)
            .await
            .expect("la ligne dalice se relit")
            .expect("la ligne dalice existe");
        assert!(relue.approved, "la ligne dalice est mise a jour en base");

        let script_bob = format!(r#"resources::update("recipes", {ligne_bob}, #{{ approved: true }})"#);
        let refus = runner
            .run("G_cuisine", "alice", &script_bob, &limites, None)
            .await
            .expect_err("alice ne peut pas ecrire la ligne de bob");
        match &refus {
            LymailleError::ScriptFailed(message) => {
                assert!(
                    message.contains("forbidden"),
                    "le refus nomme le motif : {message}"
                );
            }
            autre => panic!("`ScriptFailed` attendue, obtenu : {autre:?}"),
        }
        let relue = recipes::Entity::find_by_id(ligne_bob)
            .one(&db)
            .await
            .expect("la ligne de bob se relit")
            .expect("la ligne de bob existe");
        assert!(!relue.approved, "la ligne de bob est restee intacte");
    }

    /// Scenario « un module d'hôte accordé est appelable, un autre non » : `view` monté, `act`
    /// inconnu, et un module `AdminOnly` accordé refusé par un non-admin.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_module_hote_accorde_est_appelable_un_autre_non() {
        let db = db_test().await;
        let alice = compte(&db, "alice", false).await;
        inserer_type(&db, "G_t_mod", grants(&[], &["view"]), true).await;
        inserer_contexte(&db, "G_modctx", Some("G_t_mod"), &[]).await;
        inserer_type(&db, "G_t_vip", grants(&[], &["guet"]), true).await;
        inserer_contexte(&db, "G_vipctx", Some("G_t_vip"), &[]).await;
        let deps = deps_with(db.clone(), empty_registry(), host_registry(), SpyFactory::new());
        let runner = ScriptRunner::new(deps);
        let limites = ScriptLimits::default();

        let rendu = runner
            .run("G_modctx", "alice", "view::show(7)", &limites, None)
            .await
            .expect("le module accorde est appelable");
        assert_eq!(rendu, json!(7));

        let refus = runner
            .run("G_modctx", "alice", "act::show(7)", &limites, None)
            .await
            .expect_err("le module non accorde est inconnu");
        assert!(
            matches!(refus, LymailleError::ScriptFailed(_)),
            "le module non accorde est une fonction inconnue : {refus:?}"
        );

        let refus = runner
            .run("G_vipctx", "alice", "guet::show(7)", &limites, None)
            .await
            .expect_err("un non-admin ne passe pas la politique AdminOnly");
        match &refus {
            LymailleError::ScriptFailed(message) => {
                assert!(message.contains("forbidden"), "le refus est rendu : {message}");
            }
            autre => panic!("`ScriptFailed` attendue, obtenu : {autre:?}"),
        }
        let _ = alice;
    }

    /// Préparation du Scenario 4 : deux serveurs dont l'espion refuse l'accès selon le groupe du
    /// porteur (`grafana_admin`/`meteo_admin` restreints à `admin`), deux toolsets actifs (rôle
    /// requis `grafana`, rôle optionnel `meteo`), leurs types et contextes de liaison.
    async fn fixtures_role_mcp(db: &DatabaseConnection) {
        inserer_serveur(
            db,
            "grafana_srv",
            "http://espion.invalid/grafana",
            "static",
            Some("FIXED_KEY"),
            json!(["all"]),
        )
        .await;
        inserer_serveur(
            db,
            "grafana_admin",
            "http://espion.invalid/admin",
            "none",
            None,
            json!(["admin"]),
        )
        .await;
        inserer_serveur(
            db,
            "meteo_admin",
            "http://espion.invalid/meteo",
            "none",
            None,
            json!(["admin"]),
        )
        .await;
        inserer_toolset_actif(db, &toolset("G_ops", &[("grafana", true)])).await;
        inserer_toolset_actif(db, &toolset("G_meteo", &[("meteo", false)])).await;
        inserer_type(db, "G_t_ops", grants(&["G_ops"], &[]), true).await;
        inserer_contexte(db, "G_ops_ok", Some("G_t_ops"), &[("grafana", "grafana_srv")]).await;
        inserer_contexte(db, "G_ops_deny", Some("G_t_ops"), &[("grafana", "grafana_admin")]).await;
        inserer_type(db, "G_t_meteo", grants(&["G_meteo"], &[]), true).await;
        inserer_contexte(db, "G_meteo_deny", Some("G_t_meteo"), &[("meteo", "meteo_admin")]).await;
    }

    /// Scenario « un rôle MCP d'un toolset accordé est monté comme dans une session » : le
    /// connecteur reçoit l'appel avec l'authentification résolue pour le spectateur ; un rôle
    /// **requis** refusé fait échouer en `Forbidden("mcp server unavailable")` ; un rôle
    /// optionnel refusé est omis.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_role_mcp_d_un_toolset_accorde_est_monte_comme_dans_une_session() {
        let db = db_test().await;
        compte(&db, "alice", false).await;
        fixtures_role_mcp(&db).await;
        let spy = SpyFactory::new();
        let deps = deps_with(
            db.clone(),
            empty_registry(),
            empty_host_registry(),
            Arc::clone(&spy),
        );
        let runner = ScriptRunner::new(deps);
        let limites = ScriptLimits::default();

        let rendu = runner
            .run("G_ops_ok", "alice", "grafana::ping(#{})", &limites, None)
            .await
            .expect("le role monte est appelable comme en session");
        assert_eq!(rendu, json!({"pong": true}));
        assert!(
            spy.log()
                .contains(&("http://espion.invalid/grafana".to_owned(), true)),
            "lauth statique resolue pour le spectateur au montage : {:?}",
            spy.log()
        );
        assert!(
            spy.tool_calls()
                .iter()
                .any(|appel| appel.contains("ping") && appel.contains("bearer:true")),
            "lappel porte lauth resolue : {:?}",
            spy.tool_calls()
        );

        let refus = runner
            .run("G_ops_deny", "alice", "grafana::ping(#{})", &limites, None)
            .await
            .expect_err("le role requis vers un serveur interdit fait echouer");
        match &refus {
            LymailleError::Forbidden(message) => assert_eq!(message, "mcp server unavailable"),
            autre => panic!("`Forbidden` attendue, obtenu : {autre:?}"),
        }

        let rendu = runner
            .run("G_meteo_deny", "alice", "40 + 2", &limites, None)
            .await
            .expect("le role optionnel refuse est simplement omis");
        assert_eq!(rendu, json!(42));
        assert!(
            !spy.tool_calls().iter().any(|appel| appel.contains("meteo")),
            "aucun appel au role omis : {:?}",
            spy.tool_calls()
        );
        assert!(
            !spy.log().iter().any(|(url, _)| url.ends_with("/meteo")),
            "aucun connecteur monte pour le role omis : {:?}",
            spy.log()
        );
        assert_eq!(
            spy.calls(),
            1,
            "un seul connecteur cree : le role refuse (requis ou optionnel) n'en cree aucun"
        );
    }

    /// Scenario « un type de contexte absent ou sans version active ne donne aucun droit » :
    /// échec fermé `Forbidden("script grants unavailable")` dans les deux cas.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_type_de_contexte_absent_ou_sans_version_active_ne_donne_aucun_droit() {
        let db = db_test().await;
        compte(&db, "alice", false).await;
        inserer_type(&db, "G_t_inactive", grants(&[], &[]), false).await;
        inserer_contexte(&db, "G_orphelin", Some("G_t_inactive"), &[]).await;
        let deps = deps_with(
            db.clone(),
            empty_registry(),
            empty_host_registry(),
            SpyFactory::new(),
        );
        let runner = ScriptRunner::new(deps);
        let limites = ScriptLimits::default();

        let refus = runner
            .run("G_inconnu", "alice", "1 + 1", &limites, None)
            .await
            .expect_err("le contexte inconnu ne donne aucun droit");
        match &refus {
            LymailleError::Forbidden(message) => assert_eq!(message, "script grants unavailable"),
            autre => panic!("`Forbidden` attendue, obtenu : {autre:?}"),
        }

        let refus = runner
            .run("G_orphelin", "alice", "1 + 1", &limites, None)
            .await
            .expect_err("un type sans version active ne donne aucun droit");
        match &refus {
            LymailleError::Forbidden(message) => assert_eq!(message, "script grants unavailable"),
            autre => panic!("`Forbidden` attendue, obtenu : {autre:?}"),
        }
    }

    /// Scenario « durée, opérations et taille échouent en erreur typée » (AC-5) : jamais de
    /// valeur tronquée.
    #[tokio::test(flavor = "multi_thread")]
    async fn duree_operations_et_taille_echouent_en_erreur_typed() {
        let db = db_test().await;
        compte(&db, "alice", false).await;
        inserer_type(&db, "G_t_vide", grants(&[], &[]), true).await;
        inserer_contexte(&db, "G_vide", Some("G_t_vide"), &[]).await;
        let deps = deps_with(
            db.clone(),
            empty_registry(),
            empty_host_registry(),
            SpyFactory::new(),
        );
        let runner = ScriptRunner::new(deps);

        let duree = ScriptLimits {
            max_duration: Duration::from_millis(100),
            max_operations: 1 << 60,
            ..Default::default()
        };
        let refus = runner
            .run("G_vide", "alice", "loop {}", &duree, None)
            .await
            .expect_err("la boucle infinie depasse la duree");
        assert!(
            matches!(refus, LymailleError::ScriptTimeout),
            "duree typée attendue : {refus:?}"
        );

        let operations = ScriptLimits {
            max_duration: Duration::from_secs(10),
            max_operations: 1000,
            ..Default::default()
        };
        let refus = runner
            .run(
                "G_vide",
                "alice",
                "let x = 0; for i in 0..200000 { x += 1; } x",
                &operations,
                None,
            )
            .await
            .expect_err("la boucle depasse les operations");
        assert!(
            matches!(refus, LymailleError::ScriptOperationLimit),
            "operations typées attendues : {refus:?}"
        );

        let taille = ScriptLimits {
            max_duration: Duration::from_secs(10),
            max_result_bytes: 64,
            ..Default::default()
        };
        let refus = runner
            .run(
                "G_vide",
                "alice",
                "let s = \"aaaa\"; while s.len() < 500 { s += s; } s",
                &taille,
                None,
            )
            .await
            .expect_err("le resultat depasse la taille");
        assert!(
            matches!(refus, LymailleError::ScriptResultTooLarge),
            "taille typée attendue : {refus:?}"
        );
    }

    /// Scenario « le droit est relu à chaque appel de module » : premier passage sous le groupe
    /// admin, second refusé après le retrait.
    #[tokio::test(flavor = "multi_thread")]
    async fn le_droit_est_relu_a_chaque_appel_de_module() {
        let db = db_test().await;
        let alice = compte(&db, "alice", true).await;
        inserer_type(&db, "G_t_vip", grants(&[], &["guet"]), true).await;
        inserer_contexte(&db, "G_vipctx", Some("G_t_vip"), &[]).await;
        let deps = deps_with(db.clone(), empty_registry(), host_registry(), SpyFactory::new());
        let runner = ScriptRunner::new(deps);
        let limites = ScriptLimits::default();

        let rendu = runner
            .run("G_vipctx", "alice", "guet::show(7)", &limites, None)
            .await
            .expect("ladmin passe la politique AdminOnly");
        assert_eq!(rendu, json!(7));

        retirer_groupes(&db, alice.user_id).await;

        let refus = runner
            .run("G_vipctx", "alice", "guet::show(7)", &limites, None)
            .await
            .expect_err("le retrait du groupe est effectif au second appel");
        match &refus {
            LymailleError::ScriptFailed(message) => {
                assert!(message.contains("forbidden"), "le refus est rendu : {message}");
            }
            autre => panic!("`ScriptFailed` attendue, obtenu : {autre:?}"),
        }
    }

    /// Scenario « une erreur de script ne cite ni le source ni de ligne ni de jeton » : erreur
    /// Rhai, lecture en base qui échoue, panne de connecteur `forward` — le message reste
    /// expurgé.
    #[tokio::test(flavor = "multi_thread")]
    async fn une_erreur_de_script_ne_cite_ni_le_source_ni_une_ligne_ni_un_jeton() {
        // Erreur Rhai et panne de lecture côté base : même base, table `recipes` absente.
        let db = db_test().await;
        compte(&db, "alice", false).await;
        inserer_type(&db, "G_t_vide", grants(&[], &[]), true).await;
        inserer_contexte(&db, "G_vide", Some("G_t_vide"), &[]).await;
        let deps = deps_with(
            db.clone(),
            recipes_registry(),
            empty_host_registry(),
            SpyFactory::new(),
        );
        let runner = ScriptRunner::new(deps);
        let limites = ScriptLimits::default();

        let refus = runner
            .run(
                "G_vide",
                "alice",
                "function f() { return 1 / 0; }\nlet trace = \"FRAGMENT_SOURCE_UNIQUE_zl9k\";\nif trace.len() == 0 { print(trace); }\nf()",
                &limites,
                None,
            )
            .await
            .expect_err("le script leve une erreur Rhai");
        match &refus {
            LymailleError::ScriptFailed(message) => {
                assert!(
                    !message.contains("FRAGMENT_SOURCE_UNIQUE_zl9k") && !message.contains("function f"),
                    "le message ne cite ni le source ni une ligne : {message}"
                );
            }
            autre => panic!("`ScriptFailed` attendue, obtenu : {autre:?}"),
        }

        let refus = runner
            .run(
                "G_vide",
                "alice",
                r#"resources::get("recipes", 1)"#,
                &limites,
                None,
            )
            .await
            .expect_err("la lecture de la table absente echoue");
        match &refus {
            LymailleError::ScriptFailed(message) => {
                assert!(
                    !message.contains("no such table")
                        && !message.contains("DbErr")
                        && !message.contains("SELECT"),
                    "le detail dune DbErr ne remonte pas : {message}"
                );
            }
            autre => panic!("`ScriptFailed` attendue, obtenu : {autre:?}"),
        }

        // Panne du connecteur d'un rôle `forward` : ni le jeton ni l'URL n'apparaissent.
        inserer_serveur(
            &db,
            "fwd_srv",
            "http://espion.invalid/fwd",
            "forward",
            None,
            json!(["all"]),
        )
        .await;
        inserer_toolset_actif(&db, &toolset("G_fwd", &[("fwd", true)])).await;
        inserer_type(&db, "G_t_fwd", grants(&["G_fwd"], &[]), true).await;
        inserer_contexte(&db, "G_fwdctx", Some("G_t_fwd"), &[("fwd", "fwd_srv")]).await;
        let spy = SpyFactory::new();
        spy.fail("le serveur a refuse le porteur JETON-UNIQUE-jk7q");
        let deps = deps_with(
            db.clone(),
            empty_registry(),
            empty_host_registry(),
            Arc::clone(&spy),
        );
        let runner = ScriptRunner::new(deps);
        let jeton = Zeroizing::new("JETON-UNIQUE-jk7q".to_owned());
        let refus = runner
            .run("G_fwdctx", "alice", "fwd::ping(#{})", &limites, Some(jeton))
            .await
            .expect_err("le connecteur qui echoue echoue le script");
        match &refus {
            LymailleError::ScriptFailed(message) => {
                assert!(
                    !message.contains("JETON-UNIQUE-jk7q") && !message.contains("espion.invalid"),
                    "ni le jeton ni lURL ne paraissent : {message}"
                );
            }
            autre => panic!("`ScriptFailed` attendue, obtenu : {autre:?}"),
        }
    }

    // Le type des Scenario ci-dessus impose aussi le typage public de `ScriptLimits`.
    #[allow(dead_code)]
    fn limites_typage(limite: &ScriptLimits) -> (Duration, u64, usize, BTreeSet<String>) {
        (
            limite.max_duration,
            limite.max_operations,
            limite.max_result_bytes,
            BTreeSet::new(),
        )
    }
}
