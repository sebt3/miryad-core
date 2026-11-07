//! Assemblage du moteur d'une session interactive (`src/lymaille/interactive/assemble.sdd`).
//!
//! Pendant interactif de l'assemblage batch de `../../workflow/subagent_step.rs` : même patron
//! `InferenceFactory`/`DynProvider` (les types du step batch sont privés à leur module), même
//! `ScratchpadEngine`. Pour **ce** principal : agent et profil relus via ses stores, `check_grants`
//! sur les modules de la définition, toolsets **actifs** (jamais un brouillon), contexte lié à la
//! session résolu par `SeaContextStore` (privé de l'appelant, `G_…` du catalogue, `H_…` par
//! `resolve_context` de l'hôte — jamais le contexte d'un autre), puis pour chaque rôle lié du
//! contexte ou exigé par les toolsets et leurs types : `resolve_role` (hôte d'abord, repli table
//! `mcp_access`) et `build_auth` avec le jeton du tour. Une session `Config` n'assemble **aucun**
//! client MCP et reçoit les modules de configuration (`config_registration` sur nos stores).
//!
//! Le jeton `forward` n'est transmis qu'à `build_auth` et ne vit que dans le `SeaMcpAuth` du tour.
//! Aucune décision de droit n'est dupliquée : tout passe par `../identity.rs`, `../mcp_access.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use sea_orm::DatabaseConnection;
use zeroize::Zeroizing;

use ::lymaille::AgentDefinition;
use ::lymaille::AgentRef;
use ::lymaille::AgentStore as _;
use ::lymaille::CancelHandle;
use ::lymaille::Clock;
use ::lymaille::ConfigEnv;
use ::lymaille::ConfigStores;
use ::lymaille::ContextError;
use ::lymaille::ContextStore as _;
use ::lymaille::ContextType;
use ::lymaille::ContextTypeStore as _;
use ::lymaille::DefaultStrategy;
use ::lymaille::EventSink;
use ::lymaille::GrantedModule;
use ::lymaille::InferenceError;
use ::lymaille::InferenceProvider;
use ::lymaille::InferenceRequest;
use ::lymaille::InferenceResponse;
use ::lymaille::InspectionSlot;
use ::lymaille::McpEnv;
use ::lymaille::McpRetryPolicy;
use ::lymaille::ModelSnapshot;
use ::lymaille::ProfileStore as _;
use ::lymaille::Provenance;
use ::lymaille::Provider;
use ::lymaille::ProviderStore as _;
use ::lymaille::ReplayMode;
use ::lymaille::ResolvedContext;
use ::lymaille::ResolvedProfile;
use ::lymaille::RestoredSession;
use ::lymaille::RigConfig;
use ::lymaille::RigProvider;
use ::lymaille::Scratchpad;
use ::lymaille::ScratchpadEngine;
use ::lymaille::ScratchpadLimits;
use ::lymaille::SegmentContext;
use ::lymaille::SessionError;
use ::lymaille::SessionId;
use ::lymaille::SessionKind;
use ::lymaille::SessionState;
use ::lymaille::SessionStore as _;
use ::lymaille::Summarizer;
use ::lymaille::SystemClock;
use ::lymaille::Toolset;
use ::lymaille::ToolsetContext;
use ::lymaille::ToolsetStore as _;
use ::lymaille::TurnConfig;
use ::lymaille::TurnOutput;
use ::lymaille::TurnRunner;
use ::lymaille::assemble;
use ::lymaille::config_registration;
use ::lymaille::restore_session;
use ::lymaille::rhai_pad_definition;

use crate::lymaille::error::LymailleError;
use crate::lymaille::host::SharedHost;
use crate::lymaille::identity::{CallerPrincipal, check_grants};
use crate::lymaille::install_crypto_provider;
use crate::lymaille::mcp_mount::{McpConnectorFactory, MountDeps, RmcpConnectorFactory, mount_role};
use crate::lymaille::resource_module::{ResourceRegistry, resources_module};
use crate::lymaille::secret::SecretResolver;
use crate::lymaille::store::LymailleStores;
use crate::lymaille::store::SeaSessionStore;

use super::hub::EventHub;

/// Modules dont l'accord par une définition est réservé (DSD `FR-9`) : parité avec
/// `ADMIN_ONLY_MODULES` de `../../workflow/subagent_step.rs` — les deux listes doivent rester
/// identiques ; la liste est vide à ce jour, le mécanisme (`check_grants`) est câblé et testé
/// par injection (`Scenario` de la spec).
pub(crate) const ADMIN_ONLY_MODULES: &[&str] = &[];

/// Couture d'inférence (test) : produit le client d'inférence d'un provider, là où la production
/// construit un [`RigProvider`]. duplicat assumé de la couture de
/// `../../workflow/subagent_step.rs` : cette dernière est privée à son module et le module
/// `workflow` est gated par la feature `workflow`, que `lymaille` seul n'active pas
/// (`References` de la spec).
pub(crate) trait InferenceFactory: Send + Sync {
    /// Client pour l'adresse et la clé du provider résolu. `api_key` est déjà sortie de son
    /// `Zeroizing` ; nulle trace n'en porte la valeur.
    fn make(&self, address: String, api_key: Option<String>) -> Arc<dyn DynInference>;
}

/// Client d'inférence sous forme objet : [`InferenceProvider`] de `lymaille` n'est pas
/// object-safe (méthodes `impl Future`), le pas d'indirection est posé ici.
pub(crate) trait DynInference: Send + Sync {
    fn complete_dyn(
        &self,
        request: InferenceRequest,
    ) -> Pin<Box<dyn Future<Output = Result<InferenceResponse, InferenceError>> + Send + '_>>;
}

impl<P> DynInference for P
where
    P: InferenceProvider + Sync + 'static,
{
    fn complete_dyn(
        &self,
        request: InferenceRequest,
    ) -> Pin<Box<dyn Future<Output = Result<InferenceResponse, InferenceError>> + Send + '_>> {
        Box::pin(self.complete(request))
    }
}

/// Fabrique de production : un [`RigProvider`] par provider résolu (feature `rig` de `lymaille`).
struct RigInferenceFactory;

impl InferenceFactory for RigInferenceFactory {
    fn make(&self, address: String, api_key: Option<String>) -> Arc<dyn DynInference> {
        Arc::new(RigProvider::new(&RigConfig::new(address, api_key)))
    }
}

/// Client objet rendu compatible avec le générique [`InferenceProvider`] de `lymaille`.
struct DynProvider(Arc<dyn DynInference>);

impl InferenceProvider for DynProvider {
    fn complete(
        &self,
        request: InferenceRequest,
    ) -> impl Future<Output = Result<InferenceResponse, InferenceError>> + Send {
        self.0.complete_dyn(request)
    }
}

/// Summarizer de l'assemblage interactif : `lymaille` 0.1.0 n'en fournit aucun de production ;
/// si la compactation était atteinte, l'échec est rapporté honnêtement (jamais un résumé vide
/// fabriqué). Même position que `NoSummarizer` du step batch.
struct NoSummarizer;

impl Summarizer for NoSummarizer {
    fn summarize(
        &self,
        _transcript: &str,
        _digest: Option<&::lymaille::Digest>,
    ) -> impl Future<Output = Result<String, ContextError>> + Send {
        std::future::ready(Err(ContextError::Summarizer(
            "no summarizer is wired for interactive sessions".to_owned(),
        )))
    }
}

