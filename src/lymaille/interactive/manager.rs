//! Gestion du cycle de vie des sessions interactives — socle commun REST+SSE et Matrix
//! (`src/lymaille/interactive/manager.sdd`).
//!
//! Le `SessionManager` est le **socle commun** que les canaux appellent sans logique propre
//! (DSD `FR-11`, `DEC-6`) : ouvrir (nature portée par le type du contexte, `DEC-28`), envoyer,
//! suivre les événements par le hub, annuler, reprendre, fermer. Une session ne persiste que le
//! `subject` de son utilisateur ; le principal est reconstruit en base à **chaque appel**
//! (`../identity.sdd`, DSD `DEC-3`). Au plus un tour actif par session et par processus
//! (`TurnGate` de `lymaille`, `try_begin` dans `send`) ; aucun conflit inter-processus n'est
//! détecté (`session#DEC-1` de `lymaille`, dernier `save` gagne) — la crate cible un déploiement
//! mono-réplique, plusieurs répliques exigeraient de l'affinité de session. Un tour est une tâche
//! `tokio` **détachée** de la requête qui l'a lancé : fermer le client SSE n'annule pas le tour.
//!
//! L'assemblage (`./assemble.rs`) est refait à **chaque tour** dans la tâche du tour : le jeton
//! `forward` d'un tour n'y vit que pendant ce tour, jamais dans la mémoire du gestionnaire. La
//! colonne `context` de la ligne de session est posée à l'ouverture et **immuable** depuis : le
//! `save` du state par les tours met `state`/`kind`/`updated_at` à jour et ne l'écrit jamais
//! (`DEC-28`).

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use sea_orm::ConnectionTrait;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait;
use sea_orm::sea_query::Query;
use zeroize::Zeroizing;

use ::lymaille::AgentDefinition;
use ::lymaille::AgentKind;
use ::lymaille::AgentRef;
use ::lymaille::AgentStore as _;
use ::lymaille::CancelHandle;
use ::lymaille::ContextType;
use ::lymaille::Redactor;
use ::lymaille::SessionError;
use ::lymaille::SessionFilter;
use ::lymaille::SessionId;
use ::lymaille::SessionIndex as _;
use ::lymaille::SessionKind;
use ::lymaille::SessionState;
use ::lymaille::SessionStore as _;
use ::lymaille::SessionSummary;
use ::lymaille::ToolsetStore as _;
use ::lymaille::TurnGate;

use crate::lymaille::error::LymailleError;
use crate::lymaille::identity::CallerPrincipal;
use crate::lymaille::identity::IdKind;
use crate::lymaille::identity::Identity;
use crate::lymaille::identity::classify;
use crate::lymaille::resource_module::ResourceRegistry;
use crate::lymaille::secret::SecretResolver;
use crate::lymaille::store::LymailleStores;

use super::assemble::AssembleDeps;
use super::assemble::assemble_session;
use super::hub::EventHub;
use super::hub::Subscription;

/// Table des sessions, nom complet (`../migration.rs`) — le gestionnaire y pose et relit la
/// colonne `context`, que le store de sessions n'écrit jamais.
const SESSIONS_TABLE: &str = "miryad_lymaille_sessions";
/// Table des contextes privés (`../migration.rs`) — lecture de `context_type` au propriétaire.
const USER_CONTEXTS_TABLE: &str = "miryad_lymaille_user_contexts";
/// Table des contextes du catalogue (`../migration.rs`) — lecture de `context_type`.
const CATALOG_CONTEXTS_TABLE: &str = "miryad_lymaille_catalog_contexts";
/// Table des types de contexte (`../migration.rs`) — nature, agent par défaut, agents autorisés.
const CATALOG_CONTEXT_TYPES_TABLE: &str = "miryad_lymaille_catalog_context_types";

/// Texte de `NotFound` des sessions : absent et invisible rendent le **même** texte, l'existence
/// d'une session d'un autre n'est jamais révélée (`Must not` de `./manager.sdd`).
const SESSION_NOT_FOUND: &str = "session not found";
/// Texte de `NotFound` des contextes à l'ouverture — même indiscernabilité (absent ou invisible).
const CONTEXT_NOT_FOUND: &str = "context not found";
/// Texte de `Forbidden` d'un agent hors de l'ensemble autorisé du type (`MRD-LYMAILLE-002`,
/// AC-19 de la DSD) — l'agent retenu n'appartient pas à `allowed_agents` ou n'a pas la nature
/// du type.
const AGENT_NOT_ALLOWED: &str = "agent not allowed for this context type";

/// Limites appliquées par le gestionnaire (`Must` de `./manager.sdd` : défauts 8 et 30 min).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Plafond de sessions par utilisateur non admin (l'admin est exempté).
    pub max_open_per_user: usize,
    /// Inactivité au-delà de laquelle la session est déchargée de la mémoire (jamais de l'état
    /// persisté). Verrou et déchargement **par processus** (limite mono-réplique de la spec).
    pub idle_ttl: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_open_per_user: 8,
            idle_ttl: Duration::from_mins(30),
        }
    }
}

/// Ce que l'application fournit au gestionnaire : l'attirail d'assemblage (connexion, hub,
/// lecteurs de ressources, secrets, horloge, hôte optionnel, coutures de test), le verrou de
/// tour et les limites.
pub struct ManagerDeps {
    /// Dependances d'assemblage (`./assemble.rs`) — le moteur d'une session est assemblé à chaque
    /// tour avec elles.
    pub assemble: AssembleDeps,
    /// Verrou des tours actifs, partagé par toutes les sessions (`lymaille`).
    pub gate: TurnGate,
    /// Plafond et déchargement mémoire.
    pub limits: Limits,
}

impl ManagerDeps {
    /// Deps de production : assemblage `AssembleDeps::new`, verrou neuf, limites par défaut.
    #[must_use]
    pub fn new(
        db: sea_orm::DatabaseConnection,
        readers: Arc<ResourceRegistry>,
        secrets: Arc<dyn SecretResolver>,
        hub: Arc<EventHub>,
    ) -> Self {
        Self {
            assemble: AssembleDeps::new(db, readers, secrets, hub),
            gate: TurnGate::new(),
            limits: Limits::default(),
        }
    }
}

/// La session rendue par `open` et `resume` : identifiant opaque, nature résolue, agent retenu,
/// contexte persisté, et les avis de provenance de la reprise (`OI-8`).
#[derive(Debug, Clone)]
pub struct SessionHandle {
    /// Identifiant opaque de la session (UUID v4 à l'ouverture).
    pub id: SessionId,
    /// Nature résolue à l'ouverture — portée par le type du contexte (`DEC-28`), jamais demandée.
    pub kind: SessionKind,
    /// Agent retenu (`agent` explicite ou `default_agent` du type).
    pub agent_id: String,
    /// Contexte sur lequel la session est ouverte — colonne immuable de la ligne.
    pub context: String,
    /// Avis de provenance de la reprise (empreinte ou version changée) ; vide à l'ouverture.
    pub notices: Vec<String>,
}

/// Mémoire d'une session ouverte : l'anse d'annulation du tour courant (publiée dès que le moteur
/// est assemblé), le drapeau d'annulation demandée avant cette publication, et l'instant du
/// dernier usage pour le déchargement `idle_ttl`.
struct Entry {
    cancel: Mutex<Option<CancelHandle>>,
    cancel_requested: AtomicBool,
    last_used: AtomicU64,
}

impl Entry {
    fn new(now_ms: u64) -> Self {
        Self {
            cancel: Mutex::new(None),
            cancel_requested: AtomicBool::new(false),
            last_used: AtomicU64::new(now_ms),
        }
    }
}

struct Inner {
    deps: ManagerDeps,
    identity: Identity,
    turns: Mutex<Table>,
}

/// Mémoire des sessions ouvertes de ce processus : entrées par session (annulation, dernier
/// usage), plafond `max_open_per_user` — les sessions fermées ou déchargées ne comptent plus —
/// et marques de fermeture : une session `close`ée rend le même `NotFound` qu'une session absente
/// jusqu'à une `resume` (`Scenario` « fermer libère la session » de ./manager.sdd).
/// Mono-réplique : la table ne dit rien des autres processus (`session#DEC-1` de `lymaille`).
#[derive(Default)]
struct Table {
    by_id: HashMap<SessionId, (String, Arc<Entry>)>,
    closed: HashSet<SessionId>,
}

impl Table {
    /// Enregistre l'usage de la session `id` par `subject` : rend son entrée (créée si needed)
    /// et date son dernier usage.
    fn touch(&mut self, subject: &str, id: &SessionId, now_ms: u64) -> Arc<Entry> {
        let entry = self
            .by_id
            .entry(id.clone())
            .or_insert_with(|| (subject.to_owned(), Arc::new(Entry::new(now_ms))))
            .1
            .clone();
        entry.last_used.store(now_ms, Ordering::SeqCst);
        entry
    }

    /// Nombre de sessions ouvertes en mémoire par `subject` (source du plafond).
    fn count(&self, subject: &str) -> usize {
        self.by_id.values().filter(|entry| entry.0 == subject).count()
    }

    /// Décharge une session de la mémoire (fermeture) et la marque fermée : `send` rend dès lors
    /// le même `NotFound` qu'une session absente, jusqu'à une reprise. L'état persisté demeure.
    fn close(&mut self, id: &SessionId) {
        let _removed: Option<(String, Arc<Entry>)> = self.by_id.remove(id);
        let _closed = self.closed.insert(id.clone());
    }

    /// Une session fermée (non encore reprise) dans ce processus.
    fn is_closed(&self, id: &SessionId) -> bool {
        self.closed.contains(id)
    }

    /// Reprise : la marque de fermeture est levée, la session accepte de nouveau des tours
    /// (l'état persisté relisu la rend disponible « à la demande », ./manager.sdd).
    fn reopen(&mut self, id: &SessionId) {
        let _gone = self.closed.remove(id);
    }

    /// Déchargement `idle_ttl` : les sessions inactives depuis le plafond quittent la mémoire,
    /// jamais un tour en cours (`Must` de `./manager.sdd` ; verrou et déchargement par
    /// processus — limite mono-réplique).
    fn sweep(&mut self, gate: &TurnGate, now_ms: u64, idle_ttl_ms: u64) {
        self.by_id.retain(|id, (_, entry)| {
            let idle_ms = now_ms.saturating_sub(entry.last_used.load(Ordering::SeqCst));
            idle_ms < idle_ttl_ms || gate.is_active(id)
        });
    }
}

/// Gestionnaire des sessions interactives (`./manager.sdd`) — `Clone` : tous les clones partagent
/// la mémoire des sessions ouvertes et le verrou des tours.
#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<Inner>,
}

