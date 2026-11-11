//! Kind natif `"subagent"` (features `workflow` **et** `lymaille`) : sous-agent `lymaille` en
//! run batch autonome depuis un workflow (`src/workflow/subagent_step.sdd`, DSD `FR-6`,
//! `DEC-10` à `DEC-12`).
//!
//! Un seul `ctx.run` : le kind est ordinaire, le run entier est la fermeture du `ctx.run` du
//! dispatcher. La session porte un nom déterministe dérivé de `(run_key, step_id)` : à un rejeu,
//! la session sauvegardée est reprise (`restore_session`), jamais recommencée. Le step tourne
//! sous le `subject` du déclencheur (`RunInfo::trigger`) ; sans identité il échoue sans repli.
//! Le plafond `max_secs` (défaut [`DEFAULT_MAX_SECS`], borné par le plafond d'application
//! `max_secs_ceiling` — défaut [`DEFAULT_LONG_CEILING_SECS`, amendement DEC-29]) est appliqué par
//! l'annulation coopérative `CancelHandle`. Le kind est routé vers le handler dédié `execute_long`
//! du dispatcher, dont les bornes couvrent ce plafond (DSD `OI-4`, `OI-11`).
//!
//! L'application enregistre elle-même le kind : `registry.register(SubagentStep::new(deps))`.
//! Amendement lot 2 (DSD `DEC-20`, `DEC-21`) : le module `resources` écrit sous `WriteScope`
//! `(run_key, step_key)` — idempotent au rejeu par le journal d'écritures — et les rôles MCP du
//! contexte lié passent par `mcp_access` sans session interactive : un `forward` est refusé.
//! Amendement R15 (DSD `DEC-27`, `DEC-29`) : le plafond d'application se règle par
//! `with_max_secs_ceiling` (l'application qui le change lie `recommended_options_for` à la même
//! valeur) ; `SubagentDeps::host` donne à `resolve_role` l'autorité de l'hôte sur les rôles MCP
//! liés, sous le principal du déclencheur. Le montage des rôles (`./mcp_mount.sdd`, `mount_role`)
//! ajoute chaque client au moteur du run avant le premier tour : un échec est fatal.
//! Aucune erreur, aucune trace ne contient un prompt, un secret, un contenu de session ou de
//! ressource.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use zeroize::Zeroizing;

use ::lymaille::AgentDefinition;
use ::lymaille::AgentKind;
use ::lymaille::AgentRef;
use ::lymaille::AgentStore as _;
use ::lymaille::CancelHandle;
use ::lymaille::ContextError;
use ::lymaille::ContextLevel;
use ::lymaille::ContextStore as _;
use ::lymaille::DEFAULT_BATCH_MAX_TURNS;
use ::lymaille::DEFAULT_CORRECTIONS;
use ::lymaille::DEFAULT_SUMMARY_PROMPT;
use ::lymaille::DefaultStrategy;
use ::lymaille::EventSink;
use ::lymaille::GrantedModule;
use ::lymaille::Grants;
use ::lymaille::InferenceError;
use ::lymaille::InferenceProvider;
use ::lymaille::InferenceRequest;
use ::lymaille::InferenceResponse;
use ::lymaille::McpEnv;
use ::lymaille::McpRetryPolicy;
use ::lymaille::MemoryLevel;
use ::lymaille::MemoryScopes;
use ::lymaille::ModelSnapshot;
use ::lymaille::NullSink;
use ::lymaille::ProfileStore as _;
use ::lymaille::Provenance;
use ::lymaille::Provider;
use ::lymaille::ProviderStore as _;
use ::lymaille::ReplayMode;
use ::lymaille::ResolvedProfile;
use ::lymaille::RestoredSession;
use ::lymaille::ResultCollector;
use ::lymaille::RigConfig;
use ::lymaille::RigProvider;
use ::lymaille::Scratchpad;
use ::lymaille::ScratchpadEngine;
use ::lymaille::ScratchpadLimits;
use ::lymaille::SegmentContext;
use ::lymaille::SessionId;
use ::lymaille::SessionKind;
use ::lymaille::SessionState;
use ::lymaille::SessionStore as _;
use ::lymaille::SubagentOutcome;
use ::lymaille::SubagentStatus;
use ::lymaille::Summarizer;
use ::lymaille::SystemClock;
use ::lymaille::TodoList;
use ::lymaille::Toolset;
use ::lymaille::ToolsetContext;
use ::lymaille::ToolsetStore as _;
use ::lymaille::TurnConfig;
use ::lymaille::TurnRunner;
use ::lymaille::assemble;
use ::lymaille::memory_module;
use ::lymaille::restore_session;
use ::lymaille::result_module;
use ::lymaille::rhai_pad_definition;
use ::lymaille::run_batch;
use ::lymaille::todo_module;

use sea_orm::DatabaseConnection;

use crate::lymaille::error::LymailleError;
use crate::lymaille::host::SharedHost;
use crate::lymaille::identity::{ADMIN_ONLY_MODULES, CallerPrincipal, Identity, check_grants};
use crate::lymaille::install_crypto_provider;
use crate::lymaille::mcp_mount::{McpConnectorFactory, MountDeps, RmcpConnectorFactory, mount_role};
use crate::lymaille::resource_module::{ResourceRegistry, resources_module};
use crate::lymaille::secret::SecretResolver;
use crate::lymaille::store::LymailleStores;
use crate::lymaille::store::SeaSessionStore;
use crate::lymaille::store::WriteScope;

use super::dispatcher::{DEFAULT_LONG_CEILING_SECS, LONG_CEILING_MAX_SECS};
use super::step::MiryadWorkflowStep;
use super::step::RunInfo;
use super::step::StepError;

/// Plafond par défaut du run d'un sous-agent, en secondes (DSD `OI-4`, décidé par `DEC-10`) —
/// strictement sous la borne ≈ 11 min d'un `ctx.run` par défaut.
pub const DEFAULT_MAX_SECS: u64 = 480;

/// Type de contexte sous lequel le step lit ses rôles MCP liés, et nom du contexte de toolsets des
/// sessions qu'il produit : la même clé partout, pour que la reprise retrouve un contexte homologue.
/// Identifiant valide au sens de `identity::classify` (`_`, jamais `-` : un nom avec `-` ne serait
/// jamais relu).
///
/// **Contrat** : c'est le nom sous lequel l'utilisateur lie ses serveurs MCP au step. Un renommage
/// laisse silencieusement sans effet les contextes déjà liés en base.
pub const SUBAGENT_CONTEXT_TYPE: &str = "workflow_subagent";

/// Requêtes d'inférence par tour de batch : `1`. Un tour est alors « une requête, un appel d'outil
/// éventuel » ; le résultat de l'outil ouvre le tour suivant du batch (message de rappel de
/// `lymaille`), jamais une seconde requête du même tour. C'est la valeur que verrouille le
/// `Scenario` « un rejeu reprend la session sauvegardée » : la reprise n'adresse qu'une seule
/// requête au provider. Un run complet dispose de `max_turns` requêtes (défaut
/// `DEFAULT_BATCH_MAX_TURNS`).
const MAX_INFERENCES_PER_TURN: u32 = 1;