/// Ce que l'application fournit à l'assemblage (`Must` de la spec) : connexion, lecteurs de
/// ressources, secrets, hub d'événements, horloge, hôte optionnel, et les deux coutures de test
/// (inférence, connecteurs MCP) dont les valeurs par défaut sont les fabriques de production.
pub struct AssembleDeps {
    /// Connexion à la base de l'application (identité, stores, module des ressources).
    pub db: DatabaseConnection,
    /// Registre des entités lisibles et écrivable par les agents (`../resource_module.rs`).
    pub readers: Arc<ResourceRegistry>,
    /// Résolveur des références d'authentification (`../secret.rs`).
    pub secrets: Arc<dyn SecretResolver>,
    /// Diffusion des événements de session (`./hub.rs`) — l'`EventSink` des tours et des modules.
    pub hub: Arc<EventHub>,
    /// Horloge des états, des tours et des modules.
    pub clock: Arc<dyn Clock>,
    /// Hôte applicatif optionnel (DSD `DEC-27`) : contextes `H_…` et décisions MCP.
    pub host: Option<SharedHost>,
    /// Couture d'inférence : [`RigInferenceFactory`] en production. Traité `pub(crate)` : la
    /// couture n'est pas une surface publique — `AssembleDeps::new` pose la fabrique de
    /// production, les tests de ce fichier remplacent le champ.
    pub(crate) inference: Arc<dyn InferenceFactory>,
    /// Couture des connecteurs MCP : [`RmcpConnectorFactory`] en production. Traité
    /// `pub(crate)` comme la couture d'inférence.
    pub(crate) mcp: Arc<dyn McpConnectorFactory>,
}

impl AssembleDeps {
    /// Construit les dépendances de production : fabriques `rig` et `rmcp`, horloge système, sans
    /// hôte (l'application ajoute `host` par construction du champ).
    #[must_use]
    pub fn new(
        db: DatabaseConnection,
        readers: Arc<ResourceRegistry>,
        secrets: Arc<dyn SecretResolver>,
        hub: Arc<EventHub>,
    ) -> Self {
        Self {
            db,
            readers,
            secrets,
            hub,
            clock: Arc::new(SystemClock::new()),
            host: None,
            inference: Arc::new(RigInferenceFactory),
            mcp: Arc::new(RmcpConnectorFactory),
        }
    }
}

/// Session assemblée : le `TurnRunner` prêt à recevoir les tours de l'utilisateur, l'annulation
/// racine de la session et les avis de provenance (rôles omis, écarts de reprise `OI-8`).
pub struct AssembledSession {
    runner: TurnRunner<DynProvider, DefaultStrategy<NoSummarizer>, SeaSessionStore>,
    cancel: CancelHandle,
    notices: Vec<String>,
    toolset_versions: BTreeMap<String, String>,
    /// Le moteur de la session — conservé pour l'introspection de test (modules montés).
    engine: Arc<ScratchpadEngine>,
}

impl std::fmt::Debug for AssembledSession {
    /// Opaque par construction : le moteur et le coureur portent des secrets (redactor, auths
    /// MCP) — rien n'y est rendu.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssembledSession").finish_non_exhaustive()
    }
}

impl AssembledSession {
    /// Le tour `text` de l'utilisateur, sous l'annulation racine de la session.
    ///
    /// # Errors
    ///
    /// `LymailleError::Store` sur panne du store de session, `LymailleError::Run` sinon.
    pub async fn run_turn(&mut self, text: &str) -> Result<TurnOutput, LymailleError> {
        let cancel = self.cancel.clone();
        self.runner.run_turn(text, &cancel).await.map_err(from_session)
    }

    /// L'anse d'annulation racine de la session.
    #[must_use]
    pub const fn cancel(&self) -> &CancelHandle {
        &self.cancel
    }

    /// Les avis de provenance produits à l'assemblage (rôles optionnels omis, reprise).
    #[must_use]
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    /// Les versions effectives des toolsets montés (`Provenance` de la session).
    #[must_use]
    pub const fn toolset_versions(&self) -> &BTreeMap<String, String> {
        &self.toolset_versions
    }

    /// Le moteur assemblé de la session — pour l'introspection (fonctions et modules montés).
    #[must_use]
    pub fn engine(&self) -> Arc<ScratchpadEngine> {
        Arc::clone(&self.engine)
    }
}

/// Assemble le moteur et le `TurnRunner` de la session `session` de l'agent `agent_id` pour ce
/// principal (`Must` de la spec, étapes 1 à 6).
///
/// # Errors
///
/// - `LymailleError::NotFound` : agent inconnu de ce principal, toolset actif absent.
/// - `LymailleError::Forbidden` : module réservé accordé (`check_grants`), rôle **requis** vers
///   un serveur refusé (`"mcp server unavailable"`), `forward` sans jeton.
/// - `LymailleError::InvalidDefinition` : assemblage d'un toolset impossible, nom de module
///   invalide.
/// - `LymailleError::Store` / `LymailleError::Config` / `LymailleError::Run` : pannes des stores,
///   du moteur ou d'un tour.
pub async fn assemble_session(
    deps: &AssembleDeps,
    caller: &CallerPrincipal,
    kind: SessionKind,
    agent_id: &str,
    session: &SessionId,
    token: Option<Zeroizing<String>>,
) -> Result<AssembledSession, LymailleError> {
    assemble_with(deps, caller, kind, agent_id, session, token, ADMIN_ONLY_MODULES).await
}