impl SessionManager {
    /// Attache un gestionnaire aux dépendances fournies.
    #[must_use]
    pub fn new(deps: ManagerDeps) -> Self {
        let identity = Identity::new(deps.assemble.db.clone());
        Self {
            inner: Arc::new(Inner {
                deps,
                identity,
                turns: Mutex::new(Table::default()),
            }),
        }
    }

    /// Ouvre une session sur le contexte `context` (arbitrage `DEC-28` : l'utilisateur choisit un
    /// contexte, pas un agent parmi N) — voir le `Must` de `./manager.sdd`.
    ///
    /// # Errors
    ///
    /// [`LymailleError::Forbidden`] (`agent not allowed for this context type`,
    /// `too many open sessions`, principal inconnu), [`LymailleError::NotFound`] (contexte
    /// invisible, agent insoluble — textes indiscernables), [`LymailleError::InvalidDefinition`]
    /// (contexte hôte sans agent explicite, `Config` sur `H_…`, type inutilisable).
    pub async fn open(
        &self,
        subject: &str,
        context: &str,
        agent: Option<&str>,
    ) -> Result<SessionHandle, LymailleError> {
        let caller = self.principal(subject).await?;
        let stores = self.stores(subject);
        // Résolution (`DEC-28`) : la nature vient du TYPE du contexte, l'agent retenu est
        // l'agent explicite sinon le défaut du type ; un contexte hôte `H_…` exige un agent
        // de travail explicite, aucun type n'étant en table.
        let (kind, agent_id, definition) = match classify(context)? {
            IdKind::Host => {
                let Some(agent) = agent else {
                    return Err(LymailleError::InvalidDefinition(
                        "host sessions need an explicit agent".to_string(),
                    ));
                };
                self.host_context_exists(&caller, context).await?;
                let definition = self.resolve_agent(&stores, agent).await?;
                if definition.kind() != AgentKind::Work {
                    return Err(LymailleError::InvalidDefinition(
                        "host contexts do not open config sessions".to_string(),
                    ));
                }
                (SessionKind::Work, agent.to_owned(), definition)
            }
            kind @ (IdKind::Catalog | IdKind::Private) => {
                self.resolve_type_start(&stores, &caller, kind, context, agent)
                    .await?
            }
        };
        // Plafond de sessions ouvertes en mémoire (l'admin est exemptée).
        if !caller.is_admin
            && lock(&self.inner.turns).count(subject) >= self.inner.deps.limits.max_open_per_user
        {
            return Err(LymailleError::Forbidden("too many open sessions".to_string()));
        }
        let versions = self.toolset_versions(&stores, &definition).await?;
        let id = SessionId::new(uuid::Uuid::new_v4().to_string())
            .ok_or_else(|| LymailleError::Store("generated session identifier was rejected".to_string()))?;
        let state = SessionState::new(
            id.clone(),
            AgentRef::new(definition.id(), definition.fingerprint()),
            context,
            versions,
            self.inner.deps.assemble.clock.now(),
        )
        .with_kind(kind);
        stores.sessions().save(&state).await.map_err(from_session)?;
        // La colonne `context` est posée ici et nulle part ailleurs : le `save` du state par
        // les tours ne l'écrit jamais (`DEC-28`).
        self.stamp_context(subject, &id, context).await?;
        self.inner.deps.assemble.hub.register(&id, subject);
        self.touch(subject, &id);
        Ok(SessionHandle {
            id,
            kind,
            agent_id,
            context: context.to_owned(),
            notices: Vec::new(),
        })
    }

    /// Lance le tour `text` de la session `id` en tâche détachée ; le jeton `forward` ne vit que
    /// pendant ce tour.
    ///
    /// # Errors
    ///
    /// [`LymailleError::NotFound`] (session absente ou d'un autre), [`LymailleError::Conflict`]
    /// (`turn active`).
    pub async fn send(
        &self,
        subject: &str,
        id: &SessionId,
        text: String,
        token: Option<Zeroizing<String>>,
    ) -> Result<(), LymailleError> {
        let caller = self.principal(subject).await?;
        let state = self.readable(subject, id).await?;
        // Session fermée par `close` et non encore reprise : le même `NotFound` indiscernable
        // qu'une session absente (`Scenario` « fermer libère la session » de ./manager.sdd).
        if lock(&self.inner.turns).is_closed(id) {
            return Err(LymailleError::NotFound(SESSION_NOT_FOUND.to_string()));
        }
        let permit = match self.inner.deps.gate.try_begin(id.clone()) {
            Ok(permit) => permit,
            Err(SessionError::TurnActive(_)) => {
                return Err(LymailleError::Conflict("turn active".to_string()));
            }
            Err(other) => return Err(from_session(other)),
        };
        let entry = self.touch(subject, id);
        // L'entrée repart vierge d'annulation : l'anse du tour précédent n'a plus de prise,
        // le drapeau attend une annulation arrivée avant la publication de l'anse du tour.
        *lock(&entry.cancel) = None;
        entry.cancel_requested.store(false, Ordering::SeqCst);
        let kind = state.kind();
        let agent_id = state.agent().name().to_owned();
        let inner = Arc::clone(&self.inner);
        let session = id.clone();
        // Le tour est une tâche **détachée** : ni la requête ni l'abonné SSE ne l'annulent ;
        // le permis du verrou meurt avec elle (`Must` de `./manager.sdd`). Le jeton `forward`
        // ne vit que dans cette tâche, jamais dans la mémoire du gestionnaire.
        tokio::spawn(async move {
            let _permit = permit;
            let mut assembled =
                match assemble_session(&inner.deps.assemble, &caller, kind, &agent_id, &session, token).await
                {
                    Ok(assembled) => assembled,
                    Err(error) => {
                        tracing::warn!(error = %error, "lymaille manager: assembly failed, turn abandoned");
                        return;
                    }
                };
            *lock(&entry.cancel) = Some(assembled.cancel().clone());
            if entry.cancel_requested.load(Ordering::SeqCst) {
                assembled.cancel().cancel();
            }
            match assembled.run_turn(&text).await {
                Ok(output) => {
                    tracing::debug!(outcome = ?output.outcome(), "lymaille manager: turn ended");
                }
                Err(error) => {
                    tracing::warn!(error = %error, "lymaille manager: turn failed");
                }
            }
        });
        Ok(())
    }

    /// Abonnement aux événements de la session, après cloisonnement.
    ///
    /// # Errors
    ///
    /// [`LymailleError::NotFound`] (session absente ou d'un autre).
    pub async fn subscribe(&self, subject: &str, id: &SessionId) -> Result<Subscription, LymailleError> {
        self.principal(subject).await?;
        self.readable(subject, id).await?;
        Ok(self.inner.deps.assemble.hub.subscribe(subject, id))
    }

    /// Annule (coopérativement) le tour actif ; `true` si un tour était actif.
    ///
    /// # Errors
    ///
    /// [`LymailleError::NotFound`] (session absente ou d'un autre).
    pub async fn cancel(&self, subject: &str, id: &SessionId) -> Result<bool, LymailleError> {
        self.principal(subject).await?;
        self.readable(subject, id).await?;
        let active = self.inner.deps.gate.is_active(id);
        if active {
            self.signal(id);
        }
        Ok(active)
    }

    /// Reprend une session persistée : état relu via le store, avis de provenance rendus.
    ///
    /// # Errors
    ///
    /// [`LymailleError::NotFound`] (session absente, invisible, ou sans colonne `context`), puis
    /// les erreurs de l'assemblage de la reprise.
    pub async fn resume(&self, subject: &str, id: &SessionId) -> Result<SessionHandle, LymailleError> {
        let caller = self.principal(subject).await?;
        let state = self.readable(subject, id).await?;
        // La colonne `context` de la ligne est la source du contexte rendu ; une ligne qui n'en
        // porte pas (session d'avant le lot 2 — garde) est un `NotFound` indiscernable.
        let context = self
            .read_context_column(subject, id)
            .await?
            .ok_or_else(|| LymailleError::NotFound(SESSION_NOT_FOUND.to_string()))?;
        let kind = state.kind();
        let agent_id = state.agent().name().to_owned();
        // La restauration proprement dite est celle de `lymaille` (`restore_session` sous
        // l'assemblage) : les avis de provenance remontent à l'appelant, la reprise continue.
        let assembled =
            assemble_session(&self.inner.deps.assemble, &caller, kind, &agent_id, id, None).await?;
        self.inner.deps.assemble.hub.register(id, subject);
        // Une session fermée que l'on reprend est rouverte : la marque de fermeture est levée,
        // les tours sont de nouveau acceptés (l'état relisu est seul juge de sa disponibilité).
        lock(&self.inner.turns).reopen(id);
        self.touch(subject, id);
        Ok(SessionHandle {
            id: id.clone(),
            kind,
            agent_id,
            context,
            notices: assembled.notices().to_vec(),
        })
    }

    /// Ferme la session : annule un tour actif, ferme le hub, décharge la mémoire et la marque
    /// fermée — `send` rend dès lors le même `NotFound` qu'une session absente, jusqu'à une
    /// `resume`. L'état persisté demeure (la purge est `../store/purge.sdd`). Silencieuse sur
    /// session inconnue.
    pub async fn close(&self, subject: &str, id: &SessionId) {
        if self.principal(subject).await.is_err() || self.readable(subject, id).await.is_err() {
            tracing::debug!("lymaille manager: close on an unknown or invisible session");
            return;
        }
        if self.inner.deps.gate.is_active(id) {
            self.signal(id);
        }
        self.inner.deps.assemble.hub.close(id);
        lock(&self.inner.turns).close(id);
    }

    /// Résumés expurgés (titre masqué) des sessions de `subject` selon le filtre.
    ///
    /// # Errors
    ///
    /// [`LymailleError::Forbidden`] (principal inconnu), [`LymailleError::Store`] (panne d'index).
    pub async fn list(
        &self,
        subject: &str,
        filter: &SessionFilter,
    ) -> Result<Vec<SessionSummary>, LymailleError> {
        self.principal(subject).await?;
        let stores = self.stores(subject);
        let ids = stores.session_index().list(filter).await.map_err(from_session)?;
        let mut summaries = Vec::new();
        for id in ids {
            if let Some(state) = stores.sessions().load(&id).await.map_err(from_session)? {
                // Résumés expurgés : le titre passe par le `Redactor` (`from_state`).
                summaries.push(SessionSummary::from_state(&state, &Redactor::new()));
            }
        }
        Ok(summaries)
    }

    // ── Aides internes ──────────────────────────────────────────────────────

    /// Principal relu en base à cet appel ; un compte retiré entre deux appels échoue l'appel
    /// (`Must` : `UnknownSubject` → `Forbidden`).
    async fn principal(&self, subject: &str) -> Result<CallerPrincipal, LymailleError> {
        self.inner
            .identity
            .resolve(subject)
            .await
            .map_err(|error| match error {
                LymailleError::UnknownSubject => {
                    LymailleError::Forbidden("no local account for this session subject".to_string())
                }
                other => other,
            })
    }