/// Nom de session déterministe d'un step : `"wf-"` suivi des 32 premiers hexadécimaux de
/// `sha256(run_key + "\0" + step_id)` — 35 caractères. Un rejeu (la fermeture du `ctx.run`
/// relancée depuis zéro après une coupure) retrouve la même session (`DEC-10`).
pub(crate) fn session_name(run: &RunInfo) -> String {
    let mut hasher = Sha256::new();
    hasher.update(run.run_key.as_bytes());
    hasher.update(b"\0");
    hasher.update(run.step_id.as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!("wf-{}", digest.chars().take(32).collect::<String>())
}

/// Les trois dépendances que l'application fournit au step `subagent` (`SubagentDeps`).
pub struct SubagentDeps {
    /// Connexion à la base de l'application (identité, stores, module des ressources).
    pub db: DatabaseConnection,
    /// Résolveur des références d'authentification des providers (`src/lymaille/secret.sdd`).
    pub secrets: Arc<dyn SecretResolver>,
    /// Registre des entités lues **et écrites** par les agents — amendement lot 2 (DSD `DEC-21`) :
    /// `readers` devient `registry`, un [`ResourceRegistry`] (`src/lymaille/resource_module.sdd`).
    pub registry: Arc<ResourceRegistry>,
    /// Extension de l'application hôte (`src/lymaille/host.sdd`) — amendement DEC-27 : les rôles
    /// MCP liés qu'elle pourvoit sont tranchés par sa décision, sous le principal du
    /// déclencheur. `None` par défaut : seule la liaison classique de `mcp_servers` répond.
    pub host: Option<SharedHost>,
}

/// Couture d'inférence du step (test) : produit le client d'inférence d'un provider, là où la
/// production construit un [`RigProvider`]. Aucun réseau dans les tests : un `ScriptedProvider`
/// de `lymaille` tient lieu de client.
trait InferenceFactory: Send + Sync {
    /// Client pour l'adresse et la clé du provider résolu. `api_key` est déjà sortie de son
    /// `Zeroizing` (le constructeur de `RigConfig` exige un `String`) ; nulle trace n'en porte
    /// la valeur.
    fn make(&self, address: String, api_key: Option<String>) -> Arc<dyn DynInference>;
}

/// Client d'inférence sous forme objet (couture interne) : [`InferenceProvider`] de `lymaille`
/// n'est pas object-safe (méthodes `impl Future`), le pas d'indirection est posé ici.
trait DynInference: Send + Sync {
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

/// Fabrique de production : un [`RigProvider`] par provider résolu (feature `rig` de `lymaille`,
/// activée par la feature `lymaille` de la crate).
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

/// Summarizer du lot 1 : `lymaille` 0.1.0 ne fournit aucun summarizer de production et le step
/// n'en câble aucun. La compactation n'est jamais atteinte dans un run batch de ≤ 8 tours sous
/// le budget du profil ; si elle l'était, l'échec est rapporté honnêtement comme échec de tour
/// (jamais un résumé vide fabriqué).
struct NoSummarizer;

impl Summarizer for NoSummarizer {
    fn summarize(
        &self,
        _transcript: &str,
        _digest: Option<&::lymaille::Digest>,
    ) -> impl Future<Output = Result<String, ContextError>> + Send {
        std::future::ready(Err(ContextError::Summarizer(
            "no summarizer is wired for workflow sub-agent runs".to_owned(),
        )))
    }
}

/// Kind `"subagent"` : run batch d'un sous-agent `lymaille` sous le sujet du déclencheur.
pub struct SubagentStep {
    deps: SubagentDeps,
    inference: Arc<dyn InferenceFactory>,
    /// Plafond d'application de `max_secs` (amendement DEC-29) : la validation de la `config`
    /// refuse tout `max_secs` hors de `1..=max_secs_ceiling`. Sans `with_max_secs_ceiling`,
    /// [`DEFAULT_LONG_CEILING_SECS`] ; l'application qui le change lie le handler
    /// `execute_long` à la même valeur par `recommended_options_for` (./dispatcher.sdd).
    max_secs_ceiling: u64,
    /// Couture des connecteurs MCP du montage (`./mcp_mount.sdd`) : [`RmcpConnectorFactory`] en
    /// production, un connecteur espion en test. Privée (amendement DEC-27, accord du développeur
    /// principal) : `SubagentDeps` garde sa forme de littéral publique.
    mcp: Arc<dyn McpConnectorFactory>,
}

impl SubagentStep {
    /// Construit le step autour des dépendances de l'application, avec les fabriques de
    /// production (`rig`, `rmcp`) et le plafond d'application par défaut
    /// ([`DEFAULT_LONG_CEILING_SECS`], DEC-29).
    #[must_use]
    pub fn new(deps: SubagentDeps) -> Self {
        Self {
            deps,
            inference: Arc::new(RigInferenceFactory),
            max_secs_ceiling: DEFAULT_LONG_CEILING_SECS,
            mcp: Arc::new(RmcpConnectorFactory),
        }
    }

    /// Fixe le plafond d'application de `max_secs` (amendement DEC-29) : `ceiling` dans
    /// `1..=LONG_CEILING_MAX_SECS`, strictement couvert par les bornes du handler `execute_long`
    /// que l'application lie à la même valeur (`recommended_options_for(ceiling)`).
    ///
    /// # Errors
    ///
    /// [`LymailleError::InvalidDefinition`] si `ceiling` est hors de `1..=LONG_CEILING_MAX_SECS`
    /// (`"max_secs ceiling out of range"`).
    pub fn with_max_secs_ceiling(self, ceiling: u64) -> Result<Self, LymailleError> {
        if !(1..=LONG_CEILING_MAX_SECS).contains(&ceiling) {
            return Err(LymailleError::InvalidDefinition(
                "max_secs ceiling out of range".to_owned(),
            ));
        }
        Ok(Self {
            max_secs_ceiling: ceiling,
            ..self
        })
    }

    /// Construit le step avec une fabrique d'inférence de rechange (tests unitaires uniquement —
    /// un `ScriptedProvider` remplace `RigProvider`, aucun réseau).
    #[cfg(test)]
    fn with_inference(deps: SubagentDeps, factory: Arc<dyn InferenceFactory>) -> Self {
        Self::with_seams(deps, factory, Arc::new(RmcpConnectorFactory))
    }

    /// Construit le step avec les deux coutures de test : inférence (`ScriptedProvider`) et
    /// connecteurs MCP (espion). Aucun réseau.
    #[cfg(test)]
    fn with_seams(
        deps: SubagentDeps,
        factory: Arc<dyn InferenceFactory>,
        mcp: Arc<dyn McpConnectorFactory>,
    ) -> Self {
        Self {
            deps,
            inference: factory,
            max_secs_ceiling: DEFAULT_LONG_CEILING_SECS,
            mcp,
        }
    }
}

/// `config` du kind `"subagent"`, après rendu de template (`task` peut référencer `inputs`).
/// Clés inconnues refusées.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubagentConfig {
    agent: String,
    task: String,
    max_secs: Option<u64>,
    max_turns: Option<u32>,
}

/// Étape 1 — échec de parse ou de validation de la `config` : non retryable, aucun accès.
fn invalid(detail: impl std::fmt::Display) -> StepError {
    StepError {
        message: format!("config subagent invalide: {detail}"),
        retryable: false,
    }
}

/// Échec non retryable à message fixe (étapes 2 à 11).
fn permanent(message: impl Into<String>) -> StepError {
    StepError {
        message: message.into(),
        retryable: false,
    }
}

/// Erreur de [`LymailleError`] → [`StepError`] : le message est le `Display` de l'erreur (code
/// `MRD-LYMAILLE-NNN`, jamais de secret ni de contenu de session), `retryable` suit sa décision.
/// S'applique aux étapes 1 à 8 (rien n'a encore été exécuté).
fn from_lymaille(error: &LymailleError) -> StepError {
    StepError {
        retryable: error.retryable(),
        message: error.to_string(),
    }
}

/// Erreur d'un store de définitions (`AgentsError`) aux étapes 3 à 6 : une panne de stockage est
/// retryable (comme `LymailleError::Store`), tout refus de l'hôte ne l'est pas.
fn from_agents(error: ::lymaille::AgentsError) -> StepError {
    match error {
        ::lymaille::AgentsError::Store(text) => from_lymaille(&LymailleError::Store(text)),
        other => permanent(other.to_string()),
    }
}

/// Erreur d'un store de toolsets (`ToolsetsError`) aux étapes 7 et 8 — même règle que
/// [`from_agents`].
fn from_toolsets(error: ::lymaille::ToolsetsError) -> StepError {
    match error {
        ::lymaille::ToolsetsError::Store(text) => from_lymaille(&LymailleError::Store(text)),
        other => permanent(other.to_string()),
    }
}

/// Erreur de session (`SessionError`) aux étapes 8 et 9 — même règle que [`from_agents`].
fn from_session(error: ::lymaille::SessionError) -> StepError {
    match error {
        ::lymaille::SessionError::Store(text) => from_lymaille(&LymailleError::Store(text)),
        other => permanent(other.to_string()),
    }
}

/// Validation pure de la `config` (`Must` étape 1), avant tout accès. Le `max_secs` demandé est
/// borné par le plafond d'application `ceiling` (amendement DEC-29, défaut
/// [`DEFAULT_LONG_CEILING_SECS`]).
fn validate(cfg: &SubagentConfig, ceiling: u64) -> Result<(), StepError> {
    if cfg.agent.is_empty() {
        return Err(invalid("agent vide"));
    }
    if cfg.task.trim().is_empty() {
        return Err(invalid("task vide"));
    }
    if let Some(max_secs) = cfg.max_secs
        && !(1..=ceiling).contains(&max_secs)
    {
        return Err(invalid(format!("max_secs hors de 1..={ceiling} : {max_secs}")));
    }
    if cfg.max_turns == Some(0) {
        return Err(invalid("max_turns doit être supérieur ou égal à 1"));
    }
    Ok(())
}

/// Raiso fixed d'un `SubagentOutcome` `Failed` : la raison posée par `lymaille` (phrase fixe),
/// sinon les lieux de schéma de la dernière soumission (jamais une valeur soumise), sinon une
/// phrase fixe.
fn failure_reason(outcome: &SubagentOutcome) -> String {
    if let Some(reason) = outcome.reason() {
        return reason.to_owned();
    }
    outcome
        .errors()
        .first()
        .map_or_else(|| "no valid result was accepted".to_owned(), String::clone)
}

/// Étapes 9 à 11 — l'issue du run batch. `Valid` rend la valeur soumise et validée ;
/// `Cancelled` est le plafond atteint ; `Failed`/erreur de run est un échec non retryable (le
/// run peut avoir eu des effets — DSD `DEC-11`).
fn outcome_to_result(outcome: &SubagentOutcome) -> Result<Value, StepError> {
    match outcome.status() {
        SubagentStatus::Valid => outcome
            .result()
            .cloned()
            .ok_or_else(|| permanent("agent run failed: valid outcome without a result")),
        SubagentStatus::Cancelled => Err(permanent("agent run exceeded max_secs")),
        SubagentStatus::Failed | SubagentStatus::Interrupted => Err(permanent(format!(
            "agent run failed: {}",
            failure_reason(outcome)
        ))),
        // Statut ajouté par une version mineure de `lymaille` : jamais reconnu au lot 1, il ne
        // vaut pas un succès — échec non retryable, comme `Failed`.
        _ => Err(permanent("agent run failed: unknown outcome status")),
    }
}

/// Issue des étapes 3 à 6 : tout ce que le moteur construit, résolu sous le `subject` du
/// déclencheur (le principal est reconstruit depuis la table des utilisateurs à chaque run —
/// jamais de droits mis en cache).
struct Resolved {
    /// Fabrique des stores `lymaille`, liée au sujet du déclencheur.
    stores: LymailleStores,
    /// Principal reconstruit du déclencheur (droits relus à l'exécution, DSD `FR-4`) — relu à
    /// l'étape 7 pour le contrôle d'usage des serveurs MCP liés.
    caller: CallerPrincipal,
    /// Définition lue du sous-agent.
    definition: AgentDefinition,
    /// Profil effectif de la définition.
    profile: ResolvedProfile,
    /// Client d'inférence du provider (production : `rig` ; la couture de test le remplace).
    client: Arc<dyn DynInference>,
    /// Clé d'API résolue de la référence d'authentification du provider ; enregistrée auprès du
    /// `Redactor` de la session, elle n'en sort jamais.
    api_key: Option<Zeroizing<String>>,
}

impl SubagentStep {
    /// Étapes 3 à 6 : crypto, principal, définition (nature `Sub`, schéma de sortie présent),
    /// défense en profondeur sur les modules (DSD `FR-9`), profil, provider, secret, client.
    async fn resolve(&self, agent: &str, trigger: &str) -> Result<Resolved, StepError> {
        install_crypto_provider();
        let caller = Identity::new(self.deps.db.clone())
            .resolve(trigger)
            .await
            .map_err(|error| from_lymaille(&error))?;
        let stores = LymailleStores::new(self.deps.db.clone(), trigger.to_owned());
        let definition: AgentDefinition = stores
            .agents()
            .get_agent(agent)
            .await
            .map_err(from_agents)?
            .ok_or_else(|| permanent(format!("unknown agent: {agent}")))?;
        if definition.kind() != AgentKind::Sub {
            return Err(permanent("agent is not a sub-agent"));
        }
        definition
            .output_schema()
            .ok_or_else(|| permanent("sub-agent has no output schema"))?;
        check_grants(&caller, definition.modules(), ADMIN_ONLY_MODULES)
            .map_err(|error| from_lymaille(&error))?;
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
            .ok_or_else(|| permanent("inference provider is unknown"))?;
        let api_key: Option<Zeroizing<String>> = provider
            .auth()
            .map(|reference| self.deps.secrets.resolve(reference))
            .transpose()
            .map_err(|error| from_lymaille(&error))?;
        let client = self.inference.make(
            provider.address().to_owned(),
            api_key.as_deref().map(ToString::to_string),
        );
        Ok(Resolved {
            stores,
            caller,
            definition,
            profile,
            client,
            api_key,
        })
    }
}

/// Contexte de construction des modules accordés (étape 7) : les objets déjà disponibles, rien
/// de plus — un module dont la construction exigerait davantage n'est pas câblé au lot 1.
struct ModuleHost<'a> {
    step: &'a SubagentStep,
    stores: &'a LymailleStores,
    trigger: &'a str,
    session_id: &'a SessionId,
    /// Scope d'idempotence du step batch (amendement lot 2, DSD `DEC-21`) : les clés de la
    /// session `Sub`, sous lesquelles le module `resources` journalise ses écritures.
    scope: &'a WriteScope,
    sink: &'a Arc<dyn EventSink>,
    clock: &'a Arc<dyn ::lymaille::Clock>,
}

/// Étape 7, côté modules : chaque nom accordé par la définition devient un module accordé. Un nom
/// inconnu est un échec non retryable, jamais un repli.
fn granted_modules(host: &ModuleHost<'_>, names: &[String]) -> Result<Vec<GrantedModule>, StepError> {
    let mut granted: Vec<GrantedModule> = Vec::new();
    for name in names {
        let module = match name.as_str() {
            "resources" => resources_module(
                Arc::clone(&host.step.deps.registry),
                host.step.deps.db.clone(),
                host.trigger.to_owned(),
                Some(host.scope.clone()),
            ),
            "todo" => todo_module(
                TodoList::new(),
                host.session_id.clone(),
                Arc::clone(host.sink),
                Arc::clone(host.clock),
            ),
            // La portée `project` de `memory` n'a pas de référence au lot 1 (aucune notion
            // de projet côté hôte — question `[?]` remontée) ; `memory_project` n'est donc
            // pas un nom connu ici.
            "memory_user" | "memory_session" => {
                let level = if name == "memory_user" {
                    MemoryLevel::User
                } else {
                    MemoryLevel::Session
                };
                memory_module(
                    level,
                    Arc::new(host.stores.memories()),
                    MemoryScopes::new(host.trigger, "", host.session_id),
                    Arc::clone(host.sink),
                    Arc::clone(host.clock),
                )
            }
            other => return Err(permanent(format!("unknown module: {other}"))),
        };
        granted.push(
            GrantedModule::new(name, module.into())
                .ok_or_else(|| permanent(format!("invalid module name: {name}")))?,
        );
    }
    Ok(granted)
}