/// Cœur de `assemble_session` avec la liste de modules réservés injectée — séparateur du
/// `Scenario` « un module réservé à l'admin est refusé à l'assemblage » ; la fonction publique
/// passe la liste vide.
async fn assemble_with(
    deps: &AssembleDeps,
    caller: &CallerPrincipal,
    kind: SessionKind,
    agent_id: &str,
    session: &SessionId,
    token: Option<Zeroizing<String>>,
    reserved: &[&str],
) -> Result<AssembledSession, LymailleError> {
    install_crypto_provider();
    // Étapes 1 à 3 : stores du sujet de l'appelant, définition relue, contrôle des modules
    // réservés, profil et client d'inférence — la référence d'auth n'est résolue qu'ici, à la
    // construction du client (`DEC-16`).
    let resolved = resolve_inference(deps, caller, agent_id, reserved).await?;
    // Étape 4 : toolsets actifs et contexte lié à la session (immuable, porté par l'état
    // persisté — `DEC-28`).
    let scope = resolve_scope(&resolved.stores, &resolved.definition, session).await?;
    // Étape 5 : modules — `resources` pour tous, clients MCP par rôle accordé (aucun pour
    // `Config`) — puis le moteur assemblé de tout cela.
    let mut granted = base_modules(deps, caller)?;
    // Noms réservés à la garde de collision (`./mcp_mount.sdd`, étape 0) : les modules déjà
    // accordés. Aucun module d'office de `lymaille` n'est à ajouter (`lymaille` 0.2.0 n'en déclare
    // aucun : `assemble` ne grant que ce qu'il reçoit).
    let granted_names: Vec<&str> = granted.iter().map(GrantedModule::name).collect();
    let (mcp_granted, mut notices) = mount_mcp_roles(
        deps,
        caller,
        kind,
        &scope,
        session,
        &resolved,
        token.as_ref(),
        &granted_names,
    )
    .await?;
    granted.extend(mcp_granted);
    let engine = build_engine(deps, &resolved, kind, session, &scope, &granted, token.as_ref())?;
    let toolset_versions: BTreeMap<String, String> = scope
        .toolsets
        .iter()
        .map(|toolset| (toolset.name().to_owned(), toolset.version().to_string()))
        .collect();

    // Étape 6 : état et pad — reprise via `restore_session` quand la session existe (avis de
    // provenance transmis, `OI-8`), sinon état neuf de la nature demandée.
    let session_store = resolved.stores.sessions();
    let provenance = Provenance::new(resolved.definition.fingerprint(), toolset_versions.clone());
    let (state, pad, restore_notices) = if session_store.load(session).await.map_err(from_session)?.is_some()
    {
        let restored: RestoredSession =
            restore_session(&session_store, session, Arc::clone(&engine), &provenance)
                .await
                .map_err(from_session)?;
        restored.into_parts()
    } else {
        let fresh = SessionState::new(
            session.clone(),
            AgentRef::new(resolved.definition.id(), resolved.definition.fingerprint()),
            scope.context_name.clone(),
            toolset_versions.clone(),
            deps.clock.now(),
        )
        .with_kind(kind);
        (fresh, Scratchpad::new(Arc::clone(&engine)), Vec::new())
    };
    notices.extend(restore_notices);

    let limits = ScratchpadLimits::new();
    let budget = resolved.profile.budget(
        u64::try_from(::lymaille::DEFAULT_SUMMARY_PROMPT.chars().count()).unwrap_or(u64::MAX),
        u64::try_from(limits.render_cap_chars()).unwrap_or(u64::MAX),
    );
    let turn_config = TurnConfig::new(
        Arc::clone(&deps.clock),
        configured_redactor(resolved.api_key.as_ref(), token.as_ref()),
        Arc::clone(&deps.hub) as Arc<dyn EventSink>,
        CancelHandle::new(),
        budget,
    )
    .with_agent(resolved.definition.id());
    let segment = SegmentContext::new(
        resolved.definition.prompt().to_owned(),
        vec![rhai_pad_definition()],
        ModelSnapshot::new(resolved.profile.model(), resolved.profile.replay()),
    );
    let cancel = CancelHandle::new();
    let runner = TurnRunner::new(
        DynProvider(resolved.client),
        DefaultStrategy::new(NoSummarizer, ReplayMode::None),
        resolved.profile,
        segment,
        pad,
        resolved.stores.sessions(),
        state,
        turn_config,
    );
    Ok(AssembledSession {
        runner,
        cancel,
        notices,
        toolset_versions,
        engine,
    })
}

/// Étapes 1 à 3 : stores du sujet de l'appelant, définition de l'agent relu, profil et client
/// d'inférence de la session.
struct Resolved {
    stores: LymailleStores,
    definition: AgentDefinition,
    profile: ResolvedProfile,
    api_key: Option<Zeroizing<String>>,
    client: Arc<dyn DynInference>,
}

/// Stores liés au sujet de l'appelant (routage `owner_id` de `../store/toolset.rs` — un contexte,
/// un toolset ou une ligne d'autrui est introuvable par construction), définition relue avec le
/// contrôle en profondeur des modules réservés (`FR-4`), puis profil et provider ; la référence
/// d'auth n'est résolue qu'ici, à la construction du client (`DEC-16`).
async fn resolve_inference(
    deps: &AssembleDeps,
    caller: &CallerPrincipal,
    agent_id: &str,
    reserved: &[&str],
) -> Result<Resolved, LymailleError> {
    let stores = match &deps.host {
        Some(host) => {
            LymailleStores::new(deps.db.clone(), caller.subject.clone()).with_host(Arc::clone(host))
        }
        None => LymailleStores::new(deps.db.clone(), caller.subject.clone()),
    };
    let definition: AgentDefinition = stores
        .agents()
        .get_agent(agent_id)
        .await
        .map_err(from_agents)?
        .ok_or_else(|| LymailleError::NotFound(format!("unknown agent: {agent_id}")))?;
    // Défense en profondeur sur les modules : refus à l'assemblage, l'appel du module revalide.
    check_grants(caller, definition.modules(), reserved)?;
    let profile = stores
        .profiles()
        .resolve_profile(definition.profile(), None)
        .await
        .map_err(from_agents)?;
    let provider: Provider = stores
        .providers()
        .get_provider(profile.provider())
        .await
        .map_err(from_agents)?
        .ok_or_else(|| LymailleError::Config("inference provider is unknown".to_string()))?;
    let api_key: Option<Zeroizing<String>> = provider
        .auth()
        .map(|reference| deps.secrets.resolve(reference))
        .transpose()?;
    let client = deps.inference.make(
        provider.address().to_owned(),
        api_key.as_deref().map(ToString::to_string),
    );
    Ok(Resolved {
        stores,
        definition,
        profile,
        api_key,
        client,
    })
}

/// Étape 4 : le périmètre de la session — toolsets actifs, contexte persisté et rôles exigés.
struct Scope {
    context_name: String,
    context: Option<ToolsetContext>,
    bindings: BTreeMap<String, String>,
    toolsets: Vec<Toolset>,
    required: BTreeSet<String>,
    optional: BTreeSet<String>,
}

/// Toolsets **actifs** de la définition (jamais un brouillon) et contexte lié à la session
/// (immuable, porté par l'état persisté — `DEC-28`) : privé de l'appelant ou `G_…` du catalogue,
/// `H_…` par `resolve_context` de l'hôte — `SeaContextStore` ne lit jamais la ligne privée d'un
/// autre (règle 3 de `../store/mod.rs`) — puis les rôles exigés par les toolsets et aplatis de
/// leurs types (`DEC-28`).
async fn resolve_scope(
    stores: &LymailleStores,
    definition: &AgentDefinition,
    session: &SessionId,
) -> Result<Scope, LymailleError> {
    let state = stores.sessions().load(session).await.map_err(from_session)?;
    let context_name = state.map_or_else(String::new, |state| state.toolset_context().to_owned());
    let mut toolsets: Vec<Toolset> = Vec::new();
    for name in definition.toolsets() {
        let toolset = stores
            .toolsets()
            .get_active(name)
            .await
            .map_err(from_toolsets)?
            .ok_or_else(|| LymailleError::NotFound(format!("unknown toolset: {name}")))?;
        toolsets.push(toolset);
    }
    let context: Option<ToolsetContext> = if context_name.is_empty() {
        None
    } else {
        stores
            .contexts()
            .get_context(&context_name)
            .await
            .map_err(from_toolsets)?
    };
    let bindings: BTreeMap<String, String> =
        context.as_ref().map_or_else(BTreeMap::new, toolset_context_roles);
    let (required, optional) = role_requirements(&toolsets, stores).await?;
    Ok(Scope {
        context_name,
        context,
        bindings,
        toolsets,
        required,
        optional,
    })
}