    /// Stores liés au `subject`, hôte compris (résolution `H_…` des contextes à l'assemblage).
    fn stores(&self, subject: &str) -> LymailleStores {
        let stores = LymailleStores::new(self.inner.deps.assemble.db.clone(), subject.to_owned());
        match &self.inner.deps.assemble.host {
            Some(host) => stores.with_host(Arc::clone(host)),
            None => stores,
        }
    }

    /// Cloisonnement : la ligne (state) de la session lue par le store indexé `(subject, id)` —
    /// absente comme tenue par un autre rend le même `NotFound` (`Must not` de la spec).
    async fn readable(&self, subject: &str, id: &SessionId) -> Result<SessionState, LymailleError> {
        self.stores(subject)
            .sessions()
            .load(id)
            .await
            .map_err(from_session)?
            .ok_or_else(|| LymailleError::NotFound(SESSION_NOT_FOUND.to_string()))
    }

    /// Contexte d'un identifiant `H_…` : rendu par l'hôte pour ce principal ; absent comme
    /// hôte inexistant est le même `NotFound` indiscernable (`host.sdd`, échec fermé).
    async fn host_context_exists(
        &self,
        caller: &CallerPrincipal,
        context: &str,
    ) -> Result<(), LymailleError> {
        // Échec fermé (`host.sdd`) : hôte absent, `None` ou `Err` rendent le même `NotFound`.
        let found = match &self.inner.deps.assemble.host {
            Some(host) => host.resolve_context(context, caller).await.unwrap_or_default(),
            None => None,
        };
        if found.is_none() {
            return Err(LymailleError::NotFound(CONTEXT_NOT_FOUND.to_string()));
        }
        Ok(())
    }

    /// Résolution (`DEC-28` révisé le 2026-11-05) d'un contexte non hôte pour `open` : référence
    /// visible du type, `ContextType` amont (colonne `active`) et colonne `allowed_agents`,
    /// `lymaille::resolve_start` contre le catalogue d'agents (0.2.0), agent retenu et nature de
    /// la session. Référence absente ou type inexistant/inutilisable :
    /// `InvalidDefinition("context has no usable type")` ; type sans défaut sans agent explicite :
    /// `InvalidDefinition("agent required: the context type has no default agent")` ; agent hors
    /// de l'ensemble autorisé ou de nature incompatible avec le `session_kind` du type :
    /// `Forbidden`.
    async fn resolve_type_start(
        &self,
        stores: &LymailleStores,
        caller: &CallerPrincipal,
        kind: IdKind,
        context: &str,
        agent: Option<&str>,
    ) -> Result<(SessionKind, String, AgentDefinition), LymailleError> {
        let reference = self.visible_context_type(caller, kind, context).await?;
        let Some(reference) = reference else {
            return Err(LymailleError::InvalidDefinition(
                "context has no usable type".to_string(),
            ));
        };
        let Some((context_type, allowed)) = self.context_type(&reference).await? else {
            return Err(LymailleError::InvalidDefinition(
                "context has no usable type".to_string(),
            ));
        };
        // `lymaille::resolve_start` (0.2.0) : le `default_agent` du type résolu contre le
        // catalogue d'agents — introuvable, sous-agent ou nature incohérente avec le
        // `session_kind` du type → `InvalidDefinition`. Un type sans défaut ne résout
        // rien et le catalogue n'est pas lu.
        let start = ::lymaille::resolve_start(&context_type, &stores.agents())
            .await
            .map_err(|error| match error {
                ::lymaille::ToolsetsError::Store(text) => LymailleError::Store(text),
                other => LymailleError::InvalidDefinition(other.to_string()),
            })?;
        let agent_id = match agent.or_else(|| context_type.default_agent()) {
            Some(agent) => agent.to_owned(),
            None => {
                return Err(LymailleError::InvalidDefinition(
                    "agent required: the context type has no default agent".to_string(),
                ));
            }
        };
        if !allowed.iter().any(|allowed| allowed == &agent_id) {
            return Err(LymailleError::Forbidden(AGENT_NOT_ALLOWED.to_string()));
        }
        // L'agent retenu : le défaut déjà résolu par `resolve_start` (catalogue vérifié), un
        // agent explicite relu pour ce principal. Un `start` `None` équivaut à l'absence de
        // `default_agent` : atteint plus haut par le refus « agent required ».
        let definition = match (agent, start) {
            (Some(_), _) => self.resolve_agent(stores, &agent_id).await?,
            (None, Some(target)) => target.agent().clone(),
            (None, None) => {
                return Err(LymailleError::InvalidDefinition(
                    "agent required: the context type has no default agent".to_string(),
                ));
            }
        };
        // Nature (`DEC-28` révisé) : le `session_kind` du type quand il en porte un — la nature
        // de l'agent doit l'égaler — sinon la nature de l'agent résolu. Les `Sub` (et toute
        // nature amont inconnue d'ici) ne sont pas des natures de session interactive
        // (`Must not`).
        let kind = match context_type.session_kind() {
            Some(kind @ (SessionKind::Work | SessionKind::Config)) => {
                if definition.kind() != agent_kind_of(kind) {
                    return Err(LymailleError::Forbidden(AGENT_NOT_ALLOWED.to_string()));
                }
                kind
            }
            Some(_) | None => match definition.kind() {
                AgentKind::Work => SessionKind::Work,
                AgentKind::Config => SessionKind::Config,
                _ => {
                    return Err(LymailleError::Forbidden(AGENT_NOT_ALLOWED.to_string()));
                }
            },
        };
        Ok((kind, agent_id, definition))
    }

    /// Visibilité du contexte et colonne `context_type` de sa ligne : `G_…` du catalogue
    /// (public), sinon la table privée de l'appelant — un contexte d'un autre est un `NotFound`
    /// indiscernable.
    async fn visible_context_type(
        &self,
        caller: &CallerPrincipal,
        kind: IdKind,
        context: &str,
    ) -> Result<Option<String>, LymailleError> {
        let mut select = Query::select();
        select
            .column(Alias::new("context_type"))
            .from(Alias::new(if kind == IdKind::Catalog {
                CATALOG_CONTEXTS_TABLE
            } else {
                USER_CONTEXTS_TABLE
            }))
            .and_where(Expr::col(Alias::new("name")).eq(context));
        if kind == IdKind::Private {
            select.and_where(Expr::col(Alias::new("owner_id")).eq(caller.user_id));
        }
        let row = self
            .inner
            .deps
            .assemble
            .db
            .query_one(&select)
            .await
            .map_err(|error| LymailleError::Store(crate::lymaille::store::db_text(&error)))?;
        let Some(row) = row else {
            return Err(LymailleError::NotFound(CONTEXT_NOT_FOUND.to_string()));
        };
        row.try_get::<Option<String>>("", "context_type")
            .map_err(|error| LymailleError::Store(crate::lymaille::store::db_text(&error)))
    }

    /// Ligne du type de contexte (`DEC-28` révisé le 2026-11-05) : le `ContextType` amont (colonne
    /// `active`), qui porte la nature (`session_kind`) et l'agent par défaut (`default_agent`),
    /// plus la colonne miryad `allowed_agents`. Ligne absente, `active` nul ou JSON illisible —
    /// type inexistant ou inutilisable : `Ok(None)`, le refus « aucun type exploitable » est posé
    /// par l'appelant.
    async fn context_type(&self, name: &str) -> Result<Option<(ContextType, Vec<String>)>, LymailleError> {
        let select = Query::select()
            .columns([Alias::new("active"), Alias::new("allowed_agents")])
            .from(Alias::new(CATALOG_CONTEXT_TYPES_TABLE))
            .and_where(Expr::col(Alias::new("name")).eq(name))
            .to_owned();
        let row = self
            .inner
            .deps
            .assemble
            .db
            .query_one(&select)
            .await
            .map_err(|error| LymailleError::Store(crate::lymaille::store::db_text(&error)))?;
        let Some(row) = row else { return Ok(None) };
        let db_text = |error: sea_orm::DbErr| LymailleError::Store(crate::lymaille::store::db_text(&error));
        let active: Option<serde_json::Value> = row.try_get("", "active").map_err(db_text)?;
        let Some(brute) = active.filter(|version| !version.is_null()) else {
            return Ok(None);
        };
        let Ok(context_type) = serde_json::from_value::<ContextType>(brute) else {
            return Ok(None);
        };
        let allowed: serde_json::Value = row.try_get("", "allowed_agents").map_err(db_text)?;
        let allowed: Vec<String> = allowed.as_array().map_or_else(Vec::new, |values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        });
        Ok(Some((context_type, allowed)))
    }

    /// Définition de l'agent relue pour ce principal (privé ou catalogue) ; insoluble : le même
    /// `NotFound` indiscernable qu'un contexte invisible (`Raises` de `open`).
    async fn resolve_agent(
        &self,
        stores: &LymailleStores,
        agent_id: &str,
    ) -> Result<AgentDefinition, LymailleError> {
        stores
            .agents()
            .get_agent(agent_id)
            .await
            .map_err(|error| match error {
                ::lymaille::AgentsError::Store(text) => LymailleError::Store(text),
                other => LymailleError::InvalidDefinition(other.to_string()),
            })?
            .ok_or_else(|| LymailleError::NotFound(CONTEXT_NOT_FOUND.to_string()))
    }

    /// Versions effectives des toolsets **actifs** de la définition — l'état initial posé à
    /// l'ouverture porte les mêmes versions que l'assemblage, la reprise du premier tour
    /// n'émet donc aucun avis fantôme.
    async fn toolset_versions(
        &self,
        stores: &LymailleStores,
        definition: &AgentDefinition,
    ) -> Result<BTreeMap<String, String>, LymailleError> {
        let mut versions = BTreeMap::new();
        for name in definition.toolsets() {
            let toolset = stores
                .toolsets()
                .get_active(name)
                .await
                .map_err(|error| match error {
                    ::lymaille::ToolsetsError::Store(text) => LymailleError::Store(text),
                    other => LymailleError::InvalidDefinition(other.to_string()),
                })?
                .ok_or_else(|| LymailleError::NotFound(format!("unknown toolset: {name}")))?;
            let _entered: Option<String> = versions.insert(name.to_owned(), toolset.version().to_string());
        }
        Ok(versions)
    }

    /// Pose la colonne `context` de la ligne (immuable depuis l'ouverture — `DEC-28`).
    async fn stamp_context(&self, subject: &str, id: &SessionId, context: &str) -> Result<(), LymailleError> {
        let update = Query::update()
            .table(Alias::new(SESSIONS_TABLE))
            .values([(Alias::new("context"), Expr::val(context.to_owned()))])
            .and_where(Expr::col(Alias::new("subject")).eq(subject))
            .and_where(Expr::col(Alias::new("session_id")).eq(id.as_str()))
            .to_owned();
        self.inner
            .deps
            .assemble
            .db
            .execute(&update)
            .await
            .map_err(|error| LymailleError::Store(crate::lymaille::store::db_text(&error)))?;
        Ok(())
    }

    /// Colonne `context` brute de la ligne `(subject, id)`.
    async fn read_context_column(
        &self,
        subject: &str,
        id: &SessionId,
    ) -> Result<Option<String>, LymailleError> {
        let select = Query::select()
            .column(Alias::new("context"))
            .from(Alias::new(SESSIONS_TABLE))
            .and_where(Expr::col(Alias::new("subject")).eq(subject))
            .and_where(Expr::col(Alias::new("session_id")).eq(id.as_str()))
            .to_owned();
        let db_text = |error: sea_orm::DbErr| LymailleError::Store(crate::lymaille::store::db_text(&error));
        let row = self
            .inner
            .deps
            .assemble
            .db
            .query_one(&select)
            .await
            .map_err(db_text)?;
        match row {
            Some(row) => row.try_get::<Option<String>>("", "context").map_err(db_text),
            None => Ok(None),
        }
    }

    /// Touche mémoire : décharge les oisives (`idle_ttl`) et date l'usage de `id`.
    fn touch(&self, subject: &str, id: &SessionId) -> Arc<Entry> {
        let now_ms = self.inner.deps.assemble.clock.now().as_millis();
        let idle_ttl_ms = u64::try_from(self.inner.deps.limits.idle_ttl.as_millis()).unwrap_or(u64::MAX);
        let mut table = lock(&self.inner.turns);
        table.sweep(&self.inner.deps.gate, now_ms, idle_ttl_ms);
        table.touch(subject, id, now_ms)
    }

    /// Signale l'annulation au tour actif : l'anse publiée est annulée (coopératif), sinon le
    /// drapeau attend que la tâche du tour la publie.
    fn signal(&self, id: &SessionId) {
        let entry = lock(&self.inner.turns)
            .by_id
            .get(id)
            .map(|(_, entry)| Arc::clone(entry));
        let Some(entry) = entry else { return };
        match lock(&entry.cancel).as_ref().cloned() {
            Some(handle) => handle.cancel(),
            None => entry.cancel_requested.store(true, Ordering::SeqCst),
        }
    }
}