/// Liaisons rôle → serveur d'un contexte, lues de sa forme sérialisée (champ `roles` du JSON
/// stocké — `ToolsetContext` ne rend que les noms des rôles, jamais leurs références ; précédent :
/// `../lymaille/interactive/assemble.rs`).
fn context_roles(context: &ToolsetContext) -> Vec<(String, String)> {
    serde_json::to_value(context)
        .ok()
        .and_then(|value| {
            value.get("roles").and_then(Value::as_object).map(|entries| {
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

/// Échec d'un montage de rôle MCP (étape 7) : message de [`from_lymaille`], `retryable: false`
/// forcé. Le montage précède le premier tour ; un rejeu ne changerait ni un refus d'autorité ni
/// une panne de base qu'on ne veut pas relancer en silence (`./subagent_step.sdd`, Must étape 7).
fn mount_failure(error: &LymailleError) -> StepError {
    StepError {
        retryable: false,
        ..from_lymaille(error)
    }
}

/// `Redactor` des clients MCP du run : la clé d'API résolue du provider y entre à la construction
/// et n'en sort jamais, pour qu'une sortie d'outil qui l'écho soit masquée (`./subagent_step.sdd`,
/// amendement 2026-11-07). Copie locale de `configured_redactor` d'`interactive/assemble.rs` (privée
/// là-bas) : une valeur trop courte est ignorée, jamais fatale.
fn mcp_redactor(api_key: Option<&Zeroizing<String>>) -> ::lymaille::Redactor {
    let mut redactor = ::lymaille::Redactor::default();
    if let Some(secret) = api_key {
        let _ = redactor.add_secret(secret.to_string());
    }
    redactor
}

/// Fin de l'étape 7 (DSD `DEC-27`, `DEC-20`) : chaque rôle MCP du contexte lié à la session
/// (`SUBAGENT_CONTEXT_TYPE`, relu sous le sujet du déclencheur) est **monté** en client par `mount_role`
/// (`./mcp_mount.sdd`), `interactive_token = None`, sous le principal du déclencheur. Les modules
/// obtenus rejoignent les modules accordés du run. Tout échec (refus, panne de l'hôte, `forward`,
/// nom invalide) est fatal et précède le premier tour, sans avis ni repli. Le `Grant` de l'hôte
/// n'est ni persisté ni tracé ; aucun texte de l'hôte ne remonte dans l'erreur.
async fn mount_context_roles(
    step: &SubagentStep,
    resolved: &Resolved,
    session_id: &SessionId,
    sink: &Arc<dyn EventSink>,
    clock: &Arc<dyn ::lymaille::Clock>,
    taken: &[&str],
) -> Result<Vec<GrantedModule>, StepError> {
    let context: Option<ToolsetContext> = resolved
        .stores
        .contexts()
        .get_context(SUBAGENT_CONTEXT_TYPE)
        .await
        .map_err(from_toolsets)?;
    let Some(context) = context else {
        return Ok(Vec::new());
    };
    let deps = MountDeps {
        db: &step.deps.db,
        secrets: &step.deps.secrets,
        host: step.deps.host.as_ref(),
        mcp: step.mcp.as_ref(),
    };
    let mut mounted = Vec::new();
    let redactor = Arc::new(mcp_redactor(resolved.api_key.as_ref()));
    for (role, server) in context_roles(&context) {
        let env = McpEnv::new(
            Arc::clone(&redactor),
            Arc::clone(sink),
            Arc::clone(clock),
            session_id.clone(),
            McpRetryPolicy::default(),
        );
        let module = mount_role(
            &deps,
            &resolved.caller,
            SUBAGENT_CONTEXT_TYPE,
            &role,
            Some(&server),
            None,
            taken,
            env,
        )
        .await
        .map_err(|error| mount_failure(&error))?;
        mounted.push(module);
    }
    Ok(mounted)
}

/// Moteur du run, collecteur de ses résultats soumission et contexte assemblé (étapes 7 et 9).
struct EngineParts {
    /// Moteur du pad, où le module `result` du collecteur est branché.
    engine: Arc<ScratchpadEngine>,
    /// Collecteur des valeurs soumises par l'agent, validées contre le schéma de sortie.
    collector: Arc<ResultCollector>,
    /// Versions des toolsets effectifs, pour la provenance de la session.
    toolset_versions: BTreeMap<String, String>,
    /// Plafond de rendu du pad, entrée du budget de tour.
    render_cap_chars: usize,
}

/// Fin de l'étape 7 et début de l'étape 9 : toolsets actifs, assemblage, module `result`,
/// moteur. Une panne d'assemblage est non retryable — le run ne commence pas.
async fn assemble_and_engine(
    stores: &LymailleStores,
    definition: &AgentDefinition,
    granted_modules: Vec<GrantedModule>,
    schema: &Value,
) -> Result<EngineParts, StepError> {
    let mut toolsets: Vec<Toolset> = Vec::new();
    for name in definition.toolsets() {
        let toolset = stores
            .toolsets()
            .get_active(name)
            .await
            .map_err(from_toolsets)?
            .ok_or_else(|| permanent(format!("unknown toolset: {name}")))?;
        toolsets.push(toolset);
    }
    let context = ToolsetContext::new(SUBAGENT_CONTEXT_TYPE.to_owned(), ContextLevel::Session)
        .map_err(from_toolsets)?
        .resolve(std::iter::empty::<(String, String, Value)>())
        .map_err(from_toolsets)?;
    let limits = ScratchpadLimits::new();
    let assembled = assemble(&toolsets, &granted_modules, &BTreeMap::new(), &context, &limits);
    if let Some(first_failure) = assembled.failures().first() {
        return Err(permanent(format!("toolset assembly failed: {first_failure}")));
    }
    let collector = Arc::new(
        ResultCollector::new(definition.id(), schema, DEFAULT_CORRECTIONS)
            .map_err(|error| permanent(format!("agent run failed: {error}")))?,
    );
    let result_granted = GrantedModule::new("result", result_module(Arc::clone(&collector)).into())
        .ok_or_else(|| permanent("invalid module name: result"))?;
    let grants: Grants = assembled
        .into_grants()
        .with_module(result_granted)
        .ok_or_else(|| permanent("module name collision: result"))?;
    let engine = Arc::new(
        ScratchpadEngine::builder(limits, grants)
            .build()
            .map_err(|error| permanent(format!("agent run failed: {error}")))?,
    );
    let toolset_versions: BTreeMap<String, String> = toolsets
        .iter()
        .map(|toolset| (toolset.name().to_owned(), toolset.version().to_string()))
        .collect();
    Ok(EngineParts {
        engine,
        collector,
        toolset_versions,
        render_cap_chars: limits.render_cap_chars(),
    })
}

/// Étape 8 : la session déterministe — `restore_session` quand elle existe (les avis de
/// provenance de `lymaille` sont transmis à l'agent, DSD `OI-8` : accepté au lot 1, pas
/// d'échec sur empreinte différente), sinon un état neuf de nature `Sub` (`Must not` : la
/// délégation, que la nature `Sub` ne porte pas).
async fn restore_or_new(
    store: &SeaSessionStore,
    session_id: &SessionId,
    engine: &Arc<ScratchpadEngine>,
    definition: &AgentDefinition,
    toolset_versions: &BTreeMap<String, String>,
    clock: &Arc<dyn ::lymaille::Clock>,
) -> Result<(SessionState, Scratchpad, Vec<String>), StepError> {
    if store.load(session_id).await.map_err(from_session)?.is_some() {
        let provenance = Provenance::new(definition.fingerprint(), toolset_versions.clone());
        let restored: RestoredSession = restore_session(store, session_id, Arc::clone(engine), &provenance)
            .await
            .map_err(from_session)?;
        return Ok(restored.into_parts());
    }
    let state = SessionState::new(
        session_id.clone(),
        AgentRef::new(definition.id(), definition.fingerprint()),
        SUBAGENT_CONTEXT_TYPE,
        toolset_versions.clone(),
        clock.now(),
    )
    .with_kind(SessionKind::Sub);
    Ok((state, Scratchpad::new(Arc::clone(engine)), Vec::new()))
}

/// Ce que requiert la construction du `TurnRunner` (étape 9) — groupée pour une signature lisible.
struct RunnerParts<'a> {
    resolved: &'a Resolved,
    session_store: SeaSessionStore,
    state: SessionState,
    pad: Scratchpad,
    sink: &'a Arc<dyn EventSink>,
    clock: &'a Arc<dyn ::lymaille::Clock>,
    render_cap_chars: usize,
}

/// Étape 9 : le `TurnRunner` du principal — le store de session est lié au `subject` du
/// déclencheur, le `Redactor` connaît la clé d'API sans qu'elle n'en sorte.
fn build_runner(
    parts: RunnerParts<'_>,
) -> Result<TurnRunner<DynProvider, DefaultStrategy<NoSummarizer>, SeaSessionStore>, StepError> {
    let mut redactor = ::lymaille::Redactor::new();
    if let Some(secret) = parts.resolved.api_key.as_deref() {
        // Enregistré auprès du Redactor de la session : masqué dans toute trace produite par
        // le pad ; la valeur ne quitte jamais cet objet.
        redactor
            .add_secret(secret.to_owned())
            .map_err(|error| permanent(format!("agent run failed: {error}")))?;
    }
    let budget = parts.resolved.profile.budget(
        u64::try_from(DEFAULT_SUMMARY_PROMPT.chars().count()).unwrap_or(u64::MAX),
        u64::try_from(parts.render_cap_chars).unwrap_or(u64::MAX),
    );
    let turn_config = TurnConfig::new(
        Arc::clone(parts.clock),
        redactor,
        Arc::clone(parts.sink),
        CancelHandle::new(),
        budget,
    )
    .with_max_steps(MAX_INFERENCES_PER_TURN)
    .with_agent(parts.resolved.definition.id());
    let segment = SegmentContext::new(
        parts.resolved.definition.prompt().to_owned(),
        vec![rhai_pad_definition()],
        ModelSnapshot::new(parts.resolved.profile.model(), parts.resolved.profile.replay()),
    );
    Ok(TurnRunner::new(
        DynProvider(Arc::clone(&parts.resolved.client)),
        DefaultStrategy::new(NoSummarizer, ReplayMode::None),
        parts.resolved.profile.clone(),
        segment,
        parts.pad,
        parts.session_store,
        parts.state,
        turn_config,
    ))
}

#[async_trait::async_trait]
impl MiryadWorkflowStep for SubagentStep {
    fn kind(&self) -> &'static str {
        "subagent"
    }

    async fn run(
        &self,
        run: &RunInfo,
        config: Value,
        _inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        // ── 1. config (validation pure, aucun accès) ──
        let cfg: SubagentConfig = serde_json::from_value(config).map_err(invalid)?;
        validate(&cfg, self.max_secs_ceiling)?;
        let max_secs = cfg.max_secs.unwrap_or(DEFAULT_MAX_SECS);
        let max_turns = cfg.max_turns.unwrap_or(DEFAULT_BATCH_MAX_TURNS);

        // ── 2. identité du déclencheur — jamais de repli (DSD FR-10) ──
        let trigger = run
            .trigger
            .clone()
            .ok_or_else(|| permanent("subagent step requires a trigger identity"))?;

        // ── 3 à 6. principal, définition, profil, provider, secret, client ──
        let resolved = self.resolve(&cfg.agent, &trigger).await?;
        let schema = resolved
            .definition
            .output_schema()
            .cloned()
            .ok_or_else(|| permanent("sub-agent has no output schema"))?;
        let session_id = SessionId::new(session_name(run))
            .ok_or_else(|| permanent("the deterministic session name is not a valid session identifier"))?;
        let clock: Arc<dyn ::lymaille::Clock> = Arc::new(SystemClock::new());
        let sink: Arc<dyn EventSink> = Arc::new(NullSink::new());

        // ── 7. toolsets et modules accordés → moteur ──
        // Le scope d'idempotence du step batch : les clés de la session `Sub` (amendement lot 2,
        // DSD `DEC-21`) — un rejeu rejourne les écritures sous les mêmes clés du journal.
        let scope = WriteScope {
            run_key: run.run_key.clone(),
            step_key: run.step_id.clone(),
        };
        let mut granted = granted_modules(
            &ModuleHost {
                step: self,
                stores: &resolved.stores,
                trigger: &trigger,
                session_id: &session_id,
                scope: &scope,
                sink: &sink,
                clock: &clock,
            },
            resolved.definition.modules(),
        )?;
        // Rôles MCP du contexte lié (DSD `DEC-27`, `DEC-20`) : chaque rôle est monté en client
        // dans le moteur du run ; refus, panne ou `forward` sont des échecs non retryables, avant
        // le premier tour.
        // Noms réservés à la garde de collision (`./mcp_mount.sdd`, étape 0) : les modules accordés
        // par la définition, et `result`, que `assemble_and_engine` ajoute ensuite.
        let mut taken: Vec<&str> = granted.iter().map(GrantedModule::name).collect();
        taken.push("result");
        let mounted = mount_context_roles(self, &resolved, &session_id, &sink, &clock, &taken).await?;
        granted.extend(mounted);
        let parts = assemble_and_engine(&resolved.stores, &resolved.definition, granted, &schema).await?;

        // ── 8. session déterministe : reprise, ou état neuf `Sub` ──
        let session_store = resolved.stores.sessions();
        let (state, pad, notices) = restore_or_new(
            &session_store,
            &session_id,
            &parts.engine,
            &resolved.definition,
            &parts.toolset_versions,
            &clock,
        )
        .await?;

        // ── 9. moteur du principal ──
        let mut runner = build_runner(RunnerParts {
            resolved: &resolved,
            session_store,
            state,
            pad,
            sink: &sink,
            clock: &clock,
            render_cap_chars: parts.render_cap_chars,
        })?;

        // ── 10. plafond max_secs + run batch ──
        // Les avis de provenance d'une reprise (DSD OI-8) ouvrent le premier message de la tâche.
        let task = if notices.is_empty() {
            cfg.task
        } else {
            format!("{}\n\n{}", notices.join("\n"), cfg.task)
        };
        let cancel = CancelHandle::new();
        let timer = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(max_secs)).await;
                cancel.cancel();
            })
        };
        let outcome = run_batch(&mut runner, &task, &parts.collector, &cancel, max_turns).await;
        timer.abort();

        // ── 11. issue — aucune session n'est supprimée (rétention : DSD OI-7) ──
        match outcome {
            Ok(outcome) => outcome_to_result(&outcome),
            Err(error) => Err(permanent(format!("agent run failed: {error}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::lymaille::definitions::mcp_server;
    use crate::lymaille::host::{HostError, LymailleHost, McpDecision, McpGrant, McpRequest};
    use crate::lymaille::mcp_mount::{DynMcpAuth, DynMcpConnector};
    use crate::lymaille::store::test_support::test_db;
    use crate::resource::{AccessPolicy, MiryadResource};
    use crate::users::membership::sync_group_memberships;
    use crate::users::resolve_user;
    use ::lymaille::ErrorClass;
    use ::lymaille::Message;
    use ::lymaille::ModelProfile;
    use ::lymaille::ProviderLevel;
    use ::lymaille::RHAI_PAD_TOOL;
    use ::lymaille::ScriptedProvider;
    use ::lymaille::ToolCall;
    use ::lymaille::{McpConnector, McpError, McpLister, McpTool};
    use sea_orm::ActiveValue::Set;
    use sea_orm::ConnectionTrait as _;
    use sea_orm::Schema;
    use sea_orm::entity::prelude::*;
    use sea_orm::sea_query::ExprTrait as _;
    use serde::{Deserialize, Serialize};
    use serde_json::json;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── fixtures ------------------------------------------------------------------------------

    /// Ressource `AdminOnly` du `Scenario` « les droits sont ceux du déclencheur » — table locale
    /// créée par `Schema` sur la base migrée (précédent : `src/lymaille/resource_module.rs`).
    mod subsecret_res {
        use super::*;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "llm_sub_secrets")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "subsecret"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    /// Ressource `Public` inscriptible par tous — fixture du `Scenario` lot 2 « un rejeu n'écrit
    /// pas deux fois » (table créée par `Schema` sur la base migrée, ouverte en écriture par
    /// `register`, pas `register_read_only`).
    mod widget_res {
        use super::*;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "llm_widgets")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "widget"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    /// Ligne de `miryad_lymaille_mcp_servers` — fixture du `Scenario` lot 2 « un serveur MCP en
    /// forward fait échouer le step » (forme des fixtures de `../lymaille/mcp_access.rs`).
    async fn inserer_serveur(
        db: &DatabaseConnection,
        nom: &str,
        auth_mode: &str,
        groupes: serde_json::Value,
    ) {
        use sea_orm::ActiveValue::NotSet;
        let actif = mcp_server::ActiveModel {
            id: NotSet,
            name: Set(nom.to_owned()),
            url: Set("https://mcp.example/".to_owned()),
            request_timeout_secs: Set(30),
            connect_timeout_secs: Set(5),
            auth_mode: Set(auth_mode.to_owned()),
            auth_ref: Set(None),
            allowed_groups: Set(groupes),
            created_at: Set(chrono::Utc::now()),
        };
        actif.insert(db).await.expect("ligne mcp_servers de test");
    }

    /// Contexte lié du step (`SUBAGENT_CONTEXT_TYPE`, privé sous `subject`) liant chaque rôle donné à un
    /// serveur — écrit par `subject` lui-même via le store (les hooks de `put_context` valident
    /// la liaison, `store/toolset.sdd`).
    async fn lier_contexte(db: &DatabaseConnection, subject: &str, roles: &[(&str, &str)]) {
        lier_contexte_de_type(db, subject, super::SUBAGENT_CONTEXT_TYPE, roles).await;
    }

    /// Contexte lié de type `context_type`, privé sous `subject` (même écriture que `lier_contexte`).
    async fn lier_contexte_de_type(
        db: &DatabaseConnection,
        subject: &str,
        context_type: &str,
        roles: &[(&str, &str)],
    ) {
        let mut context =
            ToolsetContext::new(context_type.to_owned(), ContextLevel::Session).expect("contexte de test");
        for (role, server) in roles {
            context = context
                .with_role((*role).to_owned(), (*server).to_owned())
                .expect("liaison role-serveur de test");
        }
        LymailleStores::new(db.clone(), subject)
            .contexts()
            .put_context(&context)
            .await
            .expect("le contexte lie est ecrit par son proprietaire");
    }

    /// Nombre de lignes du journal d'écritures pour un `run_key` (table posée par la migration).
    async fn journal_rows(db: &DatabaseConnection, run_key: &str) -> usize {
        let mut select = sea_orm::sea_query::Query::select();
        select
            .column(sea_orm::sea_query::Alias::new("op_key"))
            .from(sea_orm::sea_query::Alias::new("miryad_lymaille_write_journal"))
            .and_where(sea_orm::sea_query::Expr::col(sea_orm::sea_query::Alias::new("run_key")).eq(run_key));
        db.query_all(&select)
            .await
            .expect("comptage des lignes du journal")
            .len()
    }

    /// Nombre de lignes de la ressource fixture.
    async fn widget_rows(db: &DatabaseConnection) -> usize {
        widget_res::Entity::find()
            .all(db)
            .await
            .expect("comptage des lignes de la ressource fixture")
            .len()
    }

    /// Compte local de test, membre ou non du groupe `admin` (`src/lymaille/store/mod.rs`, même
    /// patron : chemin de production `resolve_user` + `sync_group_memberships`).
    async fn user(db: &DatabaseConnection, subject: &str, admin: bool) {
        let account = resolve_user(db, subject, None)
            .await
            .expect("compte local de test créé");
        if admin {
            sync_group_memberships(db, account.id, &["admin".to_owned()])
                .await
                .expect("appartenance au groupe admin posée");
        }
    }

    /// Catalogue câblé par `root` : profil `qwen` (taille de contexte connue), provider `local`
    /// dont l'authentification passe par la référence `FIXED_KEY`.
    async fn wire_catalog(db: &DatabaseConnection) {
        let root = LymailleStores::new(db.clone(), "root");
        root.profiles()
            .put_profile(
                &ModelProfile::new("qwen".to_owned(), "local".to_owned(), "qwen-test".to_owned())
                    .with_context_size(262_144),
            )
            .await
            .expect("l'admin écrit le profil");
        root.providers()
            .put_provider(
                &Provider::new(
                    "local".to_owned(),
                    "openai".to_owned(),
                    "http://inference.invalid/v1".to_owned(),
                    Some("FIXED_KEY".to_owned()),
                    ProviderLevel::User,
                )
                .expect("des références simples font un provider"),
            )
            .await
            .expect("l'admin écrit le provider");
    }

    /// Schéma de sortie des sous-agents de test : un objet à un champ `verdict` requis.
    fn verdict_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": { "verdict": { "type": "string" } },
            "required": ["verdict"],
            "additionalProperties": false
        })
    }

    /// Définition de sous-agent du catalogue, avec schéma de sortie et modules accordés.
    fn sub_agent(id: &str, prompt: &str, modules: Vec<String>) -> AgentDefinition {
        AgentDefinition::new(
            id.to_owned(),
            AgentKind::Sub,
            prompt.to_owned(),
            "qwen".to_owned(),
        )
        .with_output_schema(verdict_schema())
        .with_modules(modules)
    }

    /// Écrit un agent au catalogue par `root`.
    async fn put_catalog_agent(db: &DatabaseConnection, definition: &AgentDefinition) {
        LymailleStores::new(db.clone(), "root")
            .agents()
            .put_agent(definition)
            .await
            .expect("l'admin écrit l'agent du catalogue");
    }

    /// Réponse finale sans appel d'outil.
    fn final_text(text: &str) -> InferenceResponse {
        InferenceResponse::new(Message::assistant(text.to_owned(), None, Vec::new()), None)
    }

    /// Réponse appelant l'outil `rhai_pad` avec `script`, sous l'identifiant d'appel donné — un
    /// rejeu réémet un appel sous un identifiant neuf, jamais celui déjà soldé de la session.
    fn pad_call_id(id: &str, script: &str) -> InferenceResponse {
        InferenceResponse::new(
            Message::assistant(
                String::new(),
                None,
                vec![ToolCall::new(id, RHAI_PAD_TOOL, json!({ "script": script }))],
            ),
            None,
        )
    }

    /// Réponse appelant l'outil `rhai_pad` avec `script`.
    fn pad_call(script: &str) -> InferenceResponse {
        InferenceResponse::new(
            Message::assistant(
                String::new(),
                None,
                vec![ToolCall::new(
                    "call-1",
                    RHAI_PAD_TOOL,
                    json!({ "script": script }),
                )],
            ),
            None,
        )
    }

    /// Script qui soumet `{"verdict": "ok"}` — l'issue attendue des `Scenario` de succès.
    const SUBMIT_OK: &str = r#"result::submit(#{ verdict: "ok" })"#;

    /// Fabrique de la couture : le même provider scripté à chaque run du step ; le test en lit
    /// les requêtes reçues.
    struct ScriptFactory(Arc<ScriptedProvider>);

    impl InferenceFactory for ScriptFactory {
        fn make(&self, _address: String, _api_key: Option<String>) -> Arc<dyn DynInference> {
            let provider: Arc<dyn DynInference> = self.0.clone();
            provider
        }
    }

    /// Fabrique du `Scenario` du plafond : une inférence qui ne revient jamais.
    struct HangInference;

    impl DynInference for HangInference {
        fn complete_dyn(
            &self,
            _request: InferenceRequest,
        ) -> Pin<Box<dyn Future<Output = Result<InferenceResponse, InferenceError>> + Send + '_>> {
            Box::pin(std::future::pending::<Result<InferenceResponse, InferenceError>>())
        }
    }

    struct HangFactory;

    impl InferenceFactory for HangFactory {
        fn make(&self, _address: String, _api_key: Option<String>) -> Arc<dyn DynInference> {
            Arc::new(HangInference)
        }
    }

    /// Résolveur de secrets de test : la référence `FIXED_KEY` rend la valeur fixe, toute autre
    /// référence est `SecretUnresolved`.
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

    /// Le step du scenario : `config` des dépendances, inférence de rechange, registre de
    /// ressources vide sauf fixture contraire.
    fn step_with(db: DatabaseConnection, factory: Arc<dyn InferenceFactory>) -> SubagentStep {
        step_with_registry(db, factory, ResourceRegistry::new())
    }

    /// Le step avec un registre de ressources donné (amendement lot 2 : `SubagentDeps` porte un
    /// `ResourceRegistry`, plus un simple `ResourceReaders`).
    fn step_with_registry(
        db: DatabaseConnection,
        factory: Arc<dyn InferenceFactory>,
        registry: ResourceRegistry,
    ) -> SubagentStep {
        SubagentStep::with_inference(
            SubagentDeps {
                db,
                secrets: Arc::new(FixedSecret("s3cret")),
                registry: Arc::new(registry),
                host: None,
            },
            factory,
        )
    }

    /// Le step avec un hôte applicatif (amendement DEC-27) : les rôles MCP liés passent par sa
    /// décision avant la table.
    fn step_with_host(
        db: DatabaseConnection,
        factory: Arc<dyn InferenceFactory>,
        host: SharedHost,
        mcp: Arc<dyn McpConnectorFactory>,
    ) -> SubagentStep {
        SubagentStep::with_seams(
            SubagentDeps {
                db,
                secrets: Arc::new(FixedSecret("s3cret")),
                registry: Arc::new(ResourceRegistry::new()),
                host: Some(host),
            },
            factory,
            mcp,
        )
    }

    /// Le step du `Scenario` des droits : le registre porte la ressource `AdminOnly`.
    fn step_with_readers(db: DatabaseConnection, factory: Arc<dyn InferenceFactory>) -> SubagentStep {
        let mut registry = ResourceRegistry::new();
        registry.register::<subsecret_res::Entity>();
        step_with_registry(db, factory, registry)
    }

    fn run_info(run_key: &str, step_id: &str, trigger: Option<&str>) -> RunInfo {
        RunInfo {
            run_key: run_key.to_owned(),
            step_id: step_id.to_owned(),
            depth: 0,
            vars: json!({}),
            trigger: trigger.map(str::to_owned),
        }
    }

    fn config(agent: &str, task: &str) -> serde_json::Value {
        json!({ "agent": agent, "task": task })
    }

    /// La session `wf-…` du run, lue sous le `subject` donné (`None` quand invisible).
    async fn session_of(db: &DatabaseConnection, subject: &str, run: &RunInfo) -> Option<SessionState> {
        let id = SessionId::new(session_name(run)).expect("le nom déterministe est un identifiant valide");
        LymailleStores::new(db.clone(), subject)
            .sessions()
            .load(&id)
            .await
            .expect("la session se lit")
    }

    // ── Scenario: sans identité du déclencheur le step échoue sans repli ----------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn sans_identite_du_declencheur_le_step_echoue_sans_repli() {
        let db = test_db().await;
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let step = step_with(db, Arc::new(ScriptFactory(Arc::clone(&provider))));
        let run = run_info("t-sans-identite", "s1", None);

        let error = step
            .run(&run, config("G_sub", "inspecter"), HashMap::new())
            .await
            .expect_err("sans déclencheur le step échoue, jamais sur un principal par défaut");

        assert!(
            !error.retryable,
            "l'échec d'identité n'est pas retryable : {error}"
        );
        assert_eq!(error.message, "subagent step requires a trigger identity");
        assert!(
            provider.requests().is_empty(),
            "aucune requête d'inférence n'est partie"
        );
    }

    // ── Scenario: une config invalide ne touche ni la base ni l'inférence --------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn une_config_invalide_ne_touche_ni_base_ni_inference() {
        let db = test_db().await;
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let step = step_with(db, Arc::new(ScriptFactory(Arc::clone(&provider))));
        let run = run_info("t-config", "s1", Some("alice"));

        let cases: Vec<serde_json::Value> = vec![
            json!({ "agent": "", "task": "inspecter" }),
            json!({ "agent": "G_sub", "task": "  " }),
            json!({ "agent": "G_sub", "task": "inspecter", "max_secs": 0 }),
            // Le `Scenario` citait `541` (plafond figé du lot 1) ; sous le plafond d'application
            // par défaut de 3600 (amendement DEC-29) c'est `3601` qui déborde — même intention,
            // « max_secs hors du plafond en force ».
            json!({ "agent": "G_sub", "task": "inspecter", "max_secs": 3601 }),
            json!({ "agent": "G_sub", "task": "inspecter", "max_turns": 0 }),
            json!({ "agent": "G_sub", "task": "inspecter", "inconnue": true }),
        ];
        for case in cases {
            let error = step
                .run(&run, case.clone(), HashMap::new())
                .await
                .expect_err("la config invalide est refusée : {case}");
            assert!(
                !error.retryable,
                "la config invalide n'est pas retryable : {error}"
            );
            assert!(
                error.message.starts_with("config subagent invalide"),
                "le message est préfixé : {error}"
            );
        }
        assert!(
            provider.requests().is_empty(),
            "la validation pure ne touche ni la base ni l'inférence"
        );
    }

    // ── Scenario: un agent inconnu ou qui n'est pas un sous-agent est refusé ------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn un_agent_inconnu_ou_qui_n_est_pas_un_sous_agent_est_refuse() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "root", true).await;
        put_catalog_agent(
            &db,
            &AgentDefinition::new(
                "G_work".to_owned(),
                AgentKind::Work,
                "agent de travail".to_owned(),
                "qwen".to_owned(),
            ),
        )
        .await;
        put_catalog_agent(
            &db,
            &AgentDefinition::new(
                "G_noschema".to_owned(),
                AgentKind::Sub,
                "sans schéma".to_owned(),
                "qwen".to_owned(),
            ),
        )
        .await;
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let step = step_with(db, Arc::new(ScriptFactory(Arc::clone(&provider))));
        let run = run_info("t-refus", "s1", Some("alice"));

        let unknown = step
            .run(&run, config("G_absent", "inspecter"), HashMap::new())
            .await
            .expect_err("un agent absent est refusé");
        let not_sub = step
            .run(&run, config("G_work", "inspecter"), HashMap::new())
            .await
            .expect_err("un agent de nature `work` n'est pas un sous-agent");
        let no_schema = step
            .run(&run, config("G_noschema", "inspecter"), HashMap::new())
            .await
            .expect_err("un sous-agent sans schéma de sortie est refusé");

        for error in [&unknown, &not_sub, &no_schema] {
            assert!(!error.retryable, "le refus est définitif : {error}");
        }
        assert!(unknown.message.contains("unknown agent"), "{unknown}");
        assert_eq!(not_sub.message, "agent is not a sub-agent");
        assert_eq!(no_schema.message, "sub-agent has no output schema");
        assert_ne!(unknown.message, not_sub.message);
        assert_ne!(not_sub.message, no_schema.message);
        assert!(provider.requests().is_empty(), "aucune inférence n'est partie");
    }

    // ── Scenario: un sous-agent rend sa sortie validée -----------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn un_sous_agent_rend_sa_sortie_validee() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "root", true).await;
        wire_catalog(&db).await;
        put_catalog_agent(&db, &sub_agent("G_sub", "tu rends un verdict", Vec::new())).await;
        let provider = Arc::new(ScriptedProvider::new(vec![Ok(pad_call(SUBMIT_OK))]));
        let step = step_with(db.clone(), Arc::new(ScriptFactory(Arc::clone(&provider))));
        let run = run_info("t-sortie", "s1", Some("alice"));

        let output = step
            .run(&run, config("G_sub", "rends ton verdict"), HashMap::new())
            .await
            .expect("le sous-agent soumet sa sortie");

        assert_eq!(output, json!({ "verdict": "ok" }));
        let saved = session_of(&db, "alice", &run)
            .await
            .expect("la session `wf-…` est sauvegardée sous le subject du déclencheur");
        assert!(
            !saved.history().messages().is_empty(),
            "la session porte les tours du run"
        );
        assert!(
            session_of(&db, "bob", &run).await.is_none(),
            "la session est la session d'`alice`, visible sous son seul subject"
        );
    }

    // ── Scenario: le nom de session est déterministe -------------------------------------------

    #[test]
    fn le_nom_de_session_est_deterministe() {
        let first = session_name(&run_info("run-a", "s1", Some("alice")));
        let second = session_name(&run_info("run-a", "s1", Some("alice")));
        let other_step = session_name(&run_info("run-a", "s2", Some("alice")));

        assert_eq!(first, second, "le même (run_key, step_id) rend le même nom");
        assert_ne!(first, other_step, "un step_id différent change le nom");
        assert!(first.starts_with("wf-"), "le préfixe est `wf-` : {first}");
        assert_eq!(first.chars().count(), 35, "35 caractères : {first}");
    }

    // ── Scenario: un rejeu reprend la session sauvegardée ---------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn un_rejeu_reprend_la_session_sauvegardee() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "root", true).await;
        wire_catalog(&db).await;
        put_catalog_agent(&db, &sub_agent("G_sub", "tu rends un verdict", Vec::new())).await;
        let run = run_info("t-rejeu", "s1", Some("alice"));

        // Le run interrompu : un tour terminé, sauvegardé, sans résultat soumis (`max_turns` 1).
        let first_provider = Arc::new(ScriptedProvider::new(vec![Ok(final_text("MARK-TOUR-SAUVAGE"))]));
        let first = step_with(db.clone(), Arc::new(ScriptFactory(Arc::clone(&first_provider))));
        let interrupted = first
            .run(
                &run,
                json!({ "agent": "G_sub", "task": "inspecter", "max_turns": 1 }),
                HashMap::new(),
            )
            .await
            .expect_err("sans résultat soumis le premier run échoue");
        assert!(!interrupted.retryable, "{interrupted}");
        let saved = session_of(&db, "alice", &run)
            .await
            .expect("le tour terminé est sauvegardé avant la coupure");
        assert!(
            saved.history().messages().iter().any(|message| matches!(
                message,
                Message::Assistant { content, .. } if content.contains("MARK-TOUR-SAUVAGE")
            )),
            "la version sauvegardée de la session finit sur un tour assistant"
        );

        // Le rejeu : la fermeture du `ctx.run` repart de zéro, même `(run_key, step_id)`.
        let replay_provider = Arc::new(ScriptedProvider::new(vec![Ok(pad_call(SUBMIT_OK))]));
        let replay = step_with(db.clone(), Arc::new(ScriptFactory(Arc::clone(&replay_provider))));
        let output = replay
            .run(&run, config("G_sub", "inspecter"), HashMap::new())
            .await
            .expect("le rejeu rend le résultat");

        assert_eq!(output, json!({ "verdict": "ok" }));
        let requests = replay_provider.requests();
        assert_eq!(
            requests.len(),
            1,
            "une seule requête : le tour sauvegardé n'est pas rejoué"
        );
        assert!(
            requests[0].messages().iter().any(|message| matches!(
                message,
                Message::Assistant { content, .. } if content.contains("MARK-TOUR-SAUVAGE")
            )),
            "la requête reprend l'historique sauvegardé, tour assistant compris"
        );
    }

    // ── Scenario: le plafond max_secs arrête l'agent -------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn le_plafond_max_secs_arrete_l_agent() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "root", true).await;
        wire_catalog(&db).await;
        put_catalog_agent(&db, &sub_agent("G_sub", "tu rends un verdict", Vec::new())).await;
        let step = step_with(db.clone(), Arc::new(HangFactory));
        let run = run_info("t-plafond", "s1", Some("alice"));

        let started = std::time::Instant::now();
        let error = step
            .run(
                &run,
                json!({ "agent": "G_sub", "task": "inspecter", "max_secs": 1 }),
                HashMap::new(),
            )
            .await
            .expect_err("le plafond arrête le run");

        assert!(!error.retryable, "le plafond n'est pas retryable : {error}");
        assert_eq!(error.message, "agent run exceeded max_secs");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "l'annulation coopérative s'applique au plafond, pas au-delà"
        );
        let saved = session_of(&db, "alice", &run)
            .await
            .expect("la session annulée reste sauvegardée");
        assert!(
            !saved.history().messages().is_empty(),
            "la session sauvegardée à l'annulation porte sa tâche"
        );
    }

    // ── Scenario: aucun secret ni prompt dans les erreurs ---------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn aucun_secret_ni_prompt_dans_les_erreurs() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "root", true).await;
        wire_catalog(&db).await;
        put_catalog_agent(
            &db,
            &sub_agent("G_sub", "PROMPT-X tu rends un verdict", Vec::new()),
        )
        .await;
        let provider = Arc::new(ScriptedProvider::new(vec![Err(InferenceError::new(
            ErrorClass::Permanent,
            "le fournisseur est en panne".to_owned(),
        ))]));
        let step = step_with(db, Arc::new(ScriptFactory(Arc::clone(&provider))));
        let run = run_info("t-fuite", "s1", Some("alice"));

        let error = step
            .run(&run, config("G_sub", "inspecter"), HashMap::new())
            .await
            .expect_err("une inférence qui échoue échoue le run");

        assert!(!error.retryable, "{error}");
        assert!(
            !error.message.contains("s3cret") && !error.message.contains("PROMPT-X"),
            "le message ne cite ni le secret ni le prompt : {error}"
        );
        let debug = format!("{error:?}");
        assert!(
            !debug.contains("s3cret") && !debug.contains("PROMPT-X"),
            "le Debug ne cite ni le secret ni le prompt : {debug}"
        );
    }

    // ── Scenario: les droits sont ceux du déclencheur -------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn les_droits_sont_ceux_du_declencheur() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "root", true).await;
        wire_catalog(&db).await;
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(subsecret_res::Entity))
            .await
            .expect("la table de la ressource fixture est créée");
        subsecret_res::Entity::insert(subsecret_res::ActiveModel {
            id: Set(1),
            title: Set("delta".to_owned()),
        })
        .exec(&db)
        .await
        .expect("une ligne AdminOnly existe");
        put_catalog_agent(
            &db,
            &sub_agent(
                "G_juge",
                "tu lis et tu rends ton verdict",
                vec!["resources".to_owned()],
            ),
        )
        .await;
        let script = r#"
            let verdict = "forbidden";
            try { let value = resources::get("subsecret", 1); verdict = "lu"; }
            catch (e) { verdict = "forbidden"; }
            result::submit(#{ verdict: verdict })
        "#;

        // Sous le déclencheur non-admin : la lecture est refusée dans le script de l'agent.
        let alice_provider = Arc::new(ScriptedProvider::new(vec![Ok(pad_call(script))]));
        let alice_step = step_with_readers(db.clone(), Arc::new(ScriptFactory(Arc::clone(&alice_provider))));
        let alice_run = run_info("t-droits-alice", "s1", Some("alice"));
        let alice_output = alice_step
            .run(&alice_run, config("G_juge", "lis et juge"), HashMap::new())
            .await
            .expect("l'agent rend son verdict sous alice");

        // Sous le déclencheur admin : la même définition lit.
        let root_provider = Arc::new(ScriptedProvider::new(vec![Ok(pad_call(script))]));
        let root_step = step_with_readers(db.clone(), Arc::new(ScriptFactory(Arc::clone(&root_provider))));
        let root_run = run_info("t-droits-root", "s1", Some("root"));
        let root_output = root_step
            .run(&root_run, config("G_juge", "lis et juge"), HashMap::new())
            .await
            .expect("l'agent rend son verdict sous root");

        assert_eq!(alice_output, json!({ "verdict": "forbidden" }));
        assert_eq!(root_output, json!({ "verdict": "lu" }));
    }

    // ── Scenario lot 2: un rejeu n'écrit pas deux fois -----------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn un_rejeu_n_ecrit_pas_deux_fois() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "root", true).await;
        wire_catalog(&db).await;
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(widget_res::Entity))
            .await
            .expect("la table de la ressource fixture est créée");
        put_catalog_agent(
            &db,
            &sub_agent(
                "G_scribe",
                "tu ecris puis tu rends ton verdict",
                vec!["resources".to_owned()],
            ),
        )
        .await;
        let registry = || {
            let mut registry = ResourceRegistry::new();
            registry.register::<widget_res::Entity>();
            registry
        };
        let run = run_info("t-rejeu-ecritures", "s1", Some("alice"));
        let create = r#"resources::create("widget", #{ id: 0, title: "Ecriture-unique" })"#;

        // Le run interrompu : un `create` exécuté et journalisé, aucun résultat soumis (`max_turns` 1).
        let first_provider = Arc::new(ScriptedProvider::new(vec![Ok(pad_call(create))]));
        let first = step_with_registry(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&first_provider))),
            registry(),
        );
        let interrupted = first
            .run(
                &run,
                json!({ "agent": "G_scribe", "task": "ecris puis juge", "max_turns": 1 }),
                HashMap::new(),
            )
            .await
            .expect_err("sans résultat soumis le premier run échoue");
        assert!(!interrupted.retryable, "{interrupted}");
        assert_eq!(widget_rows(&db).await, 1, "le `create` du premier run a écrit");
        assert_eq!(
            journal_rows(&db, "t-rejeu-ecritures").await,
            1,
            "l'écriture est journalisée sous les clés de la session `Sub`"
        );

        // Le rejeu : session restaurée, le MÊME `create` est ré-émis — c'est le journal qui répond,
        // la ligne n'est pas doublée (`WriteScope` du step, DSD `DEC-21`).
        let replay_script = format!("{create};\nresult::submit(#{{ verdict: \"ok\" }})");
        let replay_provider = Arc::new(ScriptedProvider::new(vec![Ok(pad_call_id(
            "call-rejeu",
            &replay_script,
        ))]));
        let replay = step_with_registry(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&replay_provider))),
            registry(),
        );
        let output = replay
            .run(&run, config("G_scribe", "ecris puis juge"), HashMap::new())
            .await
            .expect("le rejeu rend le résultat");

        assert_eq!(output, json!({ "verdict": "ok" }));
        assert_eq!(
            widget_rows(&db).await,
            1,
            "une seule ligne existe : le rejeu n'a pas écrit deux fois"
        );
        assert_eq!(
            journal_rows(&db, "t-rejeu-ecritures").await,
            1,
            "une seule clé d'opération au journal après le rejeu"
        );
    }

    // ── Scenario lot 2: un serveur MCP en forward fait échouer le step sans retry --------------

    #[tokio::test(flavor = "multi_thread")]
    async fn un_serveur_mcp_en_forward_fait_echouer_le_step_sans_retry() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "root", true).await;
        wire_catalog(&db).await;
        put_catalog_agent(&db, &sub_agent("G_sub", "tu rends un verdict", Vec::new())).await;
        inserer_serveur(&db, "srv_fwd", "forward", json!(["all"])).await;
        lier_contexte(&db, "alice", &[("sandbox", "srv_fwd")]).await;
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let step = step_with(db.clone(), Arc::new(ScriptFactory(Arc::clone(&provider))));
        let run = run_info("t-forward", "s1", Some("alice"));

        let error = step
            .run(&run, config("G_sub", "inspecter"), HashMap::new())
            .await
            .expect_err("un serveur `forward` lié au contexte du step est refusé en batch");

        assert!(
            !error.retryable,
            "le refus du forward n'est pas retryable : {error}"
        );
        assert!(
            error
                .message
                .contains("forward auth needs an interactive session"),
            "la phrase fixe de `build_auth` : {error}"
        );
        assert!(provider.requests().is_empty(), "aucune inférence n'est partie");

        // Seconde clause de l'amendement : un serveur dont l'usage, accordé à la liaison, est
        // retiré à l'exécution du step (relecture en base de `mcp_access`, aucun cache) est un
        // échec non retryable du même ordre.
        inserer_serveur(&db, "srv_ops", "none", json!(["ops"])).await;
        let alice = crate::users::resolve_user(&db, "alice", None)
            .await
            .expect("le compte local d'alice");
        sync_group_memberships(&db, alice.id, &["ops".to_owned()])
            .await
            .expect("alice membre de ops");
        lier_contexte(&db, "alice", &[("prive", "srv_ops")]).await;
        sync_group_memberships(&db, alice.id, &[])
            .await
            .expect("appartenance retrahie avant l'exécution du step");
        let refus = step
            .run(&run, config("G_sub", "inspecter"), HashMap::new())
            .await
            .expect_err("un serveur refusé ferme le step");
        assert!(!refus.retryable, "le refus d'usage n'est pas retryable : {refus}");
        assert!(
            refus.message.contains("mcp server unavailable"),
            "le texte fermé de `server_for` : {refus}"
        );
        assert!(provider.requests().is_empty(), "toujours aucune inférence");
    }

    // ── Scenario: le plafond d'application borne max_secs (DEC-29) -----------------------------

    /// Hôte fixture du `Scenario` DEC-27 : accorde le rôle `sandbox` au seul subject `alice`,
    /// refuse (`Denied`) à tout autre — la décision se prend sur le `caller` relu par
    /// `resolve_role`, c'est-à-dire sous le déclencheur. Le jeton et l'URL sont des marqueurs de
    /// fuite : ils ne doivent jamais remonter dans une erreur.
    struct HoteParSujet;

    #[async_trait::async_trait]
    impl LymailleHost for HoteParSujet {
        async fn resolve_mcp(
            &self,
            request: &McpRequest<'_>,
            caller: &CallerPrincipal,
        ) -> Result<McpDecision, HostError> {
            if request.role == "sandbox" && caller.subject == "alice" {
                Ok(McpDecision::Granted(McpGrant {
                    url: "https://sbx.example/mcp".to_owned(),
                    token: Zeroizing::new("JETON-HOTE".to_owned()),
                    expires_at: None,
                }))
            } else {
                Ok(McpDecision::Denied)
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn le_plafond_dapplication_borne_max_secs() {
        let db = test_db().await;
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let factory: Arc<dyn InferenceFactory> = Arc::new(ScriptFactory(Arc::clone(&provider)));
        let default_step = step_with(db.clone(), Arc::clone(&factory));
        let high_step = step_with(db.clone(), Arc::clone(&factory))
            .with_max_secs_ceiling(7200)
            .expect("7200 est dans 1..=LONG_CEILING_MAX_SECS");

        // Sonde : `trigger = None`. Si la validation de `max_secs` passe, le run s'arrête à
        // l'étape 2 (« trigger identity ») ; si elle échoue, le message est préfixé
        // « config subagent invalide ». Dans les deux cas ni la base ni l'inférence ne sont
        // touchées.
        for (step, secs, accepte) in [
            (&default_step, 3600_u64, true),
            (&default_step, 3601, false),
            (&high_step, 3601, true),
            (&high_step, 7200, true),
        ] {
            let error = step
                .run(
                    &run_info("t-ceiling", "s1", None),
                    json!({ "agent": "G_sub", "task": "inspecter", "max_secs": secs }),
                    HashMap::new(),
                )
                .await
                .expect_err("la sonde rend l'échec de validation ou d'identité");
            if accepte {
                assert_eq!(
                    error.message, "subagent step requires a trigger identity",
                    "max_secs={secs} doit être accepté par le plafond en force"
                );
            } else {
                assert!(
                    error.message.starts_with("config subagent invalide"),
                    "max_secs={secs} doit être refusé par le plafond en force : {error}"
                );
            }
        }

        // Bornes du constructeur : 0 et 14 401 refusés par la phrase fixe, 14 400 accepté.
        let refuse_zero = step_with(db.clone(), Arc::clone(&factory)).with_max_secs_ceiling(0);
        assert!(
            matches!(&refuse_zero, Err(LymailleError::InvalidDefinition(detail))
                if detail == "max_secs ceiling out of range"),
            "le plafond 0 est refusé par le constructeur"
        );
        let refuse_max = step_with(db.clone(), Arc::clone(&factory)).with_max_secs_ceiling(14_401);
        assert!(
            matches!(&refuse_max, Err(LymailleError::InvalidDefinition(detail))
                if detail == "max_secs ceiling out of range"),
            "le plafond 14 401 est refusé par le constructeur"
        );
        assert!(
            matches!(&refuse_zero, Err(e) if e.to_string().contains("max_secs ceiling out of range")),
            "le Display porte la phrase fixe"
        );
        assert!(
            step_with(db, Arc::clone(&factory))
                .with_max_secs_ceiling(LONG_CEILING_MAX_SECS)
                .is_ok(),
            "LONG_CEILING_MAX_SECS est la borne haute incluse et doit être accepté"
        );
        assert!(
            provider.requests().is_empty(),
            "les sondes ne touchent ni la base ni l'inférence"
        );
    }

    // ── Scenario: un rôle MCP fourni par l'hôte est résolu sous le déclencheur (DEC-27) --------

    #[tokio::test(flavor = "multi_thread")]
    async fn un_role_mcp_fourni_par_lhote_est_resolu_sous_le_declencheur() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "bob", false).await;
        user(&db, "root", true).await;
        wire_catalog(&db).await;
        put_catalog_agent(&db, &sub_agent("G_sub", "tu rends un verdict", Vec::new())).await;
        // Le serveur est pourvu par l'hôte ; sa ligne table n'est qu'une trace de repli : en
        // mode `forward` il est inutilisable en batch, donc tout run accepté sous `alice` prouve
        // que la décision vient de l'hôte et non de la table.
        inserer_serveur(&db, "srv_hote", "forward", json!(["all"])).await;
        lier_contexte(&db, "alice", &[("sandbox", "srv_hote")]).await;
        lier_contexte(&db, "bob", &[("sandbox", "srv_hote")]).await;
        let hote: SharedHost = Arc::new(HoteParSujet);
        let spy = Arc::new(SpyMcp::default());
        let mcp: Arc<dyn McpConnectorFactory> = spy.clone();

        // Sous `alice` : l'hôte accorde le rôle, le client est monté (connecteur espion) et le
        // run va à son terme.
        let alice_provider = Arc::new(ScriptedProvider::new(vec![Ok(pad_call(SUBMIT_OK))]));
        let alice_step = step_with_host(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&alice_provider))),
            Arc::clone(&hote),
            Arc::clone(&mcp),
        );
        let alice_run = run_info("t-hote-alice", "s1", Some("alice"));
        let output = alice_step
            .run(&alice_run, config("G_sub", "inspecter"), HashMap::new())
            .await
            .expect("l'hôte accorde le rôle à alice : le run aboutit");
        assert_eq!(output, json!({ "verdict": "ok" }));
        assert!(
            !alice_provider.requests().is_empty(),
            "le rôle accordé, le run part à l'inférence"
        );
        assert_eq!(
            spy.montes(),
            vec!["https://sbx.example/mcp".to_owned()],
            "le client de l'hôte est monté sur l'URL du Grant"
        );

        // Sous `bob` : refus fermé de l'hôte, non retryable, sur la phrase fixe — pas celle du
        // repli `forward` (aucun repli sur la table), et rien qui vienne de l'hôte.
        let bob_provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let bob_step = step_with_host(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&bob_provider))),
            Arc::clone(&hote),
            Arc::clone(&mcp),
        );
        let bob_run = run_info("t-hote-bob", "s1", Some("bob"));
        let error = bob_step
            .run(&bob_run, config("G_sub", "inspecter"), HashMap::new())
            .await
            .expect_err("l'hôte refuse bob");
        assert!(!error.retryable, "le refus de l'hôte est définitif : {error}");
        assert!(
            error.message.contains("mcp server unavailable"),
            "la phrase fermée de refus d'hôte : {error}"
        );
        assert!(
            !error.message.contains("forward auth"),
            "aucun repli sur la table n'a été consulté : {error}"
        );
        let debug = format!("{error:?}");
        assert!(
            !debug.contains("JETON-HOTE") && !debug.contains("sbx.example"),
            "ni le jeton ni l'URL de l'hôte ne transparaissent : {debug}"
        );
        assert!(bob_provider.requests().is_empty(), "aucune inférence pour bob");

        // Sans hôte, la même liaison refuse le step de façon permanente : repli sur la ligne
        // `forward`, inutilisable en batch (`retryable: false`).
        let sans_hote_provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let sans_hote = step_with(db.clone(), Arc::new(ScriptFactory(sans_hote_provider)));
        let mute_run = run_info("t-hote-mute", "s1", Some("alice"));
        let refus = sans_hote
            .run(&mute_run, config("G_sub", "inspecter"), HashMap::new())
            .await
            .expect_err("sans hôte la liaison n'est pas montageable en batch");
        assert!(!refus.retryable, "le refus sans hôte est permanent : {refus}");
        assert!(
            refus
                .message
                .contains("forward auth needs an interactive session"),
            "le repli sur la ligne `forward` est refusé : {refus}"
        );
    }

    // ── Montage du client hôte et des serveurs de la table (`./mcp_mount.sdd`) ──────────────

    /// Espion des connecteurs MCP : chaque montage est noté (URL) ; à chaque relevé (listage au
    /// montage, appel d'outil), la présence d'un porteur est notée. `McpHeaders` ne montre que la
    /// présence du jeton : la fraîcheur du jeton est prouvée par le compteur de l'hôte.
    #[derive(Default)]
    struct SpyMcp {
        montes: Mutex<Vec<String>>,
        releves: Arc<Mutex<Vec<(String, bool)>>>,
    }

    impl SpyMcp {
        fn montes(&self) -> Vec<String> {
            self.montes.lock().expect("verrou de l'espion").clone()
        }

        fn releves(&self) -> Vec<(String, bool)> {
            self.releves.lock().expect("verrou de l'espion").clone()
        }
    }

    impl McpConnectorFactory for SpyMcp {
        fn make(
            &self,
            url: &str,
            _request_timeout: Duration,
            _connect_timeout: Duration,
            auth: Option<Arc<dyn DynMcpAuth>>,
        ) -> Arc<dyn DynMcpConnector> {
            self.montes
                .lock()
                .expect("verrou de l'espion")
                .push(url.to_owned());
            Arc::new(SpyConnector {
                url: url.to_owned(),
                auth,
                releves: Arc::clone(&self.releves),
            })
        }
    }

    /// Connecteur espion : sans réseau, relève la présence d'un porteur à chaque relevé.
    struct SpyConnector {
        url: String,
        auth: Option<Arc<dyn DynMcpAuth>>,
        releves: Arc<Mutex<Vec<(String, bool)>>>,
    }

    impl SpyConnector {
        async fn relever(&self) -> Result<(), McpError> {
            let porteur = match &self.auth {
                Some(auth) => format!("{:?}", auth.headers_dyn().await?).contains("bearer: true"),
                None => false,
            };
            self.releves
                .lock()
                .expect("verrou de l'espion")
                .push((self.url.clone(), porteur));
            Ok(())
        }
    }

    impl McpLister for SpyConnector {
        async fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
            self.relever().await?;
            Ok(vec![McpTool::new("ping", "le ping", json!({"type": "object"}))])
        }
    }

    impl McpConnector for SpyConnector {
        async fn call_tool(
            &self,
            _name: &str,
            _arguments: &serde_json::Value,
        ) -> Result<serde_json::Value, McpError> {
            self.relever().await?;
            Ok(json!({"pong": true}))
        }

        fn reconnect(&self) -> impl Future<Output = ()> + Send {
            std::future::ready(())
        }
    }

    /// Hôte du `Scenario` « client de l'hôte » : accorde `sandbox` au seul `alice`, avec un jeton
    /// différent à chaque décision. Le compteur de décisions est relu par le test.
    struct HoteRotatif {
        appels: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LymailleHost for HoteRotatif {
        async fn resolve_mcp(
            &self,
            request: &McpRequest<'_>,
            caller: &CallerPrincipal,
        ) -> Result<McpDecision, HostError> {
            let rang = self.appels.fetch_add(1, Ordering::SeqCst).saturating_add(1);
            if request.role == "sandbox" && caller.subject == "alice" {
                Ok(McpDecision::Granted(McpGrant {
                    url: "https://sbx.example/mcp".to_owned(),
                    token: Zeroizing::new(format!("JETON-FICTIF-{rang}")),
                    expires_at: None,
                }))
            } else {
                Ok(McpDecision::Denied)
            }
        }
    }

    /// Hôte qui refuse tout rôle, à tout sujet (`Scenario` « rôle refusé »).
    struct HoteRefusant;

    #[async_trait::async_trait]
    impl LymailleHost for HoteRefusant {
        async fn resolve_mcp(
            &self,
            _request: &McpRequest<'_>,
            _caller: &CallerPrincipal,
        ) -> Result<McpDecision, HostError> {
            Ok(McpDecision::Denied)
        }
    }

    /// Ligne `static` de la table, référence d'authentification `FIXED_KEY` (résolue en test), à
    /// l'URL `https://mcp.example/`.
    async fn inserer_serveur_statique(db: &DatabaseConnection, nom: &str) {
        inserer_serveur_statique_a(db, nom, "https://mcp.example/").await;
    }

    /// Ligne `static` de la table, à l'URL donnée (même référence `FIXED_KEY`).
    async fn inserer_serveur_statique_a(db: &DatabaseConnection, nom: &str, url: &str) {
        use sea_orm::ActiveValue::NotSet;
        let actif = mcp_server::ActiveModel {
            id: NotSet,
            name: Set(nom.to_owned()),
            url: Set(url.to_owned()),
            request_timeout_secs: Set(30),
            connect_timeout_secs: Set(5),
            auth_mode: Set("static".to_owned()),
            auth_ref: Set(Some("FIXED_KEY".to_owned())),
            allowed_groups: Set(json!(["all"])),
            created_at: Set(chrono::Utc::now()),
        };
        actif
            .insert(db)
            .await
            .expect("ligne mcp_servers statique de test");
    }

    /// Le step des `Scenario` de montage : la couture d'inférence et la couture MCP, sans hôte
    /// sauf fourni.
    fn step_with_seams(
        db: DatabaseConnection,
        factory: Arc<dyn InferenceFactory>,
        host: Option<SharedHost>,
        mcp: Arc<dyn McpConnectorFactory>,
    ) -> SubagentStep {
        SubagentStep::with_seams(
            SubagentDeps {
                db,
                secrets: Arc::new(FixedSecret("s3cret")),
                registry: Arc::new(ResourceRegistry::new()),
                host,
            },
            factory,
            mcp,
        )
    }

    /// Script qui appelle deux fois l'outil du rôle `sandbox`, puis soumet le résultat.
    const SCRIPT_DEUX_APPELS: &str =
        r#"sandbox::ping(#{}); sandbox::ping(#{}); result::submit(#{ verdict: "ok" })"#;

    /// Script qui appelle une fois l'outil du rôle `outil`, puis soumet le résultat.
    const SCRIPT_APPEL_OUTIL: &str = r#"outil::ping(#{}); result::submit(#{ verdict: "ok" })"#;

    // ── Scenario: le client de l'hôte est monté et son jeton redemandé à chaque appel ──────────

    #[tokio::test(flavor = "multi_thread")]
    async fn le_client_de_lhote_est_monte_et_son_jeton_redemande_a_chaque_appel() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "root", true).await;
        wire_catalog(&db).await;
        put_catalog_agent(&db, &sub_agent("G_sub", "tu rends un verdict", Vec::new())).await;
        inserer_serveur(&db, "srv_hote", "forward", json!(["all"])).await;
        lier_contexte(&db, "alice", &[("sandbox", "srv_hote")]).await;
        let hote = Arc::new(HoteRotatif {
            appels: AtomicUsize::new(0),
        });
        let shared: SharedHost = hote.clone();
        let spy = Arc::new(SpyMcp::default());
        let mcp: Arc<dyn McpConnectorFactory> = spy.clone();
        let provider = Arc::new(ScriptedProvider::new(vec![Ok(pad_call(SCRIPT_DEUX_APPELS))]));
        let step = step_with_seams(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            Some(shared),
            mcp,
        );
        let run = run_info("t-client-hote", "s1", Some("alice"));

        let output = step
            .run(&run, config("G_sub", "inspecter"), HashMap::new())
            .await
            .expect("le client de l'hôte est monté : le run aboutit");

        assert_eq!(output, json!({ "verdict": "ok" }));
        // La sortie validée ne porte ni l'URL du Grant ni le jeton fictif de l'hôte. Le chemin
        // testé réussit : aucun message d'erreur n'est produit à comparer.
        let sortie = output.to_string();
        assert!(
            !sortie.contains("sbx.example") && !sortie.contains("JETON-FICTIF"),
            "la sortie ne porte ni l'URL ni le jeton : {sortie}"
        );
        assert_eq!(spy.montes(), vec!["https://sbx.example/mcp".to_owned()]);
        // Listage au montage, puis deux appels d'outil : trois relevés porteurs.
        assert_eq!(
            spy.releves(),
            vec![("https://sbx.example/mcp".to_owned(), true); 3]
        );
        // Une décision au montage, puis une par relevé : aucun jeton gardé entre deux relevés.
        assert_eq!(hote.appels.load(Ordering::SeqCst), 4);
    }

    // ── Scenario: un serveur static lié au contexte est monté en batch ────────────────────────

    /// Serveur `static` lié au rôle, sans hôte : seule la présence de l'en-tête d'authentification
    /// est vérifiée (`McpHeaders` n'expose pas sa valeur) ; aucune assertion ne porte sur la
    /// valeur du secret.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_serveur_static_lie_au_contexte_est_monte_en_batch() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "root", true).await;
        wire_catalog(&db).await;
        put_catalog_agent(&db, &sub_agent("G_sub", "tu rends un verdict", Vec::new())).await;
        inserer_serveur_statique(&db, "srv_stat").await;
        lier_contexte(&db, "alice", &[("outil", "srv_stat")]).await;
        let spy = Arc::new(SpyMcp::default());
        let mcp: Arc<dyn McpConnectorFactory> = spy.clone();
        let provider = Arc::new(ScriptedProvider::new(vec![Ok(pad_call(SCRIPT_APPEL_OUTIL))]));
        let step = step_with_seams(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            None,
            mcp,
        );
        let run = run_info("t-static", "s1", Some("alice"));

        let output = step
            .run(&run, config("G_sub", "inspecter"), HashMap::new())
            .await
            .expect("le serveur static lié est monté en batch");

        assert_eq!(output, json!({ "verdict": "ok" }));
        assert_eq!(spy.montes(), vec!["https://mcp.example/".to_owned()]);
        // Listage au montage puis appel de l'outil : deux relevés, chacun avec présence d'en-tête
        // seulement (valeur non observable).
        assert_eq!(spy.releves(), vec![("https://mcp.example/".to_owned(), true); 2]);
    }

    // ── Scenario: le contexte des rôles MCP est lu sous le type workflow_subagent ─────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn le_contexte_des_roles_mcp_est_lu_sous_le_type_workflow_subagent() {
        // (a) le nom du type est un contrat : c'est sous ce nom que les serveurs sont liés.
        assert_eq!(SUBAGENT_CONTEXT_TYPE, "workflow_subagent");

        // (b) deux contextes privés d'alice : seul celui de type `workflow_subagent` est monté.
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "carol", false).await;
        user(&db, "root", true).await;
        wire_catalog(&db).await;
        put_catalog_agent(&db, &sub_agent("G_sub", "tu rends un verdict", Vec::new())).await;
        inserer_serveur_statique(&db, "srv_stat").await;
        inserer_serveur_statique_a(&db, "srv_autre", "https://autre.example/").await;
        lier_contexte(&db, "alice", &[("sandbox", "srv_stat")]).await;
        lier_contexte_de_type(&db, "alice", "autre_type", &[("sandbox", "srv_autre")]).await;
        let spy = Arc::new(SpyMcp::default());
        let mcp: Arc<dyn McpConnectorFactory> = spy.clone();
        let provider = Arc::new(ScriptedProvider::new(vec![Ok(pad_call(
            r#"sandbox::ping(#{}); result::submit(#{ verdict: "ok" })"#,
        ))]));
        let step = step_with_seams(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            None,
            mcp,
        );
        let output = step
            .run(
                &run_info("t-type-contexte", "s1", Some("alice")),
                config("G_sub", "inspecter"),
                HashMap::new(),
            )
            .await
            .expect("le step aboutit avec le seul serveur du contexte workflow_subagent");
        assert_eq!(output, json!({ "verdict": "ok" }));
        assert_eq!(spy.montes(), vec!["https://mcp.example/".to_owned()]);

        // (c) sans contexte de type `workflow_subagent`, le step s'exécute sans rôle MCP lié :
        // carol n'a qu'un contexte `autre_type`, rien n'est monté.
        lier_contexte_de_type(&db, "carol", "autre_type", &[("sandbox", "srv_autre")]).await;
        let spy_sans = Arc::new(SpyMcp::default());
        let mcp_sans: Arc<dyn McpConnectorFactory> = spy_sans.clone();
        let provider_sans = Arc::new(ScriptedProvider::new(vec![Ok(pad_call(
            r#"result::submit(#{ verdict: "ok" })"#,
        ))]));
        let step_sans = step_with_seams(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider_sans))),
            None,
            mcp_sans,
        );
        let output_sans = step_sans
            .run(
                &run_info("t-sans-contexte", "s1", Some("carol")),
                config("G_sub", "inspecter"),
                HashMap::new(),
            )
            .await
            .expect("sans contexte workflow_subagent, le step aboutit sans rôle MCP");
        assert_eq!(output_sans, json!({ "verdict": "ok" }));
        assert!(
            spy_sans.montes().is_empty(),
            "aucun serveur monté sans contexte de type workflow_subagent"
        );
    }

    // ── Scenario: un rôle MCP refusé échoue le step avant tout tour ───────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn un_role_mcp_refuse_echoue_le_step_avant_tout_tour() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "bob", false).await;
        user(&db, "root", true).await;
        wire_catalog(&db).await;
        put_catalog_agent(&db, &sub_agent("G_sub", "tu rends un verdict", Vec::new())).await;
        inserer_serveur(&db, "srv_hote", "forward", json!(["all"])).await;
        inserer_serveur(&db, "srv_nu", "none", json!(["all"])).await;
        lier_contexte(&db, "alice", &[("sandbox", "srv_hote")]).await;
        // Nom de rôle invalide : un tiret n'est pas un identifiant Rhai, `mcp_module` le refuse
        // avant tout listage, en `Store` (retryable par défaut) que le montage rend non retryable.
        lier_contexte(&db, "bob", &[("bad-role", "srv_nu")]).await;

        // Refus de l'hôte au déclencheur (`Forbidden`) : aucun client, aucune inférence.
        let hote: SharedHost = Arc::new(HoteRefusant);
        let spy = Arc::new(SpyMcp::default());
        let mcp: Arc<dyn McpConnectorFactory> = spy.clone();
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let step = step_with_seams(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            Some(hote),
            mcp,
        );
        let error = step
            .run(
                &run_info("t-refus-hote", "s1", Some("alice")),
                config("G_sub", "inspecter"),
                HashMap::new(),
            )
            .await
            .expect_err("un rôle MCP refusé par l'hôte échoue le step");
        assert!(!error.retryable, "refus de l'hôte, non retryable : {error}");
        assert!(
            error.message.contains("mcp server unavailable"),
            "phrase fermée du refus : {error}"
        );
        assert!(
            spy.montes().is_empty(),
            "aucun client monté pour un refus de l'hôte"
        );
        assert!(
            provider.requests().is_empty(),
            "aucune inférence avant le premier tour (refus de l'hôte)"
        );

        // Nom de rôle invalide, sans hôte : l'échec de `mcp_module` (`Store`) est rendu non
        // retryable par le montage ; le run n'a pas commencé.
        let provider_nom = Arc::new(ScriptedProvider::new(Vec::new()));
        let step_nom = step_with_seams(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider_nom))),
            None,
            Arc::new(SpyMcp::default()),
        );
        let error_nom = step_nom
            .run(
                &run_info("t-refus-nom", "s1", Some("bob")),
                config("G_sub", "inspecter"),
                HashMap::new(),
            )
            .await
            .expect_err("un nom de rôle invalide échoue le montage");
        assert!(
            error_nom.message.contains("bad-role"),
            "l'échec vient de mcp_module, qui nomme le rôle refusé : {error_nom}"
        );
        assert!(
            !error_nom.retryable,
            "l'échec de mcp_module (Store) est forcé non retryable : {error_nom}"
        );
        assert!(
            provider_nom.requests().is_empty(),
            "aucune inférence : le run n'a pas commencé (nom de rôle invalide)"
        );
    }

    // ── Scenario: une sortie d'outil MCP qui écho la clé du provider est masquée ──────────────

    /// Clé fictive du provider de ce `Scenario` (résolue par la référence `FIXED_KEY`).
    const CLE_FICTIVE: &str = "cle-fictive-de-test";

    /// Message d'échec de l'outil : il reflète la clé du provider, comme un serveur qui la rend
    /// dans ses erreurs. `refus de l'outil` est le témoin que l'échec a bien été reçu.
    fn message_d_echec() -> String {
        format!("refus de l'outil, jeton {CLE_FICTIVE} rejeté")
    }

    /// Script qui appelle l'outil du rôle `outil`, capture l'erreur levée et la soumet : le texte
    /// reçu par l'agent est ce qu'il soumet (un tour bat une seule inférence : appel et soumission
    /// sont dans le même script).
    const SCRIPT_ECHEC: &str = r#"let recu = ""; try { outil::ping(#{}); } catch (erreur) { recu = erreur.to_string(); } result::submit(#{ verdict: recu })"#;

    /// Fabrique MCP dont l'outil échoue avec un message donné.
    struct FailMcp {
        message: String,
    }

    impl McpConnectorFactory for FailMcp {
        fn make(
            &self,
            _url: &str,
            _request_timeout: Duration,
            _connect_timeout: Duration,
            _auth: Option<Arc<dyn DynMcpAuth>>,
        ) -> Arc<dyn DynMcpConnector> {
            Arc::new(FailConnector {
                message: self.message.clone(),
            })
        }
    }

    /// Connecteur sans réseau : l'outil `ping` échoue avec le message de la fabrique.
    struct FailConnector {
        message: String,
    }

    impl McpLister for FailConnector {
        fn list_tools(&self) -> impl Future<Output = Result<Vec<McpTool>, McpError>> + Send {
            std::future::ready(Ok(vec![McpTool::new(
                "ping",
                "le ping",
                json!({"type": "object"}),
            )]))
        }
    }

    impl McpConnector for FailConnector {
        fn call_tool(
            &self,
            _name: &str,
            _arguments: &serde_json::Value,
        ) -> impl Future<Output = Result<serde_json::Value, McpError>> + Send {
            std::future::ready(Err(McpError::Failed(self.message.clone())))
        }

        fn reconnect(&self) -> impl Future<Output = ()> + Send {
            std::future::ready(())
        }
    }

    /// Le step avec une clé d'API du provider donnée (résolue par `FIXED_KEY`).
    fn step_avec_cle(
        db: DatabaseConnection,
        factory: Arc<dyn InferenceFactory>,
        mcp: Arc<dyn McpConnectorFactory>,
        cle: &'static str,
    ) -> SubagentStep {
        SubagentStep::with_seams(
            SubagentDeps {
                db,
                secrets: Arc::new(FixedSecret(cle)),
                registry: Arc::new(ResourceRegistry::new()),
                host: None,
            },
            factory,
            mcp,
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn un_echec_doutil_mcp_qui_echo_la_cle_du_provider_est_masque() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "root", true).await;
        wire_catalog(&db).await;
        put_catalog_agent(&db, &sub_agent("G_sub", "tu rends un verdict", Vec::new())).await;
        inserer_serveur_statique(&db, "srv_stat").await;
        lier_contexte(&db, "alice", &[("outil", "srv_stat")]).await;
        let mcp: Arc<dyn McpConnectorFactory> = Arc::new(FailMcp {
            message: message_d_echec(),
        });
        let provider = Arc::new(ScriptedProvider::new(vec![Ok(pad_call(SCRIPT_ECHEC))]));
        let step = step_avec_cle(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            mcp,
            CLE_FICTIVE,
        );

        let output = step
            .run(
                &run_info("t-echec", "s1", Some("alice")),
                config("G_sub", "inspecter"),
                HashMap::new(),
            )
            .await
            .expect("le run aboutit : l'échec de l'outil est capturé, puis soumis");

        // Témoin : l'agent a bien reçu le texte d'échec. La clé n'y figure pas, ni dans la sortie.
        let sortie = output.to_string();
        assert!(
            sortie.contains("refus de l'outil"),
            "l'agent a reçu le texte d'échec : {sortie}"
        );
        assert!(
            !sortie.contains(CLE_FICTIVE),
            "le texte d'échec reçu par l'agent ne porte pas la clé : {sortie}"
        );
    }

    // ── Scenario: un rôle MCP au nom d'un module du run échoue le step ───────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn un_role_mcp_au_nom_dun_module_du_run_echoue_le_step() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "bob", false).await;
        user(&db, "root", true).await;
        wire_catalog(&db).await;
        // `resources` est accordé par la définition ; `G_sub` n'accorde aucun module.
        put_catalog_agent(
            &db,
            &sub_agent("G_res", "tu rends un verdict", vec!["resources".to_owned()]),
        )
        .await;
        put_catalog_agent(&db, &sub_agent("G_sub", "tu rends un verdict", Vec::new())).await;
        inserer_serveur_statique(&db, "srv_stat").await;
        lier_contexte(&db, "alice", &[("resources", "srv_stat")]).await;
        lier_contexte(&db, "bob", &[("result", "srv_stat")]).await;
        let spy = Arc::new(SpyMcp::default());
        let mcp: Arc<dyn McpConnectorFactory> = spy.clone();
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let step = step_with_seams(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            None,
            mcp,
        );

        let collision_resources = step
            .run(
                &run_info("t-collision-resources", "s1", Some("alice")),
                config("G_res", "inspecter"),
                HashMap::new(),
            )
            .await
            .expect_err("un rôle `resources` face au module `resources` du run échoue le step");
        assert!(
            !collision_resources.retryable,
            "non retryable : {collision_resources}"
        );
        assert!(
            collision_resources
                .message
                .contains("mcp role name collides with a module: resources"),
            "message fixe de la collision : {collision_resources}"
        );

        let collision_result = step
            .run(
                &run_info("t-collision-result", "s1", Some("bob")),
                config("G_sub", "inspecter"),
                HashMap::new(),
            )
            .await
            .expect_err("un rôle `result` face au module `result` du batch échoue le step");
        assert!(!collision_result.retryable, "non retryable : {collision_result}");
        assert!(
            collision_result
                .message
                .contains("mcp role name collides with a module: result"),
            "message fixe de la collision : {collision_result}"
        );

        assert!(spy.montes().is_empty(), "aucun connecteur créé");
        assert!(
            provider.requests().is_empty(),
            "aucune requête d'inférence, aucun tour"
        );
    }
}