/// Clients MCP par rôle (étape 5) : la boucle sur les rôles requis puis optionnels, et la
/// **politique** de l'assemblage. Le montage d'un rôle (liaison, auth, connecteur, module) est
/// `mcp_mount::mount_role`. Un rôle **requis** refusé fait échouer la session ; un rôle
/// optionnel refusé par le serveur (`"mcp server unavailable"`) n'émet qu'un avis, toute autre
/// erreur de montage reste fatale. Une session `Config` n'assemble aucun client MCP.
#[allow(clippy::too_many_arguments)] // Signature fixée par ./mcp_mount.sdd (garde de collision, `taken`, amendement 2026-11-07).
async fn mount_mcp_roles(
    deps: &AssembleDeps,
    caller: &CallerPrincipal,
    kind: SessionKind,
    scope: &Scope,
    session: &SessionId,
    resolved: &Resolved,
    token: Option<&Zeroizing<String>>,
    granted_names: &[&str],
) -> Result<(Vec<GrantedModule>, Vec<String>), LymailleError> {
    let mut granted: Vec<GrantedModule> = Vec::new();
    let mut notices: Vec<String> = Vec::new();
    if kind == SessionKind::Config {
        return Ok((granted, notices));
    }
    let mounts = MountDeps {
        db: &deps.db,
        secrets: &deps.secrets,
        host: deps.host.as_ref(),
        mcp: deps.mcp.as_ref(),
    };
    let mcp_redactor = Arc::new(configured_redactor(resolved.api_key.as_ref(), token));
    for role in scope.required.iter().chain(scope.optional.iter()) {
        let is_required = scope.required.contains(role);
        let env = McpEnv::new(
            Arc::clone(&mcp_redactor),
            Arc::clone(&deps.hub) as Arc<dyn EventSink>,
            Arc::clone(&deps.clock),
            session.clone(),
            McpRetryPolicy::default(),
        );
        match mount_role(
            &mounts,
            caller,
            &scope.context_name,
            role,
            scope.bindings.get(role).map(String::as_str),
            token,
            granted_names,
            env,
        )
        .await
        {
            Ok(module) => granted.push(module),
            Err(LymailleError::Forbidden(message)) if !is_required && message == "mcp server unavailable" => {
                notices.push(format!("role {role} omitted: mcp server unavailable"));
            }
            Err(error) => return Err(error),
        }
    }
    Ok((granted, notices))
}

/// Moteur de la session (fin d'étape 5) : l'assemblage `lymaille` des toolsets et des modules
/// accordés — rien d'autre — et, pour une session `Config`, l'enregistrement des modules de
/// configuration sur nos stores, en lecture seule pour l'hôte, sans aucun client MCP monté
/// dessus (`agent-sessions#BR-8`).
fn build_engine(
    deps: &AssembleDeps,
    resolved: &Resolved,
    kind: SessionKind,
    session: &SessionId,
    scope: &Scope,
    granted: &[GrantedModule],
    token: Option<&Zeroizing<String>>,
) -> Result<Arc<ScratchpadEngine>, LymailleError> {
    let limits = ScratchpadLimits::new();
    let resolved_context: ResolvedContext = match scope.context.as_ref() {
        Some(context) => context
            .resolve(std::iter::empty::<(String, String, serde_json::Value)>())
            .map_err(from_toolsets)?,
        None => ToolsetContext::new("empty".to_owned(), ::lymaille::ContextLevel::Session)
            .map_err(from_toolsets)?
            .resolve(std::iter::empty::<(String, String, serde_json::Value)>())
            .map_err(from_toolsets)?,
    };
    let assembled = assemble(
        &scope.toolsets,
        granted,
        &BTreeMap::new(),
        &resolved_context,
        &limits,
    );
    if let Some(first_failure) = assembled.failures().first() {
        return Err(LymailleError::InvalidDefinition(format!(
            "toolset assembly failed: {first_failure}"
        )));
    }
    let mut builder = ScratchpadEngine::builder(limits, assembled.into_grants());
    if kind == SessionKind::Config {
        let slot = Arc::new(InspectionSlot::new());
        let registration = config_registration(
            ConfigStores::new(
                resolved.stores.agents(),
                resolved.stores.toolsets(),
                resolved.stores.context_types(),
                resolved.stores.contexts(),
            ),
            resolved.stores.session_index(),
            resolved.stores.sessions(),
            slot,
            ConfigEnv::new(
                configured_redactor(resolved.api_key.as_ref(), token),
                Arc::clone(&deps.hub) as Arc<dyn EventSink>,
                Arc::clone(&deps.clock),
                session.clone(),
            ),
        );
        builder = builder.with_registration(registration);
    }
    Ok(Arc::new(builder.build().map_err(|error| {
        LymailleError::InvalidDefinition(format!("engine build failed: {error}"))
    })?))
}

/// Rôles requis et optionnels exigés par les toolsets actifs : leurs déclarations propres et
/// l'aplatissement des types de contexte requis (DSD `DEC-28`). Un type absent est une lacune de
/// `check_context` (ouverture), pas un échec d'assemblage : il n'ajoute aucun rôle.
async fn role_requirements(
    toolsets: &[Toolset],
    stores: &LymailleStores,
) -> Result<(BTreeSet<String>, BTreeSet<String>), LymailleError> {
    let mut required: BTreeSet<String> = BTreeSet::new();
    let mut optional: BTreeSet<String> = BTreeSet::new();
    for toolset in toolsets {
        for (name, declaration) in toolset.roles() {
            if declaration.required() {
                let _ = required.insert(name.clone());
            } else {
                let _ = optional.insert(name.clone());
            }
        }
    }
    let type_names: BTreeSet<String> = toolsets
        .iter()
        .flat_map(|toolset| toolset.type_requirements().keys().cloned())
        .collect();
    let mut types: BTreeMap<String, ContextType> = BTreeMap::new();
    for name in type_names {
        if let Some(context_type) = stores
            .context_types()
            .get_type(&name)
            .await
            .map_err(from_toolsets)?
        {
            let _ = types.insert(name, context_type);
        }
    }
    let lookup = |name: &str| types.get(name).cloned();
    for toolset in toolsets {
        for name in toolset.type_requirements().keys() {
            if let Some(context_type) = types.get(name) {
                // L'aplatissement d'un type incohérent est une lacune de définition : elle
                // bloque l'ouverture (`check_context`), pas l'assemblage d'une session déjà
                // ouverte — le type est traité comme n'apportant aucun rôle.
                if let Ok(flat) = context_type.flatten(&lookup) {
                    required.extend(flat.required_roles().iter().cloned());
                    optional.extend(flat.optional_roles().iter().cloned());
                }
            }
        }
    }
    let _ = &optional;
    optional.retain(|role| !required.contains(role));
    Ok((required, optional))
}

/// Liaisons rôle → serveur de la forme sérialisée d'un contexte (champ `roles` du JSON stocké —
/// `ToolsetContext` ne rend que les noms des rôles, jamais leurs références).
fn toolset_context_roles(context: &ToolsetContext) -> BTreeMap<String, String> {
    serde_json::to_value(context)
        .ok()
        .and_then(|value| {
            value
                .get("roles")
                .and_then(serde_json::Value::as_object)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|(role, reference)| {
                            reference
                                .as_str()
                                .map(|reference| (role.clone(), reference.to_owned()))
                        })
                        .collect()
                })
        })
        .unwrap_or_default()
}

/// Les modules de base : `resources` pour tous avec le sujet de la session (`Must` étape 5).
/// Les modules nommés `todo` / `memory_*` relèvent du montage batch de
/// `../../workflow/subagent_step.rs` ; la spec de ce fichier ne promet que `resources` et
/// l'enregistrement `Config`. Les modules MCP s'ajoutent par rôle au-dessus.
fn base_modules(deps: &AssembleDeps, caller: &CallerPrincipal) -> Result<Vec<GrantedModule>, LymailleError> {
    // Session interactive : `scope = None`, pas de journal (`resource_module.sdd`,
    // amendement lot 2).
    let resources = resources_module(
        Arc::clone(&deps.readers),
        deps.db.clone(),
        caller.subject.clone(),
        None,
    );
    Ok(vec![
        GrantedModule::new("resources", resources.into())
            .ok_or_else(|| LymailleError::InvalidDefinition("invalid module name: resources".to_owned()))?,
    ])
}