/// Verrou d'une table du gestionnaire : un verrou empoisonné est récupéré — l'état est une
/// `HashMap` simple qui ne peut pas rester à demi écrit (précédent `./hub.rs`).
/// Nature d'agent correspondant à une nature de session (`DEC-28` révisé : `work` ↔ `Work`,
/// `config` ↔ `Config` ; `Sub` n'est pas une nature de session interactive).
const fn agent_kind_of(kind: SessionKind) -> AgentKind {
    match kind {
        SessionKind::Work => AgentKind::Work,
        SessionKind::Config => AgentKind::Config,
        _ => AgentKind::Sub,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Erreur de session (`SessionError`) d'un store de sessions : panne de stockage → `Store`,
/// refus → `Run` non retryable (motif de `from_session` dans `./assemble.rs` ; la fonction y
/// est privée, le gestionnaire n'importe pas l'assemblage pour un simple mappage).
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
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::time::Duration;

    use sea_orm::ConnectionTrait as _;
    use sea_orm::DatabaseConnection;
    use sea_orm::sea_query::Alias;
    use sea_orm::sea_query::Expr;
    use sea_orm::sea_query::ExprTrait;
    use sea_orm::sea_query::Query;
    use serde_json::json;
    use zeroize::Zeroizing;

    use ::lymaille::AgentDefinition;
    use ::lymaille::AgentKind;
    use ::lymaille::AgentStore as _;
    use ::lymaille::ContextLevel;
    use ::lymaille::ContextType;
    use ::lymaille::InferenceRequest;
    use ::lymaille::InferenceResponse;
    use ::lymaille::Message;
    use ::lymaille::ModelProfile;
    use ::lymaille::ProfileStore as _;
    use ::lymaille::Provider;
    use ::lymaille::ProviderLevel;
    use ::lymaille::ProviderStore as _;
    use ::lymaille::Redactor;
    use ::lymaille::ScriptedProvider;
    use ::lymaille::SessionFilter;
    use ::lymaille::SessionId;
    use ::lymaille::SessionKind;
    use ::lymaille::SessionStore as _;
    use ::lymaille::SystemClock;
    use ::lymaille::ToolsetContext;
    use ::lymaille::TurnGate;
    use async_trait::async_trait;
    use vynil_core::Semver;

    use super::Entry;
    use super::Limits;
    use super::ManagerDeps;
    use super::SESSIONS_TABLE;
    use super::SessionManager;
    use crate::lymaille::host::HostError;
    use crate::lymaille::host::LymailleHost;
    use crate::lymaille::host::SharedHost;
    use crate::lymaille::identity::CallerPrincipal;
    use crate::lymaille::identity::Identity;
    use crate::lymaille::interactive::assemble::AssembleDeps;
    use crate::lymaille::interactive::assemble::DynInference;
    use crate::lymaille::interactive::assemble::InferenceFactory;
    use crate::lymaille::interactive::event::SessionEventKind;
    use crate::lymaille::interactive::hub::EventHub;
    use crate::lymaille::interactive::hub::Subscription;
    use crate::lymaille::interactive::hub::SubscriptionError;
    use crate::lymaille::mcp_mount::McpConnectorFactory;
    use crate::lymaille::resource_module::ResourceRegistry;
    use crate::lymaille::secret::SecretResolver;
    use crate::lymaille::store::LymailleStores;
    use crate::lymaille::store::test_support::test_db;
    use crate::users::group;
    use crate::users::membership::sync_group_memberships;
    use crate::users::resolve_user;

    // ── Fixtures ────────────────────────────────────────────────────────────
    //
    // Base SQLite mémoire migrée (`test_support::test_db`), catalogue câblé par `root` (profil
    // `qwen`, provider `local` d'auth `FIXED_KEY`), agents privés écrits par leur propriétaire
    // via les stores, lignes de contextes et de types posées en SQL brut (colonnes DEC-28 hors
    // objet `lymaille`). Les idiommes sont ceux de `./assemble.rs`.

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

    /// Définition d'agent de test, sans toolset (le tour ne monte que `resources`).
    fn agent(id: &str, kind: AgentKind) -> AgentDefinition {
        AgentDefinition::new(id.to_owned(), kind, "le prompt".to_owned(), "qwen".to_owned())
            .with_modules(vec!["resources".to_owned()])
    }

    /// Écrit un agent privé (par son propriétaire).
    async fn put_agent(db: &DatabaseConnection, owner: &CallerPrincipal, definition: &AgentDefinition) {
        LymailleStores::new(db.clone(), owner.subject.clone())
            .agents()
            .put_agent(definition)
            .await
            .expect("le proprietaire ecrit son agent");
    }

    /// Construit le JSON du `ContextType` de fixture : la nature (`session_kind` amont, `None` :
    /// le JSON n'en porte pas) et l'agent par défaut vivent dans le `ContextType` de `lymaille`
    /// 0.2.0 (DSD `DEC-28` révisé le 2026-11-05), colonne `active` (ou `draft` pour la ligne au
    /// seul brouillon).
    fn brut_type(
        name: &str,
        session_kind: Option<SessionKind>,
        default_agent: Option<&str>,
    ) -> serde_json::Value {
        let context_type = ContextType::new(
            name.to_owned(),
            Semver::parse("1.0.0").expect("la version de fixture se parse"),
            json!({}),
        )
        .expect("le type de fixture se construit");
        let context_type = match session_kind {
            Some(kind) => context_type
                .with_session_kind(kind)
                .expect("la nature de fixture se pose"),
            None => context_type,
        };
        let context_type = match default_agent {
            Some(agent) => context_type
                .with_default_agent(agent.to_owned())
                .expect("l'agent par défaut de fixture se pose"),
            None => context_type,
        };
        serde_json::to_value(context_type).expect("le type se serialise")
    }

    /// Pose une ligne de `catalog_context_types` avec les colonnes DEC-28 (`allowed_agents`,
    /// `../migration.sdd`) et l'`created_at` courant, les colonnes `active` et `draft` telles que
    /// passées.
    async fn insert_type(
        db: &DatabaseConnection,
        name: &str,
        allowed: &[&str],
        active: sea_orm::sea_query::SimpleExpr,
        draft: sea_orm::sea_query::SimpleExpr,
    ) {
        let mut insert = Query::insert();
        insert
            .into_table(Alias::new(super::CATALOG_CONTEXT_TYPES_TABLE))
            .columns([
                "name",
                "version",
                "allowed_agents",
                "active",
                "draft",
                "created_at",
            ])
            .values(vec![
                Expr::val(name),
                Expr::val("1.0.0"),
                Expr::val(serde_json::Value::Array(
                    allowed.iter().map(|agent| json!(*agent)).collect(),
                )),
                active,
                draft,
                Expr::val(chrono::Utc::now()),
            ])
            .expect("les colonnes et valeurs de fixture correspondent");
        db.execute(&insert).await.expect("ligne de type inseree");
    }

    /// Écrit une ligne de `catalog_context_types` active : le JSON du type en colonne `active`,
    /// `draft` nul, `allowed_agents` en colonne DEC-28.
    async fn put_type(
        db: &DatabaseConnection,
        name: &str,
        session_kind: Option<SessionKind>,
        default_agent: Option<&str>,
        allowed: &[&str],
    ) {
        let active = brut_type(name, session_kind, default_agent);
        insert_type(db, name, allowed, Expr::val(active), Expr::null()).await;
    }

    /// Écrit une ligne de `catalog_context_types` au **seul brouillon** (troisième cas du
    /// `Scenario` « un contexte sans type exploitable est refusé », arbitrage du 2026-11-06) :
    /// `active` nul, `draft` porteur d'un type par ailleurs exploitable — c'est le refus du
    /// repli sur le brouillon qui se joue ici, pas la forme du type.
    async fn put_type_au_seul_draft(
        db: &DatabaseConnection,
        name: &str,
        session_kind: Option<SessionKind>,
        default_agent: Option<&str>,
        allowed: &[&str],
    ) {
        let draft = brut_type(name, session_kind, default_agent);
        insert_type(db, name, allowed, Expr::null(), Expr::val(draft)).await;
    }

    /// Ligne de contexte privé `(owner, name)` de type `context_type` (`None` : colonne non
    /// posée — le Given des deux refus « aucun type exploitable »).
    async fn inserer_contexte_prive(
        db: &DatabaseConnection,
        owner_id: i32,
        name: &str,
        context_type: Option<&str>,
    ) {
        let context = ToolsetContext::new(name.to_owned(), ContextLevel::User)
            .expect("le contexte de fixture se construit");
        let value = serde_json::to_value(context).expect("le contexte se serialise");
        let mut insert = Query::insert();
        insert
            .into_table(Alias::new(super::USER_CONTEXTS_TABLE))
            .columns(["owner_id", "name", "context", "context_type", "created_at"])
            .values(vec![
                Expr::val(owner_id),
                Expr::val(name),
                Expr::val(value),
                context_type.map_or_else(Expr::null, |name| Expr::val(name.to_owned())),
                Expr::val(chrono::Utc::now()),
            ])
            .expect("les colonnes et valeurs de fixture correspondent");
        db.execute(&insert).await.expect("ligne de contexte inseree");
    }

    /// Dépendances de test : fabrique d'inférence injectée, connecteurs MCP sans usage (aucun
    /// toolset dans les fixtures), horloge système, limites par défaut.
    fn deps_with(
        db: DatabaseConnection,
        inference: Arc<dyn InferenceFactory>,
        host: Option<SharedHost>,
        limits: Limits,
    ) -> ManagerDeps {
        ManagerDeps {
            assemble: AssembleDeps {
                db,
                host_modules: Arc::new(crate::lymaille::host_module::HostModules::new()),
                readers: Arc::new(ResourceRegistry::new()),
                secrets: Arc::new(FixedSecret("cle-fixe")),
                hub: Arc::new(EventHub::new(None, 256)),
                clock: Arc::new(SystemClock::new()),
                host,
                inference,
                mcp: Arc::new(NoMcp),
            },
            gate: TurnGate::new(),
            limits,
        }
    }

    /// Fabrique qui rend le même provider scripté à chaque assemblage (motif de `./assemble.rs`).
    struct ScriptFactory(Arc<ScriptedProvider>);

    impl InferenceFactory for ScriptFactory {
        fn make(&self, _address: String, _api_key: Option<String>) -> Arc<dyn DynInference> {
            let provider: Arc<dyn DynInference> = self.0.clone();
            provider
        }
    }
    /// Réponse finale sans appel d'outil.
    fn final_text(text: &str) -> InferenceResponse {
        InferenceResponse::new(Message::assistant(text.to_owned(), None, Vec::new()), None)
    }

    /// Fabrique qui **bloque** dans `complete` jusqu'à `add_permits` : le tour reste actif
    /// jusqu'à l'annulation (le tour de `lymaille` abandonne le futur au premier sondage
    /// d'annulation). `entered` est posé dès que le provider est appelé — l'attente que le tour
    /// est lancé. Le sémaphore démarre à zéro permis : le provider ne peut pas rendre la main
    /// de lui-même, seul le test le libère.
    struct HangFactory(Arc<Hang>);

    struct Hang {
        entered: AtomicBool,
        release: tokio::sync::Semaphore,
    }

    impl Hang {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                entered: AtomicBool::new(false),
                release: tokio::sync::Semaphore::new(0),
            })
        }
    }

    impl InferenceFactory for HangFactory {
        fn make(&self, _address: String, _api_key: Option<String>) -> Arc<dyn DynInference> {
            Arc::new(HangInference(Arc::clone(&self.0)))
        }
    }

    struct HangInference(Arc<Hang>);

    impl DynInference for HangInference {
        fn complete_dyn(
            &self,
            _request: InferenceRequest,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<InferenceResponse, ::lymaille::InferenceError>> + Send + '_>,
        > {
            let hang = Arc::clone(&self.0);
            Box::pin(async move {
                hang.entered.store(true, AtomicOrdering::SeqCst);
                match hang.release.acquire().await {
                    Ok(_permit) => Ok(final_text("jamais rendu")),
                    Err(_closed) => Ok(final_text("libere")),
                }
            })
        }
    }

    /// Fabrique MCP sans usage : aucun toolset dans les fixtures, donc jamais appelée ; si elle
    /// l'était, un connecteur qui ne liste aucun outil.
    struct NoMcp;

    impl McpConnectorFactory for NoMcp {
        fn make(
            &self,
            _url: &str,
            _request_timeout: Duration,
            _connect_timeout: Duration,
            _auth: Option<Arc<dyn crate::lymaille::mcp_mount::DynMcpAuth>>,
        ) -> Arc<dyn crate::lymaille::mcp_mount::DynMcpConnector> {
            Arc::new(NoConnector)
        }
    }

    struct NoConnector;

    impl ::lymaille::McpLister for NoConnector {
        fn list_tools(
            &self,
        ) -> impl Future<Output = Result<Vec<::lymaille::McpTool>, ::lymaille::McpError>> + Send {
            std::future::ready(Ok(Vec::new()))
        }
    }

    impl ::lymaille::McpConnector for NoConnector {
        fn call_tool(
            &self,
            _name: &str,
            _arguments: &serde_json::Value,
        ) -> impl Future<Output = Result<serde_json::Value, ::lymaille::McpError>> + Send {
            std::future::ready(Ok(json!({})))
        }

        fn reconnect(&self) -> impl Future<Output = ()> + Send {
            std::future::ready(())
        }
    }

    /// Résolveur de secrets de test (`./assemble.rs`).
    struct FixedSecret(&'static str);

    impl SecretResolver for FixedSecret {
        fn resolve(
            &self,
            reference: &str,
        ) -> Result<Zeroizing<String>, crate::lymaille::error::LymailleError> {
            if reference == "FIXED_KEY" {
                Ok(Zeroizing::new(self.0.to_owned()))
            } else {
                Err(crate::lymaille::error::LymailleError::SecretUnresolved(
                    reference.to_owned(),
                ))
            }
        }
    }

    /// Hôte factice : rend le contexte `H_sbx` au seul `alice`.
    struct SpyHost;

    #[async_trait]
    impl LymailleHost for SpyHost {
        async fn resolve_context(
            &self,
            id: &str,
            caller: &CallerPrincipal,
        ) -> Result<Option<ToolsetContext>, HostError> {
            if id == "H_sbx" && caller.subject == "alice" {
                Ok(Some(
                    ToolsetContext::new("H_sbx".to_owned(), ContextLevel::Shared)
                        .expect("le contexte hote de fixture se construit"),
                ))
            } else {
                Ok(None)
            }
        }
    }

    /// Le tour d'installation d'un scénario type : catalogue, agent `dev` (nature `work`) privé
    /// d'alice, type `t` (`work`, défaut `dev`, autorisé `dev`) et contexte privé `c1` de ce type.
    /// Rend le principal d'alice.
    async fn setup_work(db: &DatabaseConnection) -> CallerPrincipal {
        wire_catalog(db).await;
        let alice = compte(db, "alice", false).await;
        put_agent(db, &alice, &agent("dev", AgentKind::Work)).await;
        put_type(db, "t", Some(SessionKind::Work), Some("dev"), &["dev"]).await;
        inserer_contexte_prive(db, alice.user_id, "c1", Some("t")).await;
        alice
    }

    /// Draine les événements jusqu'à `TurnEnded` (ou 5 s) ; la liste des kinds reçus.
    async fn drain(sub: &mut Subscription) -> Vec<SessionEventKind> {
        let mut kinds = Vec::new();
        loop {
            let rendu = tokio::time::timeout(Duration::from_secs(5), sub.recv())
                .await
                .expect("un evenement arrive avant 5 s");
            let kind = rendu.expect("le canal est ouvert").kind;
            let termine = matches!(kind, SessionEventKind::TurnEnded { .. });
            kinds.push(kind);
            if termine {
                return kinds;
            }
        }
    }

    /// Attend (5 s max) que le prédicat sur l'état persisté de la session soit vrai.
    async fn wait_state(db: &DatabaseConnection, subject: &str, id: &SessionId, want: impl Fn(&str) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let stores = LymailleStores::new(db.clone(), subject.to_owned());
            let charge = stores
                .sessions()
                .load(id)
                .await
                .expect("lecture de l'etat de session");
            if let Some(state) = charge {
                let texte = state.to_json().expect("l'etat se serialise");
                if want(&texte) {
                    return;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "l'etat attendu n'est jamais apparu"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Attend (5 s max) que le provider bloqué ait été appelé — le tour est lancé.
    async fn wait_entered(hang: &Hang) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !hang.entered.load(AtomicOrdering::SeqCst) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "le provider n'a jamais ete appele"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Colonne `context` brute de la ligne de session.
    async fn context_column(db: &DatabaseConnection, subject: &str, id: &SessionId) -> Option<String> {
        let select = Query::select()
            .columns([Alias::new("context")])
            .from(Alias::new(SESSIONS_TABLE))
            .and_where(Expr::col(Alias::new("subject")).eq(subject))
            .and_where(Expr::col(Alias::new("session_id")).eq(id.as_str()))
            .to_owned();
        db.query_one(&select)
            .await
            .expect("relecture de la ligne de session")
            .and_then(|row| {
                row.try_get::<Option<String>>("", "context")
                    .expect("colonne relue")
            })
    }

    /// Nombre de lignes de sessions persistées.
    async fn session_count(db: &DatabaseConnection) -> usize {
        let select = Query::select()
            .columns([Alias::new("session_id")])
            .from(Alias::new(SESSIONS_TABLE))
            .to_owned();
        db.query_all(&select).await.expect("comptage des sessions").len()
    }

    /// L'anse d'annulation d'une entrée de test — vérifie que la mémoire du gestionnaire est
    /// bien l'objet de ces scénarios (`Entry` n'est pas une surface publique).
    #[test]
    fn une_entree_neuve_nest_annulee_ni_publiee() {
        let entree = Entry::new(0);
        assert!(!entree.cancel_requested.load(AtomicOrdering::SeqCst));
        assert!(
            entree
                .cancel
                .lock()
                .expect("verrou d entree non empoisonne")
                .is_none()
        );
    }

    // ── Scénarios ───────────────────────────────────────────────────────────

    /// Scenario « ouvrir, envoyer, recevoir la réponse par le hub » : la nature vient du type,
    /// l'agent est le défaut du type, les événements arrivent en ordre et l'état est sauvegardé.
    #[tokio::test(flavor = "multi_thread")]
    async fn ouvrir_envoyer_recevoir_par_le_hub() {
        let db = test_db().await;
        setup_work(&db).await;
        let provider = Arc::new(ScriptedProvider::new(vec![Ok(final_text("bonjour toi"))]));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            None,
            Limits::default(),
        ));

        let handle = manager
            .open("alice", "c1", None)
            .await
            .expect("la session s'ouvre sur le contexte de type work");
        assert_eq!(handle.kind, SessionKind::Work, "la nature vient du type");
        assert_eq!(handle.agent_id, "dev", "l'agent est le defaut du type");
        assert_eq!(handle.context, "c1", "le contexte est rendu");
        assert!(handle.notices.is_empty(), "une ouverture n'emmet aucun avis");

        let mut sub = manager
            .subscribe("alice", &handle.id)
            .await
            .expect("l'abonnement de la proprietaire reussit");
        manager
            .send("alice", &handle.id, "bonjour".to_owned(), None)
            .await
            .expect("le tour est lance");
        let kinds = drain(&mut sub).await;
        assert!(
            matches!(kinds.first(), Some(SessionEventKind::TurnStarted { .. })),
            "le tour commence par TurnStarted : {kinds:?}"
        );
        assert!(
            kinds
                .iter()
                .any(|kind| matches!(kind, SessionEventKind::TextDelta { .. })),
            "le texte du tour est diffuse par fragments : {kinds:?}"
        );
        assert!(
            matches!(kinds.last(), Some(SessionEventKind::TurnEnded { .. })),
            "le tour se termine par TurnEnded : {kinds:?}"
        );
        wait_state(&db, "alice", &handle.id, |etat| etat.contains("bonjour")).await;
    }

    /// Scenario « un second tour est refusé » : `Conflict("turn active")` pendant que le premier
    /// court, et le premier poursuit jusqu'au bout.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_second_tour_est_refuse() {
        let db = test_db().await;
        setup_work(&db).await;
        let hang = Hang::new();
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(HangFactory(Arc::clone(&hang))),
            None,
            Limits::default(),
        ));
        let handle = manager.open("alice", "c1", None).await.expect("ouverture");
        let sub = manager.subscribe("alice", &handle.id).await.expect("abonnement");
        manager
            .send("alice", &handle.id, "premier".to_owned(), None)
            .await
            .expect("le premier tour est lance");
        wait_entered(&hang).await;

        let refus = manager
            .send("alice", &handle.id, "second".to_owned(), None)
            .await
            .expect_err("un tour est deja actif");
        match refus {
            crate::lymaille::error::LymailleError::Conflict(message) => {
                assert_eq!(message, "turn active", "le conflit nomme l'etat : {message}");
            }
            autre => panic!("une `Conflict` est attendue, obtenu : {autre:?}"),
        }
        drop(sub);
        let sub = manager
            .subscribe("alice", &handle.id)
            .await
            .expect("reabonnement");
        let mut sub = sub;
        // Le premier tour poursuit : il etait bloque dans le provider, on le libere et il
        // s'acheve sans que le refus du second l'ait interrompu.
        hang.release.add_permits(1);
        let kinds = drain(&mut sub).await;
        assert!(
            matches!(kinds.last(), Some(SessionEventKind::TurnEnded { .. })),
            "le premier tour se termine normalement : {kinds:?}"
        );
    }

    /// Scenario « les sessions sont cloisonnées » : `send`, `subscribe`, `cancel`, `resume` par
    /// un autre, comme par un identifiant inconnu, sont le même `NotFound`.
    #[tokio::test(flavor = "multi_thread")]
    async fn les_sessions_sont_cloisonnees() {
        let db = test_db().await;
        setup_work(&db).await;
        let bob = compte(&db, "bob", false).await;
        let _ = bob;
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            None,
            Limits::default(),
        ));
        let handle = manager.open("alice", "c1", None).await.expect("ouverture");
        let inconnu = SessionId::new("s-inconnu").expect("identifiant de test");

        let textes: Vec<String> = vec![
            manager
                .send("bob", &handle.id, "x".to_owned(), None)
                .await
                .expect_err("bob ne parle pas dans la session d'une autre")
                .to_string(),
            format!(
                "{}",
                manager
                    .subscribe("bob", &handle.id)
                    .await
                    .expect_err("bob ne s'abonne pas a la session d'une autre")
            ),
            format!(
                "{}",
                manager
                    .cancel("bob", &handle.id)
                    .await
                    .expect_err("bob n'annule pas le tour d'une autre")
            ),
            format!(
                "{}",
                manager
                    .resume("bob", &handle.id)
                    .await
                    .expect_err("bob ne reprend pas la session d'une autre")
            ),
            format!(
                "{}",
                manager
                    .resume("bob", &inconnu)
                    .await
                    .expect_err("une session inconnue est le meme refus")
            ),
        ];
        for texte in &textes[1..] {
            assert_eq!(texte, &textes[0], "les refus sont indiscernables");
        }
        assert!(
            textes[0].contains("MRD-LYMAILLE-010"),
            "le refus cite NotFound (MRD-LYMAILLE-010) : {}",
            textes[0]
        );
    }

    /// Scenario « un tour survit à la déconnexion du client » : l'abonné détruit, le tour se
    /// termine et l'état est sauvegardé (la tâche du tour est détachée de la requête).
    #[tokio::test(flavor = "multi_thread")]
    async fn un_tour_survit_a_la_deconnexion_du_client() {
        let db = test_db().await;
        setup_work(&db).await;
        let provider = Arc::new(ScriptedProvider::new(vec![Ok(final_text("reponse finale"))]));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            None,
            Limits::default(),
        ));
        let handle = manager.open("alice", "c1", None).await.expect("ouverture");
        let sub = manager.subscribe("alice", &handle.id).await.expect("abonnement");
        manager
            .send("alice", &handle.id, "bonjour".to_owned(), None)
            .await
            .expect("le tour est lance");
        drop(sub);
        // Le client est parti ; `TurnEnded` a ete emis vers une file sans abonné (diffusion
        // abandonnee, `./hub.sdd`) — la trace en est l'etat sauvegarde par le tour acheve.
        wait_state(&db, "alice", &handle.id, |etat| {
            etat.contains("bonjour") && etat.contains("reponse finale")
        })
        .await;
    }

    /// Scenario « annuler » : `true` pendant le tour, fin `Cancelled`, `false` ensuite.
    #[tokio::test(flavor = "multi_thread")]
    async fn annuler_un_tour() {
        let db = test_db().await;
        setup_work(&db).await;
        let hang = Hang::new();
        // Le provider bloque jusqu'a liberation explicite ; l'annulation doit l'interrompre
        // avant : seul le sondage d'annulation arrete le tour.
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(HangFactory(Arc::clone(&hang))),
            None,
            Limits::default(),
        ));
        let handle = manager.open("alice", "c1", None).await.expect("ouverture");
        let mut sub = manager.subscribe("alice", &handle.id).await.expect("abonnement");
        manager
            .send("alice", &handle.id, "long".to_owned(), None)
            .await
            .expect("le tour long est lance");
        wait_entered(&hang).await;

        assert!(
            manager.cancel("alice", &handle.id).await.expect("cancel"),
            "un tour etait actif"
        );
        let kinds = drain(&mut sub).await;
        match kinds.last() {
            Some(SessionEventKind::TurnEnded { outcome, .. }) => assert_eq!(
                *outcome,
                crate::lymaille::interactive::event::SessionOutcome::Cancelled,
                "le tour se termine annule : {kinds:?}"
            ),
            autre => panic!("TurnEnded attendu en dernier : {autre:?}"),
        }
        assert!(
            !manager.cancel("alice", &handle.id).await.expect("cancel"),
            "plus aucun tour actif apres la fin"
        );
    }

    /// Scenario « reprendre après redémarrage » : gestionnaire neuf, `resume` puis `send` —
    /// l'historique précédent est présent dans la reprise comme dans la requête au modèle.
    #[tokio::test(flavor = "multi_thread")]
    async fn reprendre_apres_redemarrage() {
        let db = test_db().await;
        setup_work(&db).await;
        let premier = Arc::new(ScriptedProvider::new(vec![Ok(final_text("reponse un"))]));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(premier)),
            None,
            Limits::default(),
        ));
        let handle = manager.open("alice", "c1", None).await.expect("ouverture");
        let mut sub = manager.subscribe("alice", &handle.id).await.expect("abonnement");
        manager
            .send("alice", &handle.id, "bonjour".to_owned(), None)
            .await
            .expect("premier tour lance");
        drain(&mut sub).await;
        drop(sub);

        // « Redémarrage » : un gestionnaire neuf sur la même base.
        let second = Arc::new(ScriptedProvider::new(vec![Ok(final_text("reponse deux"))]));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&second))),
            None,
            Limits::default(),
        ));
        let repris = manager
            .resume("alice", &handle.id)
            .await
            .expect("la reprise reussit");
        assert_eq!(repris.kind, SessionKind::Work, "la nature est rechargee");
        assert_eq!(repris.agent_id, "dev", "l'agent est recharge");
        assert_eq!(repris.context, "c1", "le contexte est recharge");
        let mut sub = manager
            .subscribe("alice", &handle.id)
            .await
            .expect("reabonnement");
        manager
            .send("alice", &handle.id, "la suite".to_owned(), None)
            .await
            .expect("le tour est relance");
        drain(&mut sub).await;

        let requetes = second.requests();
        let premier = requetes.last().expect("le modele a ete appele apres la reprise");
        let messages =
            serde_json::to_value(premier.messages()).expect("les messages de la requete se serialisent");
        let texte = messages.to_string();
        assert!(
            texte.contains("bonjour") && texte.contains("reponse un"),
            "l'historique precedent est present dans la reprise : {texte}"
        );
    }

    /// Scenario « reprise avec avis » : l'agent modifié depuis le dernier tour est rendu en
    /// `notices` non vides, la reprise continue.
    #[tokio::test(flavor = "multi_thread")]
    async fn reprise_avec_avis() {
        let db = test_db().await;
        setup_work(&db).await;
        let provider = Arc::new(ScriptedProvider::new(vec![Ok(final_text("reponse un"))]));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            None,
            Limits::default(),
        ));
        let handle = manager.open("alice", "c1", None).await.expect("ouverture");
        let mut sub = manager.subscribe("alice", &handle.id).await.expect("abonnement");
        manager
            .send("alice", &handle.id, "bonjour".to_owned(), None)
            .await
            .expect("tour lance");
        drain(&mut sub).await;
        drop(sub);

        let alice = compte(&db, "alice", false).await;
        let modifie = AgentDefinition::new(
            "dev".to_owned(),
            AgentKind::Work,
            "un prompt revise".to_owned(),
            "qwen".to_owned(),
        )
        .with_modules(vec!["resources".to_owned()]);
        put_agent(&db, &alice, &modifie).await;

        let second = Arc::new(ScriptedProvider::new(Vec::new()));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(second)),
            None,
            Limits::default(),
        ));
        let repris = manager
            .resume("alice", &handle.id)
            .await
            .expect("la reprise continue malgré l'écart");
        assert!(
            !repris.notices.is_empty(),
            "l'empreinte changee est rendue en avis"
        );
    }

    /// Scenario « la nature vient du type, l'agent doit être autorisé » : ouvrir une `Config`
    /// n'exige aucun admin (FR-9) ; l'agent hors `allowed_agents` est le `Forbidden` typé
    /// `MRD-LYMAILLE-002` (AC-19). Les noms d'agents portent `_` : la grammaire d'identifiants
    /// de `lymaille` (`DEC-9`) refuse le tiret, les noms du scenario sont illustratifs.
    #[tokio::test(flavor = "multi_thread")]
    async fn la_nature_vient_du_type_l_agent_doit_etre_autorise() {
        let db = test_db().await;
        wire_catalog(&db).await;
        let alice = compte(&db, "alice", false).await;
        put_agent(&db, &alice, &agent("a_config", AgentKind::Config)).await;
        put_agent(&db, &alice, &agent("a_work", AgentKind::Work)).await;
        put_type(
            &db,
            "t",
            Some(SessionKind::Config),
            Some("a_config"),
            &["a_config"],
        )
        .await;
        inserer_contexte_prive(&db, alice.user_id, "c1", Some("t")).await;

        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            None,
            Limits::default(),
        ));

        let handle = manager
            .open("alice", "c1", None)
            .await
            .expect("ouvrir une Config n'exige aucun admin (FR-9)");
        assert_eq!(handle.kind, SessionKind::Config, "la nature vient du type");
        assert_eq!(handle.agent_id, "a_config", "l'agent est le defaut du type");

        let refus = manager
            .open("alice", "c1", Some("a_work"))
            .await
            .expect_err("a_work n'appartient pas a allowed_agents");
        match &refus {
            crate::lymaille::error::LymailleError::Forbidden(message) => {
                assert_eq!(
                    message, "agent not allowed for this context type",
                    "le refus nomme la regle : {message}"
                );
            }
            autre => panic!("un `Forbidden` est attendu, obtenu : {autre:?}"),
        }
        assert!(
            refus.to_string().contains("MRD-LYMAILLE-002"),
            "l'erreur typée cite MRD-LYMAILLE-002 (AC-19) : {refus}"
        );
    }

    /// Tâche « Alignement `lymaille` 0.2.0 » (`./manager.sdd`, `Must` `DEC-28` révisé) : un type
    /// sans `default_agent` (lu dans le `ContextType` amont, plus en colonne) exige un agent
    /// explicite — `InvalidDefinition("agent required: the context type has no default agent")` ;
    /// avec un agent explicite autorisé, la session s'ouvre et la nature vient de cet agent.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_type_sans_defaut_exige_un_agent_explicite() {
        let db = test_db().await;
        wire_catalog(&db).await;
        let alice = compte(&db, "alice", false).await;
        put_agent(&db, &alice, &agent("dev", AgentKind::Work)).await;
        put_type(&db, "t", None, None, &["dev"]).await;
        inserer_contexte_prive(&db, alice.user_id, "c1", Some("t")).await;

        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            None,
            Limits::default(),
        ));

        let refus = manager
            .open("alice", "c1", None)
            .await
            .expect_err("un type sans agent par défaut sans agent explicite est refusé");
        match &refus {
            crate::lymaille::error::LymailleError::InvalidDefinition(message) => {
                assert_eq!(
                    message, "agent required: the context type has no default agent",
                    "le refus nomme la regle : {message}"
                );
            }
            autre => panic!("un `InvalidDefinition` est attendu, obtenu : {autre:?}"),
        }

        let handle = manager
            .open("alice", "c1", Some("dev"))
            .await
            .expect("un agent explicite autorise l'ouverture sur un type sans defaut");
        assert_eq!(
            handle.kind,
            SessionKind::Work,
            "sans session_kind du type, la nature vient de l'agent"
        );
        assert_eq!(handle.agent_id, "dev");
    }

    /// Tâche « Alignement `lymaille` 0.2.0 » (`./manager.sdd`, `Must`) : `lymaille::resolve_start`
    /// résout le `default_agent` du type contre le catalogue d'agents — un défaut introuvable est
    /// un `InvalidDefinition`, aucune session n'est créée.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_defaut_de_type_introuvable_refuse_l_ouverture() {
        let db = test_db().await;
        wire_catalog(&db).await;
        let alice = compte(&db, "alice", false).await;
        put_type(&db, "t", Some(SessionKind::Work), Some("ghost"), &["ghost"]).await;
        inserer_contexte_prive(&db, alice.user_id, "c1", Some("t")).await;

        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            None,
            Limits::default(),
        ));

        let refus = manager
            .open("alice", "c1", None)
            .await
            .expect_err("le default_agent du type doit exister dans le catalogue");
        assert!(
            matches!(refus, crate::lymaille::error::LymailleError::InvalidDefinition(_)),
            "resolve_start rend un `InvalidDefinition`, obtenu : {refus:?}"
        );
        let compte_sessions = Query::select()
            .column(Alias::new("session_id"))
            .from(Alias::new(super::SESSIONS_TABLE))
            .to_owned();
        let lignes = db
            .query_all(&compte_sessions)
            .await
            .expect("comptage des sessions");
        assert!(
            lignes.is_empty(),
            "aucune session n'est creee sur un defaut insoluble"
        );
    }

    /// Scenario « un contexte sans type exploitable est refusé » : référence absente comme
    /// référence sans type existant rendent le même `InvalidDefinition`, aucune session créée.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_contexte_sans_type_exploitable_est_refuse() {
        let db = test_db().await;
        wire_catalog(&db).await;
        let alice = compte(&db, "alice", false).await;
        inserer_contexte_prive(&db, alice.user_id, "c_sans_type", None).await;
        inserer_contexte_prive(&db, alice.user_id, "c_fantome", Some("t_fantome")).await;

        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            None,
            Limits::default(),
        ));

        for contexte in ["c_sans_type", "c_fantome"] {
            let refus = manager
                .open("alice", contexte, None)
                .await
                .expect_err("aucun type exploitable ne doit ouvrir de session");
            match refus {
                crate::lymaille::error::LymailleError::InvalidDefinition(message) => assert_eq!(
                    message, "context has no usable type",
                    "le refus nomme la regle pour {contexte}"
                ),
                autre => panic!("un `InvalidDefinition` est attendu pour {contexte} : {autre:?}"),
            }
        }
        assert_eq!(session_count(&db).await, 0, "aucune session creee");
    }

    /// Scenario « un contexte sans type exploitable est refusé », troisième cas (spec amendée par
    /// l'arbitrage du 2026-11-06) : un contexte dont le type n'a qu'un `draft` (`active` nul) est
    /// refusé par le même `InvalidDefinition("context has no usable type")` — l'ouverture se lit
    /// sur la version **active** comme `get_type`, sans repli sur le brouillon, même si le
    /// brouillon porte un type par ailleurs exploitable. Aucune session créée.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_contexte_de_type_au_seul_draft_est_refuse() {
        let db = test_db().await;
        wire_catalog(&db).await;
        let alice = compte(&db, "alice", false).await;
        put_agent(&db, &alice, &agent("dev", AgentKind::Work)).await;
        put_type_au_seul_draft(&db, "t_brouillon", Some(SessionKind::Work), Some("dev"), &["dev"]).await;
        inserer_contexte_prive(&db, alice.user_id, "c_brouillon", Some("t_brouillon")).await;

        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            None,
            Limits::default(),
        ));

        let refus = manager
            .open("alice", "c_brouillon", None)
            .await
            .expect_err("un type au seul draft ne doit ouvrir de session");
        match refus {
            crate::lymaille::error::LymailleError::InvalidDefinition(message) => assert_eq!(
                message, "context has no usable type",
                "le refus nomme la regle : {message}"
            ),
            autre => panic!("un `InvalidDefinition` est attendu : {autre:?}"),
        }
        assert_eq!(session_count(&db).await, 0, "aucune session creee");
    }

    /// Scenario « un contexte hôte exige un agent de travail explicite » : les trois volets de
    /// l'arbitrage hôte (`H_…` sans type, agent obligatoire, nature `Work` seulement).
    #[tokio::test(flavor = "multi_thread")]
    async fn un_contexte_hote_exige_un_agent_de_travail_explicite() {
        let db = test_db().await;
        wire_catalog(&db).await;
        let alice = compte(&db, "alice", false).await;
        put_agent(&db, &alice, &agent("a_work", AgentKind::Work)).await;
        put_agent(&db, &alice, &agent("a_config", AgentKind::Config)).await;

        let host = Arc::new(SpyHost) as SharedHost;
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            Some(host),
            Limits::default(),
        ));

        let refus = manager
            .open("alice", "H_sbx", None)
            .await
            .expect_err("le contexte hote n'a pas de defaut");
        assert!(
            matches!(&refus, crate::lymaille::error::LymailleError::InvalidDefinition(message)
                if message == "host sessions need an explicit agent"),
            "un agent explicite est exige : {refus:?}"
        );

        let handle = manager
            .open("alice", "H_sbx", Some("a_work"))
            .await
            .expect("une session Work s'ouvre sur le contexte hote");
        assert_eq!(
            handle.kind,
            SessionKind::Work,
            "la nature d'un contexte hote est Work"
        );
        assert_eq!(handle.context, "H_sbx", "le contexte hote est persiste");

        let refus = manager
            .open("alice", "H_sbx", Some("a_config"))
            .await
            .expect_err("une Config ne s'ouvre pas sur un contexte hote");
        assert!(
            matches!(&refus, crate::lymaille::error::LymailleError::InvalidDefinition(message)
                if message == "host contexts do not open config sessions"),
            "la Config est refusee sur H_ : {refus:?}"
        );
    }

    /// Scenario « le contexte de la session est immuable » : le tour qui sauvegarde la ligne
    /// laisse `context` à `c1` alors que `state` et `updated_at` bougent, et `resume` rend `c1`.
    #[tokio::test(flavor = "multi_thread")]
    async fn le_contexte_de_la_session_est_immuable() {
        let db = test_db().await;
        setup_work(&db).await;
        let provider = Arc::new(ScriptedProvider::new(vec![Ok(final_text("reponse"))]));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            None,
            Limits::default(),
        ));
        let handle = manager.open("alice", "c1", None).await.expect("ouverture");
        assert_eq!(
            context_column(&db, "alice", &handle.id).await.as_deref(),
            Some("c1"),
            "la colonne context est posee a l'ouverture"
        );
        let avant = {
            let stores = LymailleStores::new(db.clone(), "alice".to_owned());
            let state = stores
                .sessions()
                .load(&handle.id)
                .await
                .expect("etat relu")
                .expect("etat present");
            state.to_json().expect("l'etat se serialise")
        };

        let mut sub = manager.subscribe("alice", &handle.id).await.expect("abonnement");
        manager
            .send("alice", &handle.id, "bonjour".to_owned(), None)
            .await
            .expect("le tour est lance");
        drain(&mut sub).await;

        assert_eq!(
            context_column(&db, "alice", &handle.id).await.as_deref(),
            Some("c1"),
            "le save du tour n'a pas touche la colonne context (DEC-28)"
        );
        let apres = {
            let stores = LymailleStores::new(db.clone(), "alice".to_owned());
            let state = stores
                .sessions()
                .load(&handle.id)
                .await
                .expect("etat relu")
                .expect("etat present");
            state.to_json().expect("l'etat se serialise")
        };
        assert_ne!(avant, apres, "l'etat a bien change sous le tour");

        let repris = manager.resume("alice", &handle.id).await.expect("reprise");
        assert_eq!(repris.context, "c1", "la reprise rend le contexte persiste");
    }

    /// Scenario « plafond de sessions ouvertes » : `max_open_per_user = 2`, la troisième d'un
    /// non admin est refusée ; l'admin y parvient.
    #[tokio::test(flavor = "multi_thread")]
    async fn plafond_de_sessions_ouvertes() {
        let db = test_db().await;
        wire_catalog(&db).await;
        let alice = compte(&db, "alice", false).await;
        put_agent(&db, &alice, &agent("dev", AgentKind::Work)).await;
        put_type(&db, "t", Some(SessionKind::Work), Some("dev"), &["dev"]).await;
        inserer_contexte_prive(&db, alice.user_id, "c1", Some("t")).await;
        let root = compte(&db, "root", true).await;
        put_agent(&db, &root, &agent("dev", AgentKind::Work)).await;
        inserer_contexte_prive(&db, root.user_id, "c1", Some("t")).await;

        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            None,
            Limits {
                max_open_per_user: 2,
                ..Limits::default()
            },
        ));
        manager.open("alice", "c1", None).await.expect("premiere session");
        manager.open("alice", "c1", None).await.expect("seconde session");
        let refus = manager
            .open("alice", "c1", None)
            .await
            .expect_err("le plafond s'applique");
        match refus {
            crate::lymaille::error::LymailleError::Forbidden(message) => {
                assert_eq!(message, "too many open sessions", "le refus nomme le plafond");
            }
            autre => panic!("un `Forbidden` est attendu, obtenu : {autre:?}"),
        }
        manager
            .open("root", "c1", None)
            .await
            .expect("l'admin est exempte du plafond");
    }

    /// Scenario « le jeton n'est jamais persisté » : ni l'état sauvegardé, ni les événements ne
    /// portent le jeton `forward` du tour (les traces n'en portent pas non plus : le jeton
    /// n'est jamais passé à `tracing`, vérifié à la lecture du chemin d'assemblage).
    #[tokio::test(flavor = "multi_thread")]
    async fn le_jeton_nest_jamais_persiste() {
        let db = test_db().await;
        setup_work(&db).await;
        let provider = Arc::new(ScriptedProvider::new(vec![Ok(final_text("reponse"))]));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            None,
            Limits::default(),
        ));
        let handle = manager.open("alice", "c1", None).await.expect("ouverture");
        let mut sub = manager.subscribe("alice", &handle.id).await.expect("abonnement");
        manager
            .send(
                "alice",
                &handle.id,
                "bonjour".to_owned(),
                Some(Zeroizing::new("JETON-NE-PAS-PERSISTER".to_owned())),
            )
            .await
            .expect("le tour est lance avec le jeton");
        let kinds = drain(&mut sub).await;

        let etat = {
            let stores = LymailleStores::new(db.clone(), "alice".to_owned());
            let state = stores
                .sessions()
                .load(&handle.id)
                .await
                .expect("etat relu")
                .expect("etat present");
            state.to_json().expect("l'etat se serialise")
        };
        assert!(
            !etat.contains("JETON-NE-PAS-PERSISTER"),
            "l'etat sauvegarde ne porte pas le jeton"
        );
        let evenements = serde_json::to_value(&kinds).expect("les kinds des evenements se serialisent");
        assert!(
            !evenements.to_string().contains("JETON-NE-PAS-PERSISTER"),
            "les diffuses ne portent pas le jeton"
        );
    }

    /// Scenario « fermer libère la session » (spec amendée 2026-11-05) : `close` pendant un tour
    /// actif — le tour est annulé, le hub est fermé, et `send` rend ensuite le même `NotFound`
    /// qu'une session absente ; l'état persisté demeure relisible par `resume`.
    #[tokio::test(flavor = "multi_thread")]
    async fn fermer_libere_la_session() {
        let db = test_db().await;
        setup_work(&db).await;
        let hang = Hang::new();
        // Le provider bloque jusqu'a liberation explicite : seul l'annulation termine le tour.
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(HangFactory(Arc::clone(&hang))),
            None,
            Limits::default(),
        ));
        let handle = manager.open("alice", "c1", None).await.expect("ouverture");
        let mut sub = manager.subscribe("alice", &handle.id).await.expect("abonnement");
        manager
            .send("alice", &handle.id, "long".to_owned(), None)
            .await
            .expect("le tour long est lance");
        wait_entered(&hang).await;

        manager.close("alice", &handle.id).await;

        // Le tour en vol est annulé : le provider bloque éternellement, seule l'annulation a pu
        // le terminer — le verrou rend un tour inactif.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while manager
            .cancel("alice", &handle.id)
            .await
            .expect("la session cloisonnee repond de son tour")
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "le tour n'a jamais ete annule"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Hub fermé : après les événements mis en file, l'abonné est rupture par `Closed`.
        loop {
            let rendu = tokio::time::timeout(Duration::from_secs(5), sub.recv())
                .await
                .expect("un evenement ou une rupture dans le delai");
            if rendu.is_err() {
                assert_eq!(
                    rendu.expect_err("le hub est ferme"),
                    SubscriptionError::Closed,
                    "la rupture est la fermeture du canal"
                );
                break;
            }
        }

        // `send` ultérieur : le même `NotFound` qu'une session absente.
        let refuse = manager
            .send("alice", &handle.id, "relance".to_owned(), None)
            .await
            .expect_err("une session fermee n'accepte plus de tour");
        let inconnu = SessionId::new("s-inconnu").expect("identifiant de test");
        let absent = manager
            .send("alice", &inconnu, "x".to_owned(), None)
            .await
            .expect_err("session absente de reference");
        assert_eq!(
            refuse.to_string(),
            absent.to_string(),
            "les refus sont indiscernables"
        );
        assert!(
            refuse.to_string().contains("MRD-LYMAILLE-010"),
            "le refus cite NotFound (MRD-LYMAILLE-010) : {refuse}"
        );

        // L'état persisté demeure : `resume` le relit et rend le contexte persisté.
        let repris = manager
            .resume("alice", &handle.id)
            .await
            .expect("l'etat persiste demeure relisible par resume");
        assert_eq!(repris.context, "c1", "la reprise relit le contexte de la ligne");
        assert_eq!(session_count(&db).await, 1, "la fermeture ne purge pas l'etat");
    }

    /// Scenario « list résume les sessions de l'appelant » (spec amendée 2026-11-05) : deux
    /// sessions d'alice (dont une dont le titre sensible est expurgé par le `Redactor` — le titre
    /// rendu est le titre d'affichage borné, jamais le contenu brut) et une de bob : la liste
    /// d'alice rend deux résumés, aucune trace de la session de bob.
    #[tokio::test(flavor = "multi_thread")]
    async fn list_resume_les_sessions_de_l_appelant() {
        let db = test_db().await;
        setup_work(&db).await;
        let bob = compte(&db, "bob", false).await;
        put_agent(&db, &bob, &agent("dev", AgentKind::Work)).await;
        inserer_contexte_prive(&db, bob.user_id, "c1", Some("t")).await;

        let provider = Arc::new(ScriptedProvider::new(vec![
            Ok(final_text("reponse alice")),
            Ok(final_text("reponse bob")),
        ]));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(Arc::clone(&provider))),
            None,
            Limits::default(),
        ));
        let s1 = manager
            .open("alice", "c1", None)
            .await
            .expect("session 1 d'alice");
        let s2 = manager
            .open("alice", "c1", None)
            .await
            .expect("session 2 d'alice");
        let s3 = manager.open("bob", "c1", None).await.expect("session de bob");

        // Titre sensible d'alice : un premier message dont la queue dépasse la borne de
        // l'extrait ; celui de bob ne doit jamais traverser la liste d'alice.
        let mut sub = manager.subscribe("alice", &s1.id).await.expect("abonnement");
        manager
            .send(
                "alice",
                &s1.id,
                format!("MARQUEUR-SENSIBLE-{}-FIN-DE-LIGNE-SECRET", "x".repeat(80)),
                None,
            )
            .await
            .expect("le tour d'alice est lance");
        drain(&mut sub).await;
        let mut sub_bob = manager.subscribe("bob", &s3.id).await.expect("abonnement bob");
        manager
            .send("bob", &s3.id, "SECRET-DE-BOB-A-NE-JAMAIS-VOIR".to_owned(), None)
            .await
            .expect("le tour de bob est lance");
        drain(&mut sub_bob).await;

        let resumes = manager
            .list("alice", &SessionFilter::new())
            .await
            .expect("la liste d'alice reussit");
        assert_eq!(resumes.len(), 2, "deux resumés, ni plus ni moins : {resumes:?}");
        let mut rendus: Vec<&str> = resumes.iter().map(|resume| resume.id().as_str()).collect();
        rendus.sort_unstable();
        let mut attendus = vec![s1.id.as_str(), s2.id.as_str()];
        attendus.sort_unstable();
        assert_eq!(rendus, attendus, "les deux sessions d'alice, aucune de bob");

        for resume in &resumes {
            let etat = LymailleStores::new(db.clone(), "alice".to_owned())
                .sessions()
                .load(resume.id())
                .await
                .expect("etat relu")
                .expect("etat present");
            assert_eq!(
                resume.title(),
                etat.display_title(&Redactor::new()),
                "le titre du résumé est le titre d'affichage masqué par le `Redactor`"
            );
            assert_eq!(resume.agent(), "dev", "l'agent est celui de l'etat");
            assert_eq!(resume.kind(), SessionKind::Work, "la nature vient du type");
        }
        let titre_s1 = resumes
            .iter()
            .find(|resume| resume.id() == &s1.id)
            .expect("le résumé de la session au titre sensible")
            .title()
            .to_owned();
        assert!(
            titre_s1.contains("MARQUEUR-SENSIBLE"),
            "le titre d'affichage porte l'extrait de la premiere emission : {titre_s1}"
        );
        assert!(
            !titre_s1.contains("FIN-DE-LIGNE-SECRET"),
            "l'extrait est borné : la queue du titre sensible n'est pas affichee : {titre_s1}"
        );
        let rendu = format!("{resumes:?}");
        assert!(
            !rendu.contains("SECRET-DE-BOB") && !rendu.contains(s3.id.as_str()),
            "aucune trace de la session de bob : {rendu}"
        );
    }
}