/// Le `Redactor` d'une session : les secrets connus du tour (clé d'API du provider, jeton
/// forward) y entrent à la construction et n'en sortent jamais.
fn configured_redactor(
    api_key: Option<&Zeroizing<String>>,
    token: Option<&Zeroizing<String>>,
) -> ::lymaille::Redactor {
    let mut redactor = ::lymaille::Redactor::default();
    for secret in api_key.into_iter().chain(token) {
        // Une valeur trop courte pour être masquée est un échec de construction du redactor,
        // pas une raison d'abandonner la session : elle est ignorée (`lymaille` la refuse par
        // principe, le secret n'en circule pas moins dans le tour).
        let _ = redactor.add_secret(secret.to_string());
    }
    redactor
}

/// Erreur d'un store de définitions (`AgentsError`) : panne de stockage → `Store`, refus →
/// `InvalidDefinition`.
fn from_agents(error: ::lymaille::AgentsError) -> LymailleError {
    match error {
        ::lymaille::AgentsError::Store(text) => LymailleError::Store(text),
        other => LymailleError::InvalidDefinition(other.to_string()),
    }
}

/// Erreur d'un store de toolsets (`ToolsetsError`) — même règle que [`from_agents`].
fn from_toolsets(error: ::lymaille::ToolsetsError) -> LymailleError {
    match error {
        ::lymaille::ToolsetsError::Store(text) => LymailleError::Store(text),
        other => LymailleError::InvalidDefinition(other.to_string()),
    }
}

/// Erreur de session (`SessionError`) : panne de stockage → `Store`, refus → `Run` non
/// retryable.
fn from_session(error: SessionError) -> LymailleError {
    match error {
        SessionError::Store(text) => LymailleError::Store(text),
        other => LymailleError::Run {
            message: other.to_string(),
            retryable: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lymaille::definitions::mcp_server;
    use crate::lymaille::identity::Identity;
    use crate::lymaille::mcp_mount::{DynMcpAuth, DynMcpConnector, McpConnectorFactory};
    use crate::lymaille::store::test_support::test_db;
    use crate::users::group;
    use crate::users::membership::sync_group_memberships;
    use crate::users::resolve_user;
    use ::lymaille::{
        AgentKind, ContextLevel, ModelProfile, ProviderLevel, Redactor, RunEnv, RunStatus, ScriptedProvider,
        ToolsetContext,
    };
    use ::lymaille::{McpConnector, McpError, McpLister, McpTool};
    use sea_orm::ActiveModelTrait;
    use sea_orm::ActiveValue::{NotSet, Set};
    use sea_orm::ConnectionTrait;
    use serde_json::json;
    use std::sync::Mutex;
    use std::time::Duration;
    use vynil_core::Semver;

    // ── Fixtures ────────────────────────────────────────────────────────────

    /// Base neuve migrée.
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

    /// Catalogue câblé par `root` (admin) : profil `qwen`, provider `local` d'auth `FIXED_KEY`.
    async fn wire_catalog(db: &DatabaseConnection) {
        let root = compte(db, "root", true).await;
        let _ = root;
        let stores = LymailleStores::new(db.clone(), "root".to_owned());
        stores
            .profiles()
            .put_profile(
                &ModelProfile::new("qwen".to_owned(), "local".to_owned(), "qwen-test".to_owned())
                    .with_context_size(262_144),
            )
            .await
            .expect("l'admin ecrit le profil");
        stores
            .providers()
            .put_provider(
                &Provider::new(
                    "local".to_owned(),
                    "openai".to_owned(),
                    "http://inference.invalid/v1".to_owned(),
                    Some("FIXED_KEY".to_owned()),
                    ProviderLevel::User,
                )
                .expect("des references simples font un provider"),
            )
            .await
            .expect("l'admin ecrit le provider");
    }

    /// Définition d'agent de test (nature `Work` par défaut des scénarios).
    fn agent(id: &str, kind: AgentKind, toolsets: Vec<String>) -> AgentDefinition {
        AgentDefinition::new(id.to_owned(), kind, "le prompt".to_owned(), "qwen".to_owned())
            .with_modules(vec!["resources".to_owned()])
            .with_toolsets(toolsets)
    }

    /// Écrit un agent privé (par son propriétaire — `classify` route vers la table privée).
    async fn put_agent(db: &DatabaseConnection, owner: &CallerPrincipal, definition: &AgentDefinition) {
        LymailleStores::new(db.clone(), owner.subject.clone())
            .agents()
            .put_agent(definition)
            .await
            .expect("le proprietaire ecrit son agent");
    }

    /// Toolset de test nommé, avec type requis optionnel.
    fn toolset(name: &str, version: &str, type_required: Option<&str>) -> Toolset {
        let toolset = Toolset::new(
            name.to_owned(),
            format!("the toolset {name}"),
            Semver::parse(version).expect("la version de fixture se parse"),
            "fn ping() { 1 }".to_owned(),
        )
        .expect("le toolset de fixture se construit");
        match type_required {
            Some(type_name) => toolset
                .with_type_requirement(
                    ::lymaille::TypeRequirement::new(
                        type_name.to_owned(),
                        Semver::parse("1.0.0").expect("la version de requirement se parse"),
                    )
                    .expect("un nom non vide fait un requirement"),
                )
                .expect("la declaration du type est acceptee"),
            None => toolset,
        }
    }

    /// Écrit la version **active** d'un toolset au catalogue, et un brouillon si `draft`.
    async fn put_active_toolset(db: &DatabaseConnection, toolset: &Toolset, draft: Option<&Toolset>) {
        let active = serde_json::to_value(toolset).expect("l'actif se serialise");
        let draft_value = draft.map(|draft| serde_json::to_value(draft).expect("le brouillon se serialise"));
        let mut insert = sea_orm::sea_query::Query::insert();
        insert
            .into_table(sea_orm::sea_query::Alias::new("miryad_lymaille_catalog_toolsets"))
            .columns([
                sea_orm::sea_query::Alias::new("toolset_id"),
                sea_orm::sea_query::Alias::new("version"),
                sea_orm::sea_query::Alias::new("active"),
                sea_orm::sea_query::Alias::new("draft"),
                sea_orm::sea_query::Alias::new("created_at"),
            ])
            .values(vec![
                sea_orm::sea_query::Expr::val(toolset.name()),
                sea_orm::sea_query::Expr::val(toolset.version().to_string()),
                sea_orm::sea_query::Expr::val(active),
                sea_orm::sea_query::Expr::val(draft_value),
                sea_orm::sea_query::Expr::val(chrono::Utc::now()),
            ])
            .expect("les colonnes et valeurs de fixture correspondent");
        db.execute(&insert)
            .await
            .expect("ligne active de toolset inseree");
    }

    /// Écrit un type de contexte au catalogue par `root`, avec ses rôles (nom, requis).
    async fn put_type(db: &DatabaseConnection, name: &str, roles: &[(&str, bool)]) {
        let mut context_type = ContextType::new(
            name.to_owned(),
            Semver::parse("1.0.0").expect("la version de fixture se parse"),
            json!({}),
        )
        .expect("le type de fixture se construit");
        for (role, required) in roles {
            context_type = context_type
                .with_role(::lymaille::RoleSpec::new((*role).to_owned(), *required))
                .expect("le role de fixture est accepte");
        }
        LymailleStores::new(db.clone(), "root".to_owned())
            .context_types()
            .put_type(&context_type)
            .await
            .expect("l'admin ecrit le type de contexte");
    }

    /// Ligne de contexte privé (table `miryad_lymaille_user_contexts`, owner donné).
    async fn inserer_contexte_prive(
        db: &DatabaseConnection,
        owner_id: i32,
        name: &str,
        roles: &[(&str, &str)],
    ) {
        let mut context = ToolsetContext::new(name.to_owned(), ContextLevel::User)
            .expect("le contexte de fixture se construit");
        for (role, server) in roles {
            context = context
                .with_role((*role).to_owned(), (*server).to_owned())
                .expect("la liaison de fixture est acceptee");
        }
        let value = serde_json::to_value(context).expect("le contexte se serialise");
        let mut insert = sea_orm::sea_query::Query::insert();
        insert
            .into_table(sea_orm::sea_query::Alias::new("miryad_lymaille_user_contexts"))
            .columns([
                sea_orm::sea_query::Alias::new("owner_id"),
                sea_orm::sea_query::Alias::new("name"),
                sea_orm::sea_query::Alias::new("context"),
                sea_orm::sea_query::Alias::new("created_at"),
            ])
            .values(vec![
                sea_orm::sea_query::Expr::val(owner_id),
                sea_orm::sea_query::Expr::val(name),
                sea_orm::sea_query::Expr::val(value),
                sea_orm::sea_query::Expr::val(chrono::Utc::now()),
            ])
            .expect("les colonnes et valeurs de fixture correspondent");
        db.execute(&insert).await.expect("ligne de contexte inseree");
    }

    /// Ligne de `miryad_lymaille_mcp_servers`.
    async fn inserer_serveur(
        db: &DatabaseConnection,
        name: &str,
        url: &str,
        auth_mode: &str,
        groupes: serde_json::Value,
    ) {
        let actif = mcp_server::ActiveModel {
            id: NotSet,
            name: Set(name.to_owned()),
            url: Set(url.to_owned()),
            request_timeout_secs: Set(30),
            connect_timeout_secs: Set(5),
            auth_mode: Set(auth_mode.to_owned()),
            auth_ref: Set(None),
            allowed_groups: Set(groupes),
            created_at: Set(chrono::Utc::now()),
        };
        actif.insert(db).await.expect("ligne mcp_servers de test");
    }

    /// État de session persisté, contexte `context`, nature `kind`.
    async fn inserer_session(
        deps: &AssembleDeps,
        caller: &CallerPrincipal,
        session: &SessionId,
        context: &str,
        kind: SessionKind,
    ) {
        let stores = LymailleStores::new(deps.db.clone(), caller.subject.clone());
        let state = SessionState::new(
            session.clone(),
            AgentRef::new("dev", "empreinte"),
            context,
            BTreeMap::new(),
            deps.clock.now(),
        )
        .with_kind(kind);
        stores
            .sessions()
            .save(&state)
            .await
            .expect("etat de session sauve");
    }

    /// Dépendances de test : provider scripté partagé, factory MCP espionne.
    fn deps_with(
        db: DatabaseConnection,
        provider: Arc<ScriptedProvider>,
        spy: Arc<SpyFactory>,
    ) -> AssembleDeps {
        AssembleDeps {
            db,
            readers: Arc::new(ResourceRegistry::new()),
            secrets: Arc::new(FixedSecret("cle-fixe")),
            hub: Arc::new(EventHub::new(None, 16)),
            clock: Arc::new(SystemClock::new()),
            host: None,
            inference: Arc::new(ScriptFactory(provider)),
            mcp: spy,
        }
    }

    /// Fabrique d'inférence : le même provider scripté à chaque session.
    struct ScriptFactory(Arc<ScriptedProvider>);

    impl InferenceFactory for ScriptFactory {
        fn make(&self, _address: String, _api_key: Option<String>) -> Arc<dyn DynInference> {
            let provider: Arc<dyn DynInference> = self.0.clone();
            provider
        }
    }

    /// Résolveur de secrets de test : `FIXED_KEY` rend la valeur fixe, toute autre référence
    /// est `SecretUnresolved`.
    struct FixedSecret(&'static str);

    impl SecretResolver for FixedSecret {
        fn resolve(&self, reference: &str) -> Result<Zeroizing<String>, LymailleError> {
            if reference == "FIXED_KEY" {
                Ok(Zeroizing::new(self.0.to_owned()))
            } else {
                Err(LymailleError::SecretUnresolved(reference.to_owned()))
            }
        }
    }

    /// Fabrique MCP espionne : chaque connecteur rendu enregistre `url` et, au premier listing
    /// (fait par `mcp_module` à l'assemblage), la présence d'un porteur dans les en-têtes rendus
    /// par l'auth du rôle.
    struct SpyFactory {
        log: Arc<Mutex<Vec<(String, bool)>>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl SpyFactory {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                log: Arc::new(Mutex::new(Vec::new())),
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn log(&self) -> Vec<(String, bool)> {
            self.log.lock().expect("verrou de lespion").clone()
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
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
            let _ = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Arc::new(SpyConnector {
                url: url.to_owned(),
                auth,
                log: Arc::clone(&self.log),
                probed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            })
        }
    }

    /// Connecteur espion : sans réseau, rend un outil unique et sonde l'auth au premier listing.
    struct SpyConnector {
        url: String,
        auth: Option<Arc<dyn DynMcpAuth>>,
        log: Arc<Mutex<Vec<(String, bool)>>>,
        probed: Arc<std::sync::atomic::AtomicBool>,
    }

    impl McpLister for SpyConnector {
        fn list_tools(&self) -> impl Future<Output = Result<Vec<McpTool>, McpError>> + Send {
            let url = self.url.clone();
            let log = Arc::clone(&self.log);
            let probed = Arc::clone(&self.probed);
            let headers = self.auth.as_ref().map(|auth| auth.headers_dyn());
            async move {
                if !probed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    let bearer = match headers {
                        Some(headers) => match headers.await {
                            Ok(headers) => format!("{headers:?}").contains("bearer: true"),
                            Err(_) => false,
                        },
                        None => false,
                    };
                    log.lock().expect("verrou de lespion").push((url.clone(), bearer));
                }
                Ok(vec![McpTool::new("ping", "le ping", json!({"type": "object"}))])
            }
        }
    }

    impl McpConnector for SpyConnector {
        fn call_tool(
            &self,
            _name: &str,
            _arguments: &serde_json::Value,
        ) -> impl Future<Output = Result<serde_json::Value, McpError>> + Send {
            std::future::ready(Ok(json!({"pong": true})))
        }

        fn reconnect(&self) -> impl Future<Output = ()> + Send {
            std::future::ready(())
        }
    }

    /// Réponse finale sans appel d'outil.
    fn final_text(text: &str) -> InferenceResponse {
        InferenceResponse::new(
            ::lymaille::Message::assistant(text.to_owned(), None, Vec::new()),
            None,
        )
    }

    // ── Scénarios ───────────────────────────────────────────────────────────

    /// Scenario « un module réservé à l'admin est refusé à l'assemblage » : `Forbidden`, la
    /// liste réservée étant le séparateur injecté (liste de production vide).
    #[tokio::test(flavor = "multi_thread")]
    async fn un_module_reserve_refuse_assemblage() {
        let db = db_test().await;
        wire_catalog(&db).await;
        let alice = compte(&db, "alice", false).await;
        let mut definition = agent("dev", AgentKind::Work, Vec::new());
        definition = definition.with_modules(vec!["resources".to_owned(), "admin_only".to_owned()]);
        put_agent(&db, &alice, &definition).await;
        let deps = deps_with(
            db.clone(),
            Arc::new(ScriptedProvider::new(Vec::new())),
            SpyFactory::new(),
        );
        let session = SessionId::new("s-reserve").expect("identifiant de session de test");

        let Err(refus) = assemble_with(
            &deps,
            &alice,
            SessionKind::Work,
            "dev",
            &session,
            None,
            &["admin_only"],
        )
        .await
        else {
            panic!("un module reserve doit refuser lassemblage d'un non-admin");
        };
        assert!(
            matches!(&refus, LymailleError::Forbidden(message) if message.contains("admin_only")),
            "le refus nomme le module reserve : {refus:?}"
        );
    }

    /// Scenario « un rôle requis vers un serveur interdit fait échouer » :
    /// `Forbidden("mcp server unavailable")`.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_role_requis_vers_un_serveur_interdit_fait_echouer() {
        let db = db_test().await;
        wire_catalog(&db).await;
        let alice = compte(&db, "alice", false).await;
        put_type(&db, "prodtype", &[("kubectl", true)]).await;
        put_active_toolset(&db, &toolset("G_kube", "1.0.0", Some("prodtype")), None).await;
        put_agent(
            &db,
            &alice,
            &agent("dev", AgentKind::Work, vec!["G_kube".to_owned()]),
        )
        .await;
        inserer_serveur(
            &db,
            "admin_srv",
            "http://espion.invalid/admin",
            "none",
            json!(["admin"]),
        )
        .await;
        inserer_contexte_prive(&db, alice.user_id, "moncontexte", &[("kubectl", "admin_srv")]).await;
        let deps = deps_with(
            db.clone(),
            Arc::new(ScriptedProvider::new(Vec::new())),
            SpyFactory::new(),
        );
        let session = SessionId::new("s-requis").expect("identifiant de session de test");
        inserer_session(&deps, &alice, &session, "moncontexte", SessionKind::Work).await;

        let refus = assemble_session(&deps, &alice, SessionKind::Work, "dev", &session, None)
            .await
            .expect_err("le role requis vers un serveur interdit echoue");
        match &refus {
            LymailleError::Forbidden(message) => assert_eq!(message, "mcp server unavailable"),
            autre => panic!("`Forbidden` attendue, obtenu : {autre:?}"),
        }
    }

    /// Scenario « un rôle optionnel refusé est omis avec avis » : succès, `notices` nomme le
    /// rôle, aucun client monté pour lui.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_role_optionnel_refuse_est_omis_avec_avis() {
        let db = db_test().await;
        wire_catalog(&db).await;
        let alice = compte(&db, "alice", false).await;
        put_type(&db, "prodtype", &[("meteo", false)]).await;
        put_active_toolset(&db, &toolset("G_kube", "1.0.0", Some("prodtype")), None).await;
        put_agent(
            &db,
            &alice,
            &agent("dev", AgentKind::Work, vec!["G_kube".to_owned()]),
        )
        .await;
        inserer_serveur(
            &db,
            "admin_srv",
            "http://espion.invalid/admin",
            "none",
            json!(["admin"]),
        )
        .await;
        inserer_contexte_prive(&db, alice.user_id, "moncontexte", &[("meteo", "admin_srv")]).await;
        let spy = SpyFactory::new();
        let deps = deps_with(
            db.clone(),
            Arc::new(ScriptedProvider::new(Vec::new())),
            Arc::clone(&spy),
        );
        let session = SessionId::new("s-optional").expect("identifiant de session de test");
        inserer_session(&deps, &alice, &session, "moncontexte", SessionKind::Work).await;

        let assembled = assemble_session(&deps, &alice, SessionKind::Work, "dev", &session, None)
            .await
            .expect("lassemblage reussit sans le role optionnel");
        assert!(
            assembled.notices().iter().any(|notice| notice.contains("meteo")),
            "l'avis nomme le role omis : {:?}",
            assembled.notices()
        );
        assert_eq!(spy.calls(), 0, "aucun client MCP n'est monte pour le role omis");
    }

    /// Scenario « seul l'actif est employé » : `active` v1 + `draft` v2 → v1 montée.
    #[tokio::test(flavor = "multi_thread")]
    async fn seul_lactif_est_employe() {
        let db = db_test().await;
        wire_catalog(&db).await;
        let alice = compte(&db, "alice", false).await;
        put_active_toolset(
            &db,
            &toolset("G_util", "1.0.0", None),
            Some(&toolset("G_util", "2.0.0", None)),
        )
        .await;
        put_agent(
            &db,
            &alice,
            &agent("dev", AgentKind::Work, vec!["G_util".to_owned()]),
        )
        .await;
        let deps = deps_with(
            db.clone(),
            Arc::new(ScriptedProvider::new(Vec::new())),
            SpyFactory::new(),
        );
        let session = SessionId::new("s-actif").expect("identifiant de session de test");
        inserer_session(&deps, &alice, &session, "", SessionKind::Work).await;

        let assembled = assemble_session(&deps, &alice, SessionKind::Work, "dev", &session, None)
            .await
            .expect("lassemblage monte la version active");
        assert_eq!(
            assembled.toolset_versions().get("G_util").map(String::as_str),
            Some("1.0.0"),
            "la version active, jamais le brouillon"
        );
    }

    /// Scenario « une session Config n'a aucun client MCP » : modules de configuration
    /// présents (`sessions::list` exécutable), zéro connecteur monté.
    #[tokio::test(flavor = "multi_thread")]
    async fn une_session_config_na_aucun_client_mcp() {
        let db = db_test().await;
        wire_catalog(&db).await;
        let alice = compte(&db, "alice", false).await;
        inserer_serveur(
            &db,
            "public_srv",
            "http://espion.invalid/public",
            "none",
            json!(["all"]),
        )
        .await;
        // La définition Config n'a pas de toolset : les liaisons du contexte ne montent rien.
        put_agent(&db, &alice, &agent("cfg", AgentKind::Config, Vec::new())).await;
        let spy = SpyFactory::new();
        let deps = deps_with(
            db.clone(),
            Arc::new(ScriptedProvider::new(vec![Ok(final_text("config"))])),
            Arc::clone(&spy),
        );
        let session = SessionId::new("s-config").expect("identifiant de session de test");
        inserer_session(&deps, &alice, &session, "", SessionKind::Config).await;

        let mut assembled = assemble_session(&deps, &alice, SessionKind::Config, "cfg", &session, None)
            .await
            .expect("lassemblage Config reussit");
        assert_eq!(spy.calls(), 0, "aucun client MCP n'est monte pour Config");
        // Le module de configuration est enregistre : `sessions::list()` s'execute.
        let pad = Scratchpad::new(assembled.engine());
        let clock = SystemClock::new();
        let redactor = Redactor::default();
        let env = RunEnv::new("cfg", &redactor, &clock);
        let outcome = pad.run("sessions::list()", &CancelHandle::new(), &env).await;
        assert!(
            matches!(outcome.status(), RunStatus::Ok),
            "le module `sessions` des sessions Config est monte : {:?}",
            outcome.status()
        );
        let _ = assembled.run_turn("bonjour").await;
    }

    /// Scenario « le jeton forward n'atteint que son serveur » : deux serveurs liés, seul le
    /// `forward` reçoit l'en-tête porteur.
    #[tokio::test(flavor = "multi_thread")]
    async fn le_jeton_forward_natteint_que_son_serveur() {
        let db = db_test().await;
        wire_catalog(&db).await;
        let alice = compte(&db, "alice", false).await;
        put_type(&db, "prodtype", &[("public_role", true), ("fwd_role", true)]).await;
        put_active_toolset(&db, &toolset("G_kube", "1.0.0", Some("prodtype")), None).await;
        put_agent(
            &db,
            &alice,
            &agent("dev", AgentKind::Work, vec!["G_kube".to_owned()]),
        )
        .await;
        inserer_serveur(
            &db,
            "srv_none",
            "http://espion.invalid/none",
            "none",
            json!(["all"]),
        )
        .await;
        inserer_serveur(
            &db,
            "srv_fwd",
            "http://espion.invalid/fwd",
            "forward",
            json!(["all"]),
        )
        .await;
        inserer_contexte_prive(
            &db,
            alice.user_id,
            "moncontexte",
            &[("public_role", "srv_none"), ("fwd_role", "srv_fwd")],
        )
        .await;
        let spy = SpyFactory::new();
        let deps = deps_with(
            db.clone(),
            Arc::new(ScriptedProvider::new(Vec::new())),
            Arc::clone(&spy),
        );
        let session = SessionId::new("s-token").expect("identifiant de session de test");
        inserer_session(&deps, &alice, &session, "moncontexte", SessionKind::Work).await;

        let _assembled = assemble_session(
            &deps,
            &alice,
            SessionKind::Work,
            "dev",
            &session,
            Some(Zeroizing::new("jeton-tour".to_owned())),
        )
        .await
        .expect("lassemblage monte les deux serveurs autorises");
        let log = spy.log();
        let vu = |url: &str| {
            log.iter()
                .find(|(seen, _)| seen == url)
                .map(|(_, bearer)| *bearer)
        };
        assert_eq!(
            vu("http://espion.invalid/fwd"),
            Some(true),
            "le serveur forward recoit le porteur"
        );
        assert_eq!(
            vu("http://espion.invalid/none"),
            Some(false),
            "le serveur sans auth ne recoit rien"
        );
    }

    /// Scenario « contexte d'un autre utilisateur » : la ligne de `bob` n'est jamais lue ; le
    /// rôle requis, sans liaison, échoue comme si le contexte était absent.
    #[tokio::test(flavor = "multi_thread")]
    async fn contexte_dun_autre_utilisateur() {
        let db = db_test().await;
        wire_catalog(&db).await;
        let alice = compte(&db, "alice", false).await;
        let bob = compte(&db, "bob", false).await;
        put_type(&db, "prodtype", &[("meteo", true)]).await;
        put_active_toolset(&db, &toolset("G_kube", "1.0.0", Some("prodtype")), None).await;
        put_agent(
            &db,
            &alice,
            &agent("dev", AgentKind::Work, vec!["G_kube".to_owned()]),
        )
        .await;
        // Le contexte de `bob`, du même nom, lie le rôle à un serveur que la table autorise :
        // s'il était lu, un client serait monté (compteur d'espion).
        inserer_serveur(
            &db,
            "public_srv",
            "http://espion.invalid/public",
            "none",
            json!(["all"]),
        )
        .await;
        inserer_contexte_prive(&db, bob.user_id, "commun", &[("meteo", "public_srv")]).await;
        let spy = SpyFactory::new();
        let deps = deps_with(
            db.clone(),
            Arc::new(ScriptedProvider::new(Vec::new())),
            Arc::clone(&spy),
        );
        let session = SessionId::new("s-autre").expect("identifiant de session de test");
        inserer_session(&deps, &alice, &session, "commun", SessionKind::Work).await;

        let refus = assemble_session(&deps, &alice, SessionKind::Work, "dev", &session, None)
            .await
            .expect_err("le contexte d'autrui ne lie rien pour alice");
        assert!(
            matches!(&refus, LymailleError::Forbidden(message) if message == "mcp server unavailable"),
            "sans liaison lisible, le role requis echoue : {refus:?}"
        );
        assert_eq!(
            spy.calls(),
            0,
            "le serveur lie par le contexte de bob n'est jamais contacte"
        );
    }

    /// Un rôle requis en `forward` sans jeton de tour est refusé (`build_auth`, phrase fixe).
    #[tokio::test(flavor = "multi_thread")]
    async fn forward_sans_jeton_refuse_assemblage() {
        let db = db_test().await;
        wire_catalog(&db).await;
        let alice = compte(&db, "alice", false).await;
        put_type(&db, "prodtype", &[("fwd_role", true)]).await;
        put_active_toolset(&db, &toolset("G_kube", "1.0.0", Some("prodtype")), None).await;
        put_agent(
            &db,
            &alice,
            &agent("dev", AgentKind::Work, vec!["G_kube".to_owned()]),
        )
        .await;
        inserer_serveur(
            &db,
            "srv_fwd",
            "http://espion.invalid/fwd",
            "forward",
            json!(["all"]),
        )
        .await;
        inserer_contexte_prive(&db, alice.user_id, "moncontexte", &[("fwd_role", "srv_fwd")]).await;
        let deps = deps_with(
            db.clone(),
            Arc::new(ScriptedProvider::new(Vec::new())),
            SpyFactory::new(),
        );
        let session = SessionId::new("s-nu").expect("identifiant de session de test");
        inserer_session(&deps, &alice, &session, "moncontexte", SessionKind::Work).await;

        let refus = assemble_session(&deps, &alice, SessionKind::Work, "dev", &session, None)
            .await
            .expect_err("un forward sans jeton de tour est refuse");
        assert!(
            matches!(&refus, LymailleError::Forbidden(message)
                if message == "forward auth needs an interactive session"),
            "la phrase fixe de `build_auth` : {refus:?}"
        );
    }

    /// Toolset inconnu : `NotFound`, sans effet.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_toolset_inconnu_aborde_assemblage() {
        let db = db_test().await;
        wire_catalog(&db).await;
        let alice = compte(&db, "alice", false).await;
        put_agent(
            &db,
            &alice,
            &agent("dev", AgentKind::Work, vec!["G_absent".to_owned()]),
        )
        .await;
        let deps = deps_with(
            db.clone(),
            Arc::new(ScriptedProvider::new(Vec::new())),
            SpyFactory::new(),
        );
        let session = SessionId::new("s-absent").expect("identifiant de session de test");
        inserer_session(&deps, &alice, &session, "", SessionKind::Work).await;

        let refus = assemble_session(&deps, &alice, SessionKind::Work, "dev", &session, None)
            .await
            .expect_err("un toolset absent arrete lassemblage");
        assert!(
            matches!(&refus, LymailleError::NotFound(message) if message.contains("G_absent")),
            "le toolset est nomme : {refus:?}"
        );
    }
}
