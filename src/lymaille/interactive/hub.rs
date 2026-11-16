//! Diffusion des événements d'une session vers ses abonnés (`src/lymaille/interactive/hub.sdd`).
//!
//! Le `EventSink` des sessions interactives (DSD `FR-11`, `DEC-24`, amendé `DEC-30`) : `lymaille`
//! émet ses événements vers [`EventHub::emit`](::lymaille::EventSink::emit), synchrone, qui ne bloque pas et ne peut pas
//! échouer. Le hub convertit chaque événement amont en [`SessionEvent`] typé (./event.sdd) et le
//! **diffuse** à zéro, un ou plusieurs abonnés de la **même** session (REST+SSE, Matrix,
//! plusieurs onglets) par `tokio::sync::broadcast` ; un abonné lent est décroché proprement
//! (`Lagged`) sans ralentir le tour. Les erreurs de script partent d'abord à l'
//! [`AuditWriter`](crate::lymaille::store::AuditWriter), **depuis l'événement amont complet**
//! (../store/audit.sdd). [`EventHub::publish_host`] diffuse les événements de l'hôte sans en
//! interpréter le contenu.
//!
//! Les événements portent déjà des noms, compteurs et enregistrements masqués (jamais un secret,
//! `lymaille` garantit le masquage) : le hub n'y ajoute aucune donnée et ne journalise jamais un
//! contenu d'événement ni un contenu d'hôte.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::sync::mpsc;

use ::lymaille::Event;
use ::lymaille::EventKind;
use ::lymaille::EventSink;
use ::lymaille::SessionId;

use super::event::SessionEvent;
use crate::lymaille::error::LymailleError;
use crate::lymaille::store::SeaEventLog;
use crate::lymaille::store::audit::AuditWriter;

/// Longueur maximale, en octets, de la forme sérialisée du `payload` d'un événement d'hôte
/// (`publish_host`, `Must` de ./hub.sdd).
pub const HOST_PAYLOAD_MAX: usize = 65_536;

/// Texte fixe de refus d'un événement d'hôte invalide (`publish_host`, `Must` de ./hub.sdd).
const HOST_INVALID: &str = "host event invalid";

/// Basse de capacité d'une file de session, quel que soit le `capacity` demandé.
const CAPACITY_MIN: usize = 16;

/// Haute de capacité d'une file de session, quel que soit le `capacity` demandé.
const CAPACITY_MAX: usize = 4_096;

/// Au-delà de cette taille sérialisée, un événement n'est pas relayé en entier : le relais
/// reçoit `Missed` à sa place (amendement #81 lot 2 de ./hub.sdd — `NOTIFY` PostgreSQL borne la
/// charge à 8 000 octets ; `pg_relay.sdd` garde la dernière barrière à 7 900).
pub const RELAY_EVENT_MAX: usize = 7_000;

/// Session wildcard d'un `Missed` : « des événements ont pu être manqués pour **toutes** les
/// sessions » (message poussé par `./pg_relay.rs` après la reconnexion de son écoute).
const ALL_SESSIONS: &str = "*";

/// Ce que la file d'une session transporte : un événement à diffuser avec son curseur du journal
/// (`Some` quand la tâche d'écriture l'a posé — amendement #81 lot 3 de ./hub.sdd ; `None` sans
/// journal ou après une panne d'écriture), ou la marque « des événements ont pu être manqués »
/// injectée par un `Missed` venu d'une autre réplique (amendement #81 lot 2 de ./hub.sdd).
/// `Clone` : exigé par `broadcast::Sender::send`.
#[derive(Clone)]
enum Frame {
    Event(Arc<SessionEvent>, Option<u64>),
    Lagged(u64),
}

/// File bornée de la tâche d'écriture : un événement déjà converti, avec sa session, en attente
/// de son curseur (amendement #81 lot 3 de ./hub.sdd).
struct PendingEvent {
    session: String,
    event: Arc<SessionEvent>,
}

/// Journal posé par [`EventHub::with_log`] : le store, la file bornée qui alimente la tâche
/// d'écriture unique, et la borne de rejeu (`replay_max` ramené à sa borne).
struct JournalHandle {
    log: SeaEventLog,
    queue: mpsc::Sender<PendingEvent>,
    replay_max: usize,
}

/// Rejoins d'écriture de `with_log`, ramenés aux bornes de ./hub.sdd (`Must` de l'amendement
/// lot 3) : `[10 ms, 1 s]`, `[1, 512]`, `[64, 65 536]`, `[1, 10 000]`.
#[derive(Clone, Copy, Debug)]
pub struct LogConfig {
    /// Battement de la tâche d'écriture : un lot part au plus tard après ce délai.
    pub flush_interval: Duration,
    /// Événements maximum par transaction de lot.
    pub batch_max: usize,
    /// Capacité de la file bornée entre `emit` et la tâche d'écriture.
    pub queue_capacity: usize,
    /// Lignes maximum relues par `subscribe_from`.
    pub replay_max: usize,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            flush_interval: Duration::from_millis(50),
            batch_max: 64,
            queue_capacity: 4_096,
            replay_max: 1_000,
        }
    }
}

impl LogConfig {
    /// Ramène chaque rejoint à sa borne la plus proche (`Must` de ./hub.sdd).
    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            flush_interval: self
                .flush_interval
                .clamp(Duration::from_millis(10), Duration::from_secs(1)),
            batch_max: self.batch_max.clamp(1, 512),
            queue_capacity: self.queue_capacity.clamp(64, 65_536),
            replay_max: self.replay_max.clamp(1, 10_000),
        }
    }
}

/// File d'une session : le canal `broadcast` partagé entre les abonnés de cette session.
struct Slot {
    sender: broadcast::Sender<Frame>,
}

/// État partagé de l'hub (`Clone` : tous les clones partagent les mêmes canaux).
struct Inner {
    audit: Option<AuditWriter>,
    capacity: usize,
    /// Identifiant de cette réplique : UUID v4 tiré à [`EventHub::new`], jamais un nom d'hôte.
    /// Un message dont l'`origin` est celui-ci est ignoré (aucune boucle de relais).
    origin: String,
    /// `subject` de chaque session enregistrée — lu par l'audit des erreurs de script.
    subjects: Mutex<HashMap<String, String>>,
    /// Canal de diffusion de chaque session ayant un abonné (ou attendant le premier).
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    /// Couture de relais entre répliques (`None` : rien ne change, comportement antérieur).
    relay: Mutex<Option<Arc<dyn EventRelay>>>,
    /// Couture de journal (`None` : rien ne change, comportement antérieur — amendement #81
    /// lot 3 de ./hub.sdd).
    journal: Mutex<Option<Arc<JournalHandle>>>,
    /// Dernier curseur diffusé par cette réplique pour chaque session (branche `Cursor` du
    /// relais, lot 3) — point de reprise des lectures du journal.
    cursor_since: Mutex<HashMap<String, u64>>,
    /// Remise des commandes `Cancel`/`Close` venues du relais (posée par ./manager.rs).
    on_command: Mutex<Option<CommandHandler>>,
}

/// Le gestionnaire posé par [`EventHub::on_command`] reçoit les commandes du relais ; alias du
/// type de la couture (alias : la forme directe déclenche `type_complexity`).
type CommandHandler = Arc<dyn Fn(RelayCommand) + Send + Sync>;

/// Hub de diffusion des événements de session vers leurs abonnés (`./hub.sdd`).
#[derive(Clone)]
pub struct EventHub {
    inner: Arc<Inner>,
}

impl EventHub {
    /// Construit un hub : `audit` reçoit les erreurs de script (`None` : pas d'audit), `capacity`
    /// borne la file de chaque session (borné à `[16, 4096]`, défaut conseillé 256).
    #[must_use]
    pub fn new(audit: Option<AuditWriter>, capacity: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                audit,
                capacity: capacity.clamp(CAPACITY_MIN, CAPACITY_MAX),
                origin: uuid::Uuid::new_v4().to_string(),
                subjects: Mutex::new(HashMap::new()),
                slots: Mutex::new(HashMap::new()),
                relay: Mutex::new(None),
                journal: Mutex::new(None),
                cursor_since: Mutex::new(HashMap::new()),
                on_command: Mutex::new(None),
            }),
        }
    }

    /// Associe la session `id` à son `subject` (appelé par ./manager.sdd à l'ouverture et à la
    /// reprise). Une session non enregistrée voit ses erreurs de script journalisées en `warn`
    /// et non auditées — jamais un événement perdu pour les abonnés.
    pub fn register(&self, id: &SessionId, subject: &str) {
        lock(&self.inner.subjects).insert(id.as_str().to_owned(), subject.to_owned());
    }

    /// Crée au besoin le canal de `id` et rend un abonnement à sa file courante. Le
    /// cloisonnement du `subject` est fait par ./manager.sdd **avant** l'abonnement.
    #[must_use]
    pub fn subscribe(&self, subject: &str, id: &SessionId) -> Subscription {
        lock(&self.inner.subjects)
            .entry(id.as_str().to_owned())
            .or_insert_with(|| subject.to_owned());
        Subscription {
            receiver: self.slot(id).sender.subscribe(),
            replay: VecDeque::new(),
            lag_first: None,
            cursor: None,
        }
    }

    /// Abonnement avec reprise : enregistre le canal **avant** de lire (aucun trou entre rejeu
    /// et direct), puis — avec journal et `after = Some(n)` — relit `read_after(id, n,
    /// replay_max)` et sert les lignes relues **avant** les directs (déduplication par curseur,
    /// amendement #81 lot 3 de ./hub.sdd). Sans journal, ou `after = None`, équivaut à
    /// [`subscribe`](Self::subscribe). Si des lignes ont été purgées entre `n` et la première
    /// conservée, ou si la relecture est tronquée à `replay_max`, le premier `recv` rend
    /// `Err(Lagged(n))` puis le flux continue.
    ///
    /// # Errors
    ///
    /// [`LymailleError::Store`] (`MRD-LYMAILLE-009`) : panne de lecture du journal.
    pub async fn subscribe_from(
        &self,
        subject: &str,
        id: &SessionId,
        after: Option<u64>,
    ) -> Result<Subscription, LymailleError> {
        let mut subscription = self.subscribe(subject, id);
        let handle = lock(&self.inner.journal).clone();
        if let (Some(handle), Some(after)) = (handle, after) {
            let page = handle.log.read_after(id, Some(after), handle.replay_max).await?;
            subscription.replay = page
                .events
                .iter()
                .map(|logged| (logged.seq, logged.event.clone()))
                .collect();
            if let Some(last) = page.events.last() {
                subscription.cursor = Some(last.seq);
            }
            subscription.lag_first = if page.missed > 0 {
                Some(page.missed)
            } else if page.truncated {
                Some(1)
            } else {
                None
            };
        }
        Ok(subscription)
    }

    /// Ferme le canal de `id` : les abonnés reçoivent [`SubscriptionError::Closed`]. Appelé quand
    /// le gestionnaire retire la session ; libère aussi son `subject` et son curseur de relais.
    pub fn close(&self, id: &SessionId) {
        lock(&self.inner.slots).remove(id.as_str());
        lock(&self.inner.subjects).remove(id.as_str());
        lock(&self.inner.cursor_since).remove(id.as_str());
    }

    /// Diffuse un événement d'hôte `SessionEventKind::Host { kind, payload }` aux abonnés de `id`
    /// avec l'horodatage courant, sans jamais interpréter le `payload`. Sans abonné, l'événement
    /// est abandonné comme les autres ; pas d'audit.
    ///
    /// # Errors
    ///
    /// [`LymailleError::InvalidDefinition`] (`MRD-LYMAILLE-004`, texte `host event invalid`) :
    /// `kind` vide, ou `payload` dont la forme sérialisée dépasse [`HOST_PAYLOAD_MAX`] octets —
    /// rien n'est alors diffusé.
    pub fn publish_host(
        &self,
        id: &SessionId,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<(), LymailleError> {
        let shape = serde_json::to_string(&payload)
            .map_err(|_| LymailleError::InvalidDefinition(HOST_INVALID.to_owned()))?;
        if kind.is_empty() || shape.len() > HOST_PAYLOAD_MAX {
            return Err(LymailleError::InvalidDefinition(HOST_INVALID.to_owned()));
        }
        let event = Arc::new(SessionEvent {
            session: id.as_str().to_owned(),
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            kind: super::event::SessionEventKind::Host {
                kind: kind.to_owned(),
                payload,
            },
        });
        self.inner.route(id.as_str(), &event);
        Ok(())
    }

    /// Canal de `id`, créé au besoin avec la capacité du hub et le `subject` donné.
    fn slot(&self, id: &SessionId) -> Arc<Slot> {
        let mut slots = lock(&self.inner.slots);
        slots
            .entry(id.as_str().to_owned())
            .or_insert_with(|| {
                let (sender, _) = broadcast::channel(self.inner.capacity);
                Arc::new(Slot { sender })
            })
            .clone()
    }

    /// Attache la couture de relais entre répliques (amendement #81 lot 2 de ./hub.sdd) :
    /// constructeur, à appeler **avant tout clone du hub** et dans un runtime `tokio`. Hors
    /// runtime : `tracing::warn` et le relais reste inactif (la réception ne démarre pas),
    /// jamais de panique. La tâche de lecture ignore les messages dont l'`origin` est la
    /// réplique elle-même, diffuse les événements reçus **sans audit ni re-relais**, traduit
    /// `Missed` en `Lagged(1)` local et remet `Cancel`/`Close` au gestionnaire posé par
    /// [`EventHub::on_command`].
    #[must_use]
    pub fn with_relay(self, relay: Arc<dyn EventRelay>) -> Self {
        let mut incoming = relay.incoming();
        *lock(&self.inner.relay) = Some(relay);
        if tokio::runtime::Handle::try_current().is_err() {
            tracing::warn!("lymaille hub: with_relay called outside a tokio runtime, relay stays inactive");
            return self;
        }
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            while let Some(message) = incoming.recv().await {
                inner.apply_relay_message(&message).await;
            }
            tracing::debug!("lymaille hub: relay incoming stream closed");
        });
        self
    }

    /// Attache la couture de journal (amendement #81 lot 3 de ./hub.sdd) : mêmes règles de
    /// constructeur et de runtime que [`with_relay`](Self::with_relay) (hors runtime : `warn`,
    /// journal inactif). La tâche d'écriture unique vide la file bornée par lots (`batch_max`
    /// événements ou `flush_interval`, une transaction par lot et par session), **puis diffuse
    /// dans l'ordre d'entrée**, chaque événement portant son curseur. Une panne d'écriture ou une
    /// file pleine diffusent sans curseur (`warn`), jamais en bloquant. Avec relais en plus, le
    /// relais reçoit `Cursor(seq)` à la place de l'événement.
    #[must_use]
    pub fn with_log(self, log: SeaEventLog, config: LogConfig) -> Self {
        let config = config.clamped();
        if tokio::runtime::Handle::try_current().is_err() {
            tracing::warn!("lymaille hub: with_log called outside a tokio runtime, journal stays inactive");
            return self;
        }
        let (sender, receiver) = mpsc::channel(config.queue_capacity);
        *lock(&self.inner.journal) = Some(Arc::new(JournalHandle {
            log,
            queue: sender,
            replay_max: config.replay_max,
        }));
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(run_writer(receiver, weak, config));
        self
    }

    /// Le hub porte-t-il un journal ? Le flux SSE n'a besoin que de ce drapeau : avec journal la
    /// ligne `id:` est le curseur, sinon le compteur de la connexion — les deux ne se mélangent
    /// jamais (amendement du 2026-11-14 de ../http/sse.sdd, issue #81 lot 3). Interne à la crate,
    /// hors de l'`Exposes` de ./hub.sdd.
    #[must_use]
    pub(crate) fn has_journal(&self) -> bool {
        lock(&self.inner.journal).is_some()
    }

    /// Remet les commandes `Cancel`/`Close` venues du relais au gestionnaire (appelé une fois
    /// par ./manager.rs à sa création). Une commande sans gestionnaire est abandonnée (`debug`).
    pub fn on_command(&self, handler: Arc<dyn Fn(RelayCommand) + Send + Sync>) {
        *lock(&self.inner.on_command) = Some(handler);
    }

    /// Publie `Cancel` pour `id` vers le relais (annulation demandée aux autres répliques).
    /// Sans relais : sans effet.
    pub fn request_cancel(&self, id: &SessionId) {
        self.inner.publish_command(RelayBody::Cancel, id);
    }

    /// Publie `Close` pour `id` vers le relais (fermeture demandée aux autres répliques).
    /// Sans relais : sans effet.
    pub fn request_close(&self, id: &SessionId) {
        self.inner.publish_command(RelayBody::Close, id);
    }
}

impl EventSink for EventHub {
    /// Reçoit un événement amont : erreur de script d'abord poussée à l'audit depuis l'événement
    /// complet, puis miroir typé diffusé sans attendre. Aucun abonné → l'événement est abandonné
    /// et le canal sans abonné est libéré ; `SendError` ignoré ; jamais de blocage ni de panique.
    fn emit(&self, event: Event) {
        if let (Some(audit), EventKind::Script(record)) = (&self.inner.audit, &event.kind)
            && record.error.is_some()
        {
            match lock(&self.inner.subjects).get(event.session.as_str()) {
                Some(subject) => audit.record(subject, &event),
                None => {
                    // Sans `subject`, l'audit n'a pas d'auteur à porter à la ligne : la
                    // trace ne cite que la panne, jamais un contenu d'événement.
                    tracing::warn!("lymaille hub: script error not audited, session not registered");
                }
            }
        }
        let shared = Arc::new(SessionEvent::from(&event));
        self.inner.route(event.session.as_str(), &shared);
    }
}

impl Inner {
    /// Diffuse une trame au canal de `session` : sans abonné, la trame est abandonnée et le
    /// canal libéré (`Must not` de ./hub.sdd : retenir des événements sans abonné) ; `send`
    /// n'attend jamais et `SendError` est ignoré.
    fn deliver(&self, session: &str, frame: Frame) {
        let mut slots = lock(&self.slots);
        if let Some(slot) = slots.get(session) {
            if slot.sender.receiver_count() == 0 {
                slots.remove(session);
            } else {
                let _sent = slot.sender.send(frame);
            }
        }
    }

    /// Publie un événement local vers le relais s'il y en a un : la forme sérialisée dépasse
    /// [`RELAY_EVENT_MAX`] → `Missed` à la place de l'événement (les abonnés des autres répliques
    /// verront `Lagged(1)` et reliront l'historique). Une forme infalsifiable ne se présente pas
    /// (`SessionEvent` est une valeur JSON) ; si elle se présentait, le message est abandonné.
    fn relay_event(&self, event: &Arc<SessionEvent>) {
        let Some(relay) = lock(&self.relay).clone() else {
            return;
        };
        let body = match serde_json::to_vec(&**event) {
            Ok(shape) if shape.len() > RELAY_EVENT_MAX => RelayBody::Missed,
            Ok(_shape) => RelayBody::Event((**event).clone()),
            Err(_error) => {
                tracing::warn!("lymaille hub: event not relayed (serialization failed)");
                return;
            }
        };
        relay.publish(RelayMessage {
            origin: self.origin.clone(),
            session: event.session.clone(),
            body,
        });
    }

    /// Publie une commande de session vers le relais (sans relais : sans effet).
    fn publish_command(&self, body: RelayBody, id: &SessionId) {
        let Some(relay) = lock(&self.relay).clone() else {
            return;
        };
        relay.publish(RelayMessage {
            origin: self.origin.clone(),
            session: id.as_str().to_owned(),
            body,
        });
    }

    /// Route un événement converti : avec journal, il entre dans la file bornée de la tâche
    /// d'écriture (qui persistera puis diffusera avec curseur) — file pleine, diffusion sans
    /// curseur et `warn`, jamais d'attente ; sans journal, diffusion immédiate sans curseur puis
    /// relais de l'événement (comportement lot 2).
    fn route(&self, session: &str, event: &Arc<SessionEvent>) {
        if let Some(handle) = lock(&self.journal).clone() {
            let pending = PendingEvent {
                session: session.to_owned(),
                event: Arc::clone(event),
            };
            if let Err(error) = handle.queue.try_send(pending) {
                let dropped = match error {
                    mpsc::error::TrySendError::Full(dropped) | mpsc::error::TrySendError::Closed(dropped) => {
                        dropped
                    }
                };
                tracing::warn!("lymaille hub: journal queue full, event delivered without a cursor");
                self.deliver(&dropped.session, Frame::Event(dropped.event, None));
            }
            return;
        }
        self.deliver(session, Frame::Event(Arc::clone(event), None));
        self.relay_event(event);
    }

    /// Publie `Cursor(seq)` vers le relais s'il y en a un (amendement #81 lot 3 : avec journal,
    /// le relais ne transporte plus que le curseur, jamais l'événement).
    fn relay_cursor(&self, session: &str, seq: u64) {
        let Some(relay) = lock(&self.relay).clone() else {
            return;
        };
        relay.publish(RelayMessage {
            origin: self.origin.clone(),
            session: session.to_owned(),
            body: RelayBody::Cursor(seq),
        });
    }

    /// Mémorise le dernier curseur diffusé par cette réplique pour `session` (point de reprise
    /// des lectures du journal déclenchées par le relais).
    fn note_cursor(&self, session: &str, seq: u64) {
        let mut since = lock(&self.cursor_since);
        let entry = since.entry(session.to_owned()).or_insert(0);
        *entry = (*entry).max(seq);
    }

    /// Dernier curseur diffusé pour `session` (`None` si aucun).
    fn last_cursor(&self, session: &str) -> Option<u64> {
        lock(&self.cursor_since).get(session).copied()
    }

    /// La session a-t-elle un abonné local (le message `Cursor` du relais ne déclenche aucune
    /// lecture sinon) ?
    fn has_local_subscriber(&self, session: &str) -> bool {
        let mut slots = lock(&self.slots);
        let Some(slot) = slots.get(session) else {
            return false;
        };
        if slot.sender.receiver_count() == 0 {
            slots.remove(session);
            return false;
        }
        true
    }

    /// Traite un message **venu d'une autre réplique** : le message de la réplique elle-même est
    /// ignoré (aucune boucle) ; un `Event` est diffusé localement **sans audit** (l'audit
    /// appartient à la réplique émettrice) et **sans re-relais** ; `Missed` devient `Lagged(1)`
    /// pour les abonnés locaux (wildcard `*` : toutes les sessions) ; `Cancel`/`Close` sont remis
    /// au gestionnaire ; `Cursor(seq)` (lot 3) déclenche une lecture du journal **si** la session
    /// a un abonné local — depuis le dernier curseur diffusé ici, sinon depuis `seq − 1` (une
    /// réplique neuve ne rejoue pas l'historique) — et diffuse ce qui manque.
    async fn apply_relay_message(&self, message: &RelayMessage) {
        if message.origin == self.origin {
            return;
        }
        match &message.body {
            RelayBody::Event(event) => {
                self.deliver(&message.session, Frame::Event(Arc::new(event.clone()), None));
            }
            RelayBody::Missed => {
                if message.session == ALL_SESSIONS {
                    let mut slots = lock(&self.slots);
                    slots.retain(|_session, slot| {
                        if slot.sender.receiver_count() == 0 {
                            return false;
                        }
                        let _sent = slot.sender.send(Frame::Lagged(1));
                        true
                    });
                } else {
                    self.deliver(&message.session, Frame::Lagged(1));
                }
            }
            RelayBody::Cancel => {
                let Some(id) = SessionId::new(&message.session) else {
                    tracing::warn!("lymaille hub: relay command with an unusable session identifier ignored");
                    return;
                };
                self.dispatch_command(RelayCommand::Cancel(id));
            }
            RelayBody::Close => {
                let Some(id) = SessionId::new(&message.session) else {
                    tracing::warn!("lymaille hub: relay command with an unusable session identifier ignored");
                    return;
                };
                self.dispatch_command(RelayCommand::Close(id));
            }
            RelayBody::Cursor(sequence) => {
                let Some(handle) = lock(&self.journal).clone() else {
                    tracing::debug!("lymaille hub: relay cursor ignored without a journal");
                    return;
                };
                if !self.has_local_subscriber(&message.session) {
                    tracing::debug!("lymaille hub: relay cursor without a local subscriber, no journal read");
                    return;
                }
                let Some(id) = SessionId::new(&message.session) else {
                    tracing::warn!("lymaille hub: relay cursor with an unusable session identifier ignored");
                    return;
                };
                // Reprise : depuis le dernier curseur diffusé ici, sinon juste avant le curseur
                // reçu (une réplique neuve ne relit pas l'historique de la session).
                let since = self
                    .last_cursor(&message.session)
                    .unwrap_or_else(|| sequence.saturating_sub(1));
                match handle.log.read_after(&id, Some(since), handle.replay_max).await {
                    Ok(page) => {
                        for logged in page.events {
                            if logged.seq <= since {
                                continue;
                            }
                            self.note_cursor(&message.session, logged.seq);
                            self.deliver(
                                &message.session,
                                Frame::Event(Arc::new(logged.event), Some(logged.seq)),
                            );
                        }
                    }
                    Err(_error) => {
                        tracing::warn!("lymaille hub: relay cursor journal read failed, nothing replayed");
                    }
                }
            }
        }
    }

    /// Remet une commande au gestionnaire posé par [`EventHub::on_command`] ; sans gestionnaire,
    /// la commande est abandonnée (`debug`).
    fn dispatch_command(&self, command: RelayCommand) {
        if let Some(handler) = lock(&self.on_command).as_ref().cloned() {
            handler(command);
        } else {
            tracing::debug!("lymaille hub: relay command dropped, no command handler");
        }
    }
}

/// Abonnement à la file d'une session, rendu par [`EventHub::subscribe`] ou
/// [`EventHub::subscribe_from`].
#[derive(Debug)]
pub struct Subscription {
    receiver: broadcast::Receiver<Frame>,
    /// Lignes relues du journal à servir avant le direct (`subscribe_from`, lot 3).
    replay: VecDeque<(u64, SessionEvent)>,
    /// `Lagged` à rendre au premier `recv` (lignes purgées ou rejeu tronqué, lot 3).
    lag_first: Option<u64>,
    /// Dernier curseur servi : les directs de curseur inférieur ou égal sont ignorés
    /// (déduplication entre rejeu et direct, `Must not` de ./hub.sdd).
    cursor: Option<u64>,
}

impl Subscription {
    /// Rend l'événement suivant de sa session, avec son curseur du journal (`None` sans journal
    /// ou quand la panne d'écriture l'a privé de curseur — amendement #81 lot 3 de ./hub.sdd).
    ///
    /// # Errors
    ///
    /// [`SubscriptionError::Lagged`] si la file a débordé (n événements perdus), si un `Missed`
    /// est venu du relais, ou au premier `recv` d'une reprise avec lignes purgées ou rejeu
    /// tronqué ; [`SubscriptionError::Closed`] si le canal a été fermé par [`EventHub::close`]
    /// ou retiré.
    pub async fn recv_cursor(&mut self) -> Result<(Option<u64>, SessionEvent), SubscriptionError> {
        loop {
            if let Some(missed) = self.lag_first.take() {
                return Err(SubscriptionError::Lagged(missed));
            }
            if let Some((seq, event)) = self.replay.pop_front() {
                self.cursor = Some(self.cursor.unwrap_or(0).max(seq));
                return Ok((Some(seq), event));
            }
            match self.receiver.recv().await {
                Ok(Frame::Event(shared, Some(seq))) => {
                    if self.cursor.is_some_and(|last| seq <= last) {
                        continue; // déjà servi par le rejeu : déduplication par curseur
                    }
                    self.cursor = Some(seq);
                    return Ok((Some(seq), (*shared).clone()));
                }
                Ok(Frame::Event(shared, None)) => return Ok((None, (*shared).clone())),
                Ok(Frame::Lagged(missed)) => Err(SubscriptionError::Lagged(missed))?,
                Err(broadcast::error::RecvError::Lagged(n)) => Err(SubscriptionError::Lagged(n))?,
                Err(broadcast::error::RecvError::Closed) => Err(SubscriptionError::Closed)?,
            }
        }
    }

    /// Rend l'événement suivant de sa session, curseur ignoré.
    ///
    /// # Errors
    ///
    /// [`SubscriptionError::Lagged`] si la file a débordé (n événements perdus), ou si un
    /// `Missed` est venu du relais (« des événements ont pu être manqués », 1 au minimum —
    /// amendement #81 lot 2 de ./hub.sdd) ; [`SubscriptionError::Closed`] si le canal a été
    /// fermé par [`EventHub::close`] ou retiré.
    pub async fn recv(&mut self) -> Result<SessionEvent, SubscriptionError> {
        self.recv_cursor().await.map(|(_cursor, event)| event)
    }
}

/// Rupture d'un abonnement : la file a débordé (n événements perdus) ou le canal est fermé.
/// Signal d'état du canal, pas une erreur applicative : pas de code `MRD-*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionError {
    /// n événements ont été perdus pendant que l'abonné ne lisait pas.
    Lagged(u64),
    /// Le canal est fermé ([`EventHub::close`] ou session retirée).
    Closed,
}

/// Verrou d'une table de l'hub : un verrou empoisonné est récupéré — l'état est un `HashMap`
/// simple qui ne peut pas rester à demi écrit.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Tâche d'écriture unique du journal (amendement #81 lot 3 de ./hub.sdd) : vide la file bornée
/// par lots (`batch_max` événements ou `flush_interval`), écrit une transaction par lot et par
/// session, puis **diffuse dans l'ordre d'entrée** chaque événement avec son curseur. Panne
/// d'écriture : le lot part sans curseur et `warn`, le flux continue. Le `Weak` évite de retenir
/// l'`Inner` — le hub clos arrête la tâche (le `sender` meurt avec lui).
async fn run_writer(
    mut receiver: mpsc::Receiver<PendingEvent>,
    weak: std::sync::Weak<Inner>,
    config: LogConfig,
) {
    loop {
        let Some(first) = receiver.recv().await else {
            return;
        };
        let mut batch = vec![first];
        while batch.len() < config.batch_max {
            match tokio::time::timeout(config.flush_interval, receiver.recv()).await {
                Ok(Some(next)) => batch.push(next),
                _ => break,
            }
        }
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let Some(handle) = lock(&inner.journal).clone() else {
            return;
        };
        let mut cursors: HashMap<usize, u64> = HashMap::new();
        let mut by_session: HashMap<&str, Vec<usize>> = HashMap::new();
        for (index, item) in batch.iter().enumerate() {
            by_session.entry(item.session.as_str()).or_default().push(index);
        }
        for (session, indices) in by_session {
            let events: Vec<SessionEvent> = indices
                .iter()
                .filter_map(|index| batch.get(*index))
                .map(|item| (*item.event).clone())
                .collect();
            let Some(id) = SessionId::new(session) else {
                continue; // texte inutilisable : diffusion sans curseur, rien à journaliser
            };
            match handle.log.append(&id, &events, chrono::Utc::now()).await {
                Ok(sequences) => {
                    for (index, sequence) in indices.iter().zip(sequences) {
                        cursors.insert(*index, sequence);
                    }
                }
                Err(_error) => {
                    tracing::warn!("lymaille hub: journal write failed, lot delivered without cursors");
                }
            }
        }
        for (index, item) in batch.iter().enumerate() {
            let cursor = cursors.get(&index).copied();
            if let Some(sequence) = cursor {
                inner.note_cursor(&item.session, sequence);
            }
            inner.deliver(&item.session, Frame::Event(Arc::clone(&item.event), cursor));
            if let Some(sequence) = cursor {
                inner.relay_cursor(&item.session, sequence);
            }
        }
    }
}

// ── Relais entre répliques (amendement #81 lot 2 de ./hub.sdd, DSD `DEC-31`) ──

/// Couture de relais entre répliques (implémentation partagée : `./pg_relay.rs`).
pub trait EventRelay: Send + Sync {
    /// Dépose un message vers les autres répliques : **synchrone, sans attente, infaillible**
    /// (file bornée interne ; débordement → abandon du message et `tracing::warn`, comme
    /// [`EventHub::emit`]).
    fn publish(&self, message: RelayMessage);

    /// Le flux des messages **venus des autres répliques**. Appelé une fois par
    /// [`EventHub::with_relay`] ; un relais appelé deux fois rend un récepteur déjà clos.
    fn incoming(&self) -> mpsc::Receiver<RelayMessage>;
}

/// Message de relais : la réplique d'origine, la session concernée et le corps.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RelayMessage {
    /// Identifiant de la réplique émettrice (UUID v4 tiré à [`EventHub::new`]). Un `Missed`
    /// de reconnexion (`./pg_relay.rs`) porte la chaîne vide.
    pub origin: String,
    /// Session concernée ; `"*"` pour un `Missed` global.
    pub session: String,
    /// Ce que la réplique émettrice demande de propager.
    pub body: RelayBody,
}

/// Corps d'un message de relais.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RelayBody {
    /// Un événement à diffuser localement (sans audit ni re-relais).
    Event(SessionEvent),
    /// Curseur du journal (lot 3 — sans journal, ignoré).
    Cursor(u64),
    /// Des événements ont pu être manqués pour cette session (`session = "*"` : toutes).
    Missed,
    /// Annulation demandée pour la session.
    Cancel,
    /// Fermeture demandée pour la session.
    Close,
}

/// Commande de session reçue du relais, remise au gestionnaire par `on_command`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayCommand {
    /// Un tour a été annulé à distance.
    Cancel(SessionId),
    /// La session a été fermée à distance.
    Close(SessionId),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use serde_json::json;

    use super::EventHub;
    use super::EventRelay;
    use super::HOST_PAYLOAD_MAX;
    use super::RelayBody;
    use super::RelayCommand;
    use super::RelayMessage;
    use super::Subscription;
    use super::SubscriptionError;
    use super::lock;
    use crate::lymaille::LymailleError;
    use crate::lymaille::interactive::event::SessionEvent;
    use crate::lymaille::interactive::event::SessionEventKind;
    use crate::lymaille::store::audit::AuditWriter;
    use ::lymaille::Event;
    use ::lymaille::EventKind;
    use ::lymaille::EventSink as _;
    use ::lymaille::Redactor;
    use ::lymaille::ScriptInput;
    use ::lymaille::ScriptRecord;
    use ::lymaille::SessionId;
    use ::lymaille::Timestamp;
    use ::lymaille::TurnId;
    use sea_orm::ConnectionTrait as _;
    use sea_orm::DatabaseConnection;
    use sea_orm::sea_query::Alias;
    use sea_orm::sea_query::Expr;
    use sea_orm::sea_query::ExprTrait as _;
    use sea_orm::sea_query::Query;
    use tokio::sync::mpsc;

    /// Table d'audit des erreurs de script (`../store/audit.rs`).
    const AUDIT_TABLE: &str = "miryad_lymaille_script_errors";

    /// Marqueur de texte de script qui ne doit jamais sortir vers un abonné.
    const SCRIPT_MARKER: &str = "SK-marque-du-texte-du-script";

    fn session(id: &str) -> SessionId {
        SessionId::new(id).expect("un texte non vide fait un identifiant de session")
    }

    fn event(id: &str, kind: EventKind) -> Event {
        Event::new(session(id), Timestamp::from_millis(1_730_000_000_000), kind)
    }

    fn text_delta(id: &str, turn: u64, text: &str) -> Event {
        event(
            id,
            EventKind::TextDelta {
                turn: TurnId::new(turn),
                text: text.to_owned(),
            },
        )
    }

    /// Événement `Script` dont le record porte `error` (texte masqué en amont) et le script
    /// `SCRIPT_MARKER`.
    fn script_error(id: &str, error: &str) -> Event {
        let script = format!("let cle = \"{SCRIPT_MARKER}\";");
        let input = ScriptInput::new("agent-a", &script, "42", Duration::from_millis(5)).with_error(error);
        event(
            id,
            EventKind::Script(ScriptRecord::new(&Redactor::default(), 2000, input)),
        )
    }

    /// Lit les lignes d'audit `(session_id, class)` de la base de test.
    async fn audit_rows(db: &DatabaseConnection) -> Vec<(String, String)> {
        let mut select = Query::select();
        select
            .columns([Alias::new("session_id"), Alias::new("class")])
            .from(Alias::new(AUDIT_TABLE));
        db.query_all(&select)
            .await
            .expect("relecture de la table d'audit")
            .iter()
            .map(|row| {
                (
                    row.try_get::<String>("", "session_id")
                        .expect("colonne session_id"),
                    row.try_get::<String>("", "class").expect("colonne class"),
                )
            })
            .collect()
    }

    /// Attend que le marqueur d'audit `text` ait été consommé par la tâche d'écriture (file
    /// FIFO, même motif que `../store/audit.rs`).
    async fn drained(db: &DatabaseConnection, marker: &str) {
        for _ in 0..400 {
            let mut select = Query::select();
            select
                .column(Alias::new("id"))
                .from(Alias::new(AUDIT_TABLE))
                .and_where(Expr::col(Alias::new("text")).eq(marker));
            if db.query_one(&select).await.expect("sonde de drainage").is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("la tâche d'audit n'a pas drainé le marqueur dans le délai");
    }

    /// `recv` borné : sous horloge en pause automatique, un abonné qui ne reçoit rien échoue
    /// vite au lieu de pendre — l'échec rouge est un timeout, pas une attente éternelle.
    async fn recv_of(subscriber: &mut Subscription) -> Result<SessionEvent, SubscriptionError> {
        tokio::time::timeout(Duration::from_millis(500), subscriber.recv())
            .await
            .expect("un événement ou une rupture dans le délai")
    }

    /// Scenario « deux abonnés reçoivent la même suite » : les trois événements, dans l'ordre,
    /// pour chacun.
    #[tokio::test(start_paused = true)]
    async fn deux_abonnes_recoivent_la_meme_suite() {
        let hub = EventHub::new(None, 16);
        hub.register(&session("s1"), "alice");
        let mut first = hub.subscribe("alice", &session("s1"));
        let mut second = hub.subscribe("alice", &session("s1"));
        for turn in 1..=3 {
            hub.emit(text_delta("s1", turn, &format!("t{turn}")));
        }
        for subscriber in [&mut first, &mut second] {
            for turn in 1..=3 {
                let received = recv_of(subscriber).await.expect("l'abonné reçoit");
                assert_eq!(
                    received.kind,
                    SessionEventKind::TextDelta {
                        turn,
                        text: format!("t{turn}")
                    }
                );
            }
        }
    }

    /// Scenario « les sessions ne se mélangent pas » : un événement de `s2` n'atteint pas
    /// l'abonné de `s1`.
    #[tokio::test(start_paused = true)]
    async fn les_sessions_ne_se_melangent_pas() {
        let hub = EventHub::new(None, 16);
        hub.register(&session("s1"), "alice");
        hub.register(&session("s2"), "alice");
        let mut subscriber = hub.subscribe("alice", &session("s1"));
        hub.emit(text_delta("s2", 1, "ailleurs"));
        let quiet = tokio::time::timeout(Duration::from_millis(50), subscriber.recv()).await;
        assert!(
            quiet.is_err(),
            "rien ne doit parvenir à l'abonné d'une autre session"
        );
    }

    /// Scenario « un abonné lent est décroché sans bloquer » : capacité 16, 100 émissions
    /// synchrones sans attente, `recv` rend `Lagged(n)` avec n ≥ 84.
    #[tokio::test(start_paused = true)]
    async fn un_abonne_lent_est_decroche_sans_bloquer() {
        let hub = EventHub::new(None, 16);
        hub.register(&session("s1"), "alice");
        let mut subscriber = hub.subscribe("alice", &session("s1"));
        let started = std::time::Instant::now();
        for turn in 1..=100 {
            hub.emit(text_delta("s1", turn, "x"));
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "`emit` ne bloque pas : {:?}",
            started.elapsed()
        );
        let first = recv_of(&mut subscriber)
            .await
            .expect_err("l'abonné en retard est décroché proprement");
        match first {
            SubscriptionError::Lagged(lost) => {
                assert!(lost >= 84, "{lost} événements perdus sur 100 émis, file de 16");
            }
            SubscriptionError::Closed => panic!("attendu Lagged, obtenu Closed"),
        }
    }

    /// Scenario « sans abonné, rien ne s'accumule » : 1000 événements d'une session sans abonné
    /// ne retiennent aucun canal.
    #[tokio::test]
    async fn sans_abonne_rien_ne_saccumule() {
        let hub = EventHub::new(None, 16);
        hub.register(&session("s1"), "alice");
        for turn in 0..1000 {
            hub.emit(text_delta("s1", turn, "x"));
        }
        let retained = lock(&hub.inner.slots).len();
        assert_eq!(retained, 0, "aucun canal sans abonné n'est retenu");
    }

    /// Scenario « les erreurs de script partent à l'audit » : une ligne, et une seule, pour la
    /// session enregistrée ; la session non enregistrée n'est pas auditée.
    #[tokio::test]
    async fn les_erreurs_de_script_partent_a_la_audit() {
        let db = crate::lymaille::store::test_support::test_db().await;
        let audit = AuditWriter::spawn(db.clone());
        let hub = EventHub::new(Some(audit), 16);
        hub.register(&session("s1"), "alice");
        hub.register(&session("s-marqueur"), "alice");
        hub.emit(script_error("s1", "runtime error: boom"));
        hub.emit(script_error("sans-registre", "runtime error: ignoree"));
        hub.emit(script_error("s-marqueur", "script cancelled"));
        drained(&db, "script cancelled").await;

        let rows = audit_rows(&db).await;
        assert_eq!(
            rows.iter().filter(|(session_id, _)| session_id == "s1").count(),
            1,
            "`record` est appelé une fois pour s1 : {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|(session_id, class)| session_id == "s1" && class == "runtime"),
            "la ligne de s1 porte la classe runtime : {rows:?}"
        );
        assert!(
            !rows.iter().any(|(session_id, _)| session_id == "sans-registre"),
            "une session non enregistrée n'est pas auditée : {rows:?}"
        );
    }

    /// Scenario `close` : l'abonné reçoit `Closed`.
    #[tokio::test]
    async fn close_ferme_le_canal() {
        let hub = EventHub::new(None, 16);
        hub.register(&session("s1"), "alice");
        let mut subscriber = hub.subscribe("alice", &session("s1"));
        hub.close(&session("s1"));
        assert_eq!(
            subscriber.recv().await.expect_err("le canal est fermé"),
            SubscriptionError::Closed
        );
    }

    /// Scenario « les abonnés reçoivent des `SessionEvent` typés » : `recv` rend exactement
    /// `SessionEvent::from(&event)`.
    #[tokio::test(start_paused = true)]
    async fn les_abonnes_recoivent_des_sessionevent_types() {
        let hub = EventHub::new(None, 16);
        hub.register(&session("s1"), "alice");
        let mut subscriber = hub.subscribe("alice", &session("s1"));
        let upstream = event("s1", EventKind::TurnStarted { turn: TurnId::new(1) });
        hub.emit(upstream.clone());
        let received = recv_of(&mut subscriber).await.expect("l'abonné reçoit");
        assert_eq!(received, SessionEvent::from(&upstream));
        assert!(matches!(received.kind, SessionEventKind::TurnStarted { turn: 1 }));
    }

    /// Scenario « `publish_host` diffuse sans interpréter » : l'abonné de `s1` reçoit le `Host`
    /// identique, l'abonné de `s2` ne reçoit rien.
    #[tokio::test(start_paused = true)]
    async fn publish_host_diffuse_sans_interpreter() {
        let hub = EventHub::new(None, 16);
        hub.register(&session("s1"), "alice");
        hub.register(&session("s2"), "alice");
        let mut on_s1 = hub.subscribe("alice", &session("s1"));
        let mut on_s2 = hub.subscribe("alice", &session("s2"));
        hub.publish_host(&session("s1"), "dashboard.show", json!({"w": "chart"}))
            .expect("la publication est valide");
        let received = recv_of(&mut on_s1).await.expect("l'abonné de s1 reçoit");
        assert_eq!(
            received.kind,
            SessionEventKind::Host {
                kind: "dashboard.show".to_owned(),
                payload: json!({"w": "chart"}),
            }
        );
        let quiet = tokio::time::timeout(Duration::from_millis(50), on_s2.recv()).await;
        assert!(quiet.is_err(), "l'abonné de s2 ne reçoit rien");
    }

    /// Scenario « un événement Host invalide est refusé » : `kind` vide puis `payload` trop
    /// grand, deux `InvalidDefinition`, rien reçu.
    #[tokio::test(start_paused = true)]
    async fn un_evenement_host_invalide_est_refuse() {
        let hub = EventHub::new(None, 16);
        hub.register(&session("s1"), "alice");
        let mut subscriber = hub.subscribe("alice", &session("s1"));
        let empty_kind = hub.publish_host(&session("s1"), "", json!({}));
        assert!(
            matches!(
                empty_kind,
                Err(LymailleError::InvalidDefinition(ref text)) if text == "host event invalid"
            ),
            "`kind` vide est refusé : {empty_kind:?}"
        );
        let big = "x".repeat(HOST_PAYLOAD_MAX);
        let oversized = hub.publish_host(&session("s1"), "dashboard.show", json!({"data": big}));
        assert!(
            matches!(
                oversized,
                Err(LymailleError::InvalidDefinition(ref text)) if text == "host event invalid"
            ),
            "un payload au-delà de {HOST_PAYLOAD_MAX} octets est refusé : {:?}",
            oversized.as_ref().err().map(ToString::to_string)
        );
        let quiet = tokio::time::timeout(Duration::from_millis(50), subscriber.recv()).await;
        assert!(quiet.is_err(), "rien n'est diffusé");
    }

    /// Scenario « l'audit lit l'événement amont complet » : la ligne d'audit pose l'erreur du
    /// record complet (la signature texte de `AuditWriter::record` est `&Event` : c'est
    /// l'événement amont, texte de script compris, qui lui est passé — le type l'impose), alors
    /// que l'abonné ne reçoit que le résumé sans texte. Horloge réelle : la base de test est
    /// connectée, et l'auto-avance d'une horloge en pause en ferait expirer le pool de
    /// connexions.
    #[tokio::test]
    async fn laudit_lit_levenement_amont_complet() {
        let db = crate::lymaille::store::test_support::test_db().await;
        let audit = AuditWriter::spawn(db.clone());
        let hub = EventHub::new(Some(audit), 16);
        hub.register(&session("s1"), "alice");
        hub.register(&session("s-marqueur"), "alice");
        let mut subscriber = hub.subscribe("alice", &session("s1"));
        hub.emit(script_error("s1", "runtime error: boom"));
        let received = recv_of(&mut subscriber).await.expect("l'abonné reçoit");
        let text = serde_json::to_string(&received).expect("le résumé sérialise");
        assert!(
            matches!(received.kind, SessionEventKind::Script { .. }),
            "l'abonné reçoit le résumé Script : {:?}",
            received.kind
        );
        assert!(
            !text.contains(SCRIPT_MARKER),
            "le résumé diffusé ne porte pas le texte du script : {text}"
        );
        hub.emit(script_error("s-marqueur", "script cancelled"));
        drained(&db, "script cancelled").await;
        let rows = audit_rows(&db).await;
        assert!(
            rows.iter()
                .any(|(session_id, class)| session_id == "s1" && class == "runtime"),
            "l'audit a reçu l'erreur du record complet de s1 : {rows:?}"
        );
    }

    // ── Relais entre répliques (issue #81, lot 2) ────────────────────────────

    /// Récepteur déjà clos : rendu par un second `incoming()` (jamais de panique).
    fn closed_receiver() -> mpsc::Receiver<RelayMessage> {
        let (sender, receiver) = mpsc::channel(1);
        drop(sender);
        receiver
    }

    /// Bus de test partagé : chaque `publish` est livré à **tous** les relais inscrits, y
    /// compris au relais émetteur lui-même (sémantique `NOTIFY` PostgreSQL — le hub ignore sa
    /// propre `origin`).
    #[derive(Default)]
    struct Bus {
        sinks: std::sync::Mutex<Vec<mpsc::Sender<RelayMessage>>>,
    }

    /// Relais branché sur le bus : file bornée de `capacity`, compteur des `publish` émis par
    /// cette réplique (témoin du « pas de re-relais »).
    struct BusRelay {
        bus: Arc<Bus>,
        sink: std::sync::Mutex<Option<mpsc::Receiver<RelayMessage>>>,
        published: AtomicUsize,
    }

    impl Bus {
        fn new() -> Arc<Self> {
            Arc::default()
        }

        fn relay(self: &Arc<Self>, capacity: usize) -> Arc<BusRelay> {
            let (sender, receiver) = mpsc::channel(capacity.max(1));
            lock(&self.sinks).push(sender);
            Arc::new(BusRelay {
                bus: Arc::clone(self),
                sink: std::sync::Mutex::new(Some(receiver)),
                published: AtomicUsize::new(0),
            })
        }
    }

    impl EventRelay for BusRelay {
        fn publish(&self, message: RelayMessage) {
            self.published.fetch_add(1, Ordering::SeqCst);
            for sink in lock(&self.bus.sinks).iter() {
                if sink.try_send(message.clone()).is_err() {
                    tracing::warn!("relais de test: file pleine, message abandonne");
                }
            }
        }

        fn incoming(&self) -> mpsc::Receiver<RelayMessage> {
            lock(&self.sink).take().unwrap_or_else(closed_receiver)
        }
    }

    /// Relais à la file pleine et jamais consommée (`Scenario` « le relais ne bloque jamais
    /// l'émission ») : chaque `publish` déborde, `try_send` rend l'erreur et le message part.
    struct StuckRelay {
        sender: mpsc::Sender<RelayMessage>,
        _unconsumed: mpsc::Receiver<RelayMessage>,
    }

    impl EventRelay for StuckRelay {
        fn publish(&self, message: RelayMessage) {
            if self.sender.try_send(message).is_err() {
                tracing::warn!("relais de test: file pleine, message abandonne");
            }
        }

        fn incoming(&self) -> mpsc::Receiver<RelayMessage> {
            closed_receiver()
        }
    }

    fn stuck_relay() -> Arc<StuckRelay> {
        let (sender, receiver) = mpsc::channel(16);
        let message = RelayMessage {
            origin: "plein".to_owned(),
            session: "s1".to_owned(),
            body: RelayBody::Missed,
        };
        let unconsumed = receiver;
        for _ in 0..16 {
            let _sent = sender.clone().try_send(message.clone());
        }
        Arc::new(StuckRelay {
            sender,
            _unconsumed: unconsumed,
        })
    }

    /// Attend (500 ms max) que `want` soit vrai — la livraison du relais est asynchrone.
    async fn wait_until(want: impl Fn() -> bool) {
        for _ in 0..200 {
            if want() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("le delai d'attente du relais est depasse");
    }

    /// Scenario « un événement émis sur une réplique arrive aux abonnés de l'autre » : `A` et
    /// `B` reliés par un relais de test, un abonné de `s1` sur chacun — les trois événements
    /// arrivent en ordre aux deux ; l'audit d'un `Script` en erreur n'est appelé que sur `A`.
    #[tokio::test]
    async fn un_evenement_emis_sur_une_replique_arrive_aux_abonnes_de_autre() {
        let db = crate::lymaille::store::test_support::test_db().await;
        let audit = AuditWriter::spawn(db.clone());
        let bus = Bus::new();
        let hub_a = EventHub::new(Some(audit), 256).with_relay(bus.relay(256));
        let hub_b = EventHub::new(None, 256).with_relay(bus.relay(256));
        for hub in [&hub_a, &hub_b] {
            hub.register(&session("s1"), "alice");
        }
        let mut on_a = hub_a.subscribe("alice", &session("s1"));
        let mut on_b = hub_b.subscribe("alice", &session("s1"));
        hub_a.emit(text_delta("s1", 1, "un"));
        hub_a.emit(text_delta("s1", 2, "deux"));
        hub_a.emit(script_error("s1", "script cancelled"));

        for subscriber in [&mut on_a, &mut on_b] {
            for (turn, text) in [(1, "un"), (2, "deux")] {
                let received = recv_of(subscriber).await.expect("l'abonné reçoit");
                assert_eq!(
                    received.kind,
                    SessionEventKind::TextDelta {
                        turn,
                        text: text.to_owned()
                    }
                );
            }
            let received = recv_of(subscriber).await.expect("l'abonné reçoit");
            assert!(
                matches!(received.kind, SessionEventKind::Script { .. }),
                "le Script arrive en troisieme : {:?}",
                received.kind
            );
        }
        drained(&db, "script cancelled").await;
        let rows = audit_rows(&db).await;
        assert_eq!(
            rows.iter().filter(|(session_id, _)| session_id == "s1").count(),
            1,
            "l'audit n'est appelé que sur A : {rows:?}"
        );
    }

    /// Scenario « un événement relayé ne repart pas » : `B` ne publie **rien** de plus sur le
    /// bus en recevant ; un message dont l'`origin` est celle du hub lui-même est ignoré —
    /// l'abonné de `A` ne reçoit qu'une seule copie de l'événement qu'il a émis.
    #[tokio::test(start_paused = true)]
    async fn un_evenement_relaye_ne_repart_pas() {
        let bus = Bus::new();
        let relay_a = bus.relay(256);
        let relay_b = bus.relay(256);
        let hub_a = EventHub::new(None, 256).with_relay(relay_a as Arc<dyn EventRelay>);
        let hub_b = EventHub::new(None, 256).with_relay(Arc::clone(&relay_b) as Arc<dyn EventRelay>);
        for hub in [&hub_a, &hub_b] {
            hub.register(&session("s1"), "alice");
        }
        let mut on_a = hub_a.subscribe("alice", &session("s1"));
        let mut on_b = hub_b.subscribe("alice", &session("s1"));
        hub_a.emit(text_delta("s1", 1, "x"));
        let first = recv_of(&mut on_a).await.expect("A diffuse localement");
        assert!(matches!(first.kind, SessionEventKind::TextDelta { .. }));
        let second = recv_of(&mut on_b).await.expect("B reçoit par le relais");
        assert!(matches!(second.kind, SessionEventKind::TextDelta { .. }));
        let quiet_a = tokio::time::timeout(Duration::from_millis(50), on_a.recv()).await;
        assert!(
            quiet_a.is_err(),
            "l'écho de sa propre origin est ignoré : A ne reçoit pas deux copies"
        );
        let quiet_b = tokio::time::timeout(Duration::from_millis(50), on_b.recv()).await;
        assert!(quiet_b.is_err(), "rien de plus n'arrive sur B");
        assert_eq!(
            relay_b.published.load(Ordering::SeqCst),
            0,
            "B ne re-relaye jamais ce qu'il reçoit"
        );
    }

    /// Scenario « un événement trop gros devient Missed » : `publish_host` de 20 000 octets sur
    /// `A` — l'abonné de `A` reçoit l'événement entier, celui de `B` reçoit `Lagged(1)` puis le
    /// flux continue.
    #[tokio::test(start_paused = true)]
    async fn un_evenement_trop_gros_devient_missed() {
        let bus = Bus::new();
        let hub_a = EventHub::new(None, 256).with_relay(bus.relay(256));
        let hub_b = EventHub::new(None, 256).with_relay(bus.relay(256));
        for hub in [&hub_a, &hub_b] {
            hub.register(&session("s1"), "alice");
        }
        let mut on_a = hub_a.subscribe("alice", &session("s1"));
        let mut on_b = hub_b.subscribe("alice", &session("s1"));
        let big = "x".repeat(20_000);
        hub_a
            .publish_host(&session("s1"), "gros", json!({"data": big}))
            .expect("valide : sous HOST_PAYLOAD_MAX");
        let received = recv_of(&mut on_a).await.expect("A reçoit l'entier");
        assert!(
            matches!(&received.kind, SessionEventKind::Host { kind, payload } if kind == "gros"
                && payload.get("data").is_some_and(serde_json::Value::is_string)),
            "l'abonné local reçoit l'événement entier : {:?}",
            received.kind
        );
        let lagged = recv_of(&mut on_b).await.expect_err("B reçoit Lagged(1)");
        assert_eq!(lagged, SubscriptionError::Lagged(1));
        hub_a
            .publish_host(&session("s1"), "petit", json!({"v": 1}))
            .expect("valide");
        let continued = recv_of(&mut on_b).await.expect("le flux continue");
        assert!(
            matches!(&continued.kind, SessionEventKind::Host { kind, .. } if kind == "petit"),
            "après le Lagged, le flux reprend : {:?}",
            continued.kind
        );
    }

    /// Scenario « le relais ne bloque jamais l'émission » : file de relais pleine et jamais
    /// consommée, 10 000 émissions — `emit` ne bloque pas, les abonnés locaux reçoivent tout
    /// aux limites de leur capacité (au moins une rupture `Lagged`, jamais `Closed`).
    #[tokio::test(start_paused = true)]
    async fn le_relais_ne_bloque_jamais_lemission() {
        let hub = EventHub::new(None, 4_096).with_relay(stuck_relay());
        hub.register(&session("s1"), "alice");
        let mut subscriber = hub.subscribe("alice", &session("s1"));
        let started = std::time::Instant::now();
        for turn in 0..10_000 {
            hub.emit(text_delta("s1", turn, "x"));
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "`emit` ne bloque jamais : {:?}",
            started.elapsed()
        );
        let mut received = 0_usize;
        let mut lagged = 0_usize;
        loop {
            match tokio::time::timeout(Duration::from_millis(50), subscriber.recv()).await {
                Ok(Ok(_event)) => received += 1,
                Ok(Err(SubscriptionError::Lagged(_))) => lagged += 1,
                Ok(Err(SubscriptionError::Closed)) => panic!("le canal ne doit jamais être fermé"),
                Err(_quiet) => break,
            }
        }
        assert!(
            received + lagged >= 4_096,
            "le local reçoit aux limites de sa capacité : {received} reçus, {lagged} ruptures"
        );
        assert!(lagged >= 1, "au-delà de la capacité, une rupture est signalée");
    }

    /// Scenario « close et cancel traversent le relais » : un gestionnaire simulé sur `B`
    /// (`on_command`) reçoit `Cancel(s1)` puis `Close(s1)` ; `A` ne reçoit pas ses propres
    /// commandes.
    #[tokio::test(start_paused = true)]
    async fn close_et_cancel_traversent_le_relais() {
        let bus = Bus::new();
        let hub_a = EventHub::new(None, 256).with_relay(bus.relay(256));
        let hub_b = EventHub::new(None, 256).with_relay(bus.relay(256));
        let recut_a: Arc<std::sync::Mutex<Vec<RelayCommand>>> = Arc::default();
        let recut_b: Arc<std::sync::Mutex<Vec<RelayCommand>>> = Arc::default();
        let sink_a = Arc::clone(&recut_a);
        hub_a.on_command(Arc::new(move |command| lock(&sink_a).push(command)));
        let sink_b = Arc::clone(&recut_b);
        hub_b.on_command(Arc::new(move |command| lock(&sink_b).push(command)));
        hub_a.register(&session("s1"), "alice");

        hub_a.request_cancel(&session("s1"));
        hub_a.request_close(&session("s1"));
        let seen_b = Arc::clone(&recut_b);
        wait_until(move || lock(&seen_b).len() >= 2).await;
        assert_eq!(
            *lock(&recut_b),
            vec![
                RelayCommand::Cancel(session("s1")),
                RelayCommand::Close(session("s1")),
            ],
            "B reçoit Cancel puis Close, dans l'ordre"
        );
        assert!(
            lock(&recut_a).is_empty(),
            "A ne reçoit pas ses propres commandes : {:?}",
            lock(&recut_a)
        );
    }

    /// `with_relay` hors runtime (Must de l'amendement lot 2) : `warn` et relais inactif,
    /// jamais de panique — `publish` et `emit` restent utilisables.
    #[test]
    fn with_relay_hors_runtime_ne_panique_pas() {
        let bus = Bus::new();
        let hub = EventHub::new(None, 16).with_relay(bus.relay(16));
        hub.register(&session("s1"), "alice");
        hub.request_cancel(&session("s1"));
        hub.emit(text_delta("s1", 1, "x"));
    }

    // ── Journal et curseurs (issue #81, lot 3) ───────────────────────────────

    use super::LogConfig;
    use crate::lymaille::store::SeaEventLog;

    /// `recv_cursor` borné, même motif que [`recv_of`].
    async fn recv_cursor_of(
        subscriber: &mut Subscription,
    ) -> Result<(Option<u64>, SessionEvent), SubscriptionError> {
        tokio::time::timeout(Duration::from_millis(2_000), subscriber.recv_cursor())
            .await
            .expect("un événement ou une rupture dans le délai")
    }

    /// Le `SessionEvent` typé correspondant à un `text_delta` de fixture.
    fn typed(id: &str, turn: u64) -> SessionEvent {
        SessionEvent::from(&text_delta(id, turn, "x"))
    }

    /// Config de journal des tests : battement ramené à 10 ms pour des tests rapides (le défaut
    /// de 50 ms est verrouillé par le `Default` de la spec). Horloge réelle : la base de test est
    /// connectée et l'auto-avance d'une horloge en pause en ferait expirer le pool (même
    /// leçon que `laudit_lit_levenement_amont_complet`).
    fn journal_config() -> LogConfig {
        LogConfig {
            flush_interval: Duration::from_millis(10),
            ..LogConfig::default()
        }
    }

    /// Hub avec journal SQLite mémoire migré, les deux partageant la même base.
    async fn journal_hub() -> (EventHub, SeaEventLog) {
        let db = crate::lymaille::store::test_support::test_db().await;
        let log = SeaEventLog::new(db.clone());
        let hub = EventHub::new(None, 256).with_log(SeaEventLog::new(db), journal_config());
        (hub, log)
    }

    /// `Scenario` « avec journal, chaque événement diffusé porte un curseur croissant » : trois
    /// émissions, `recv_cursor` rend 1, 2, 3 et le journal porte les trois lignes en ordre.
    #[tokio::test]
    async fn avec_journal_chaque_evenement_porte_un_curseur_croissant() {
        let (hub, log) = journal_hub().await;
        hub.register(&session("s1"), "alice");
        let mut subscriber = hub.subscribe("alice", &session("s1"));
        for turn in 1..=3 {
            hub.emit(text_delta("s1", turn, &format!("t{turn}")));
        }
        for turn in 1..=3 {
            let (cursor, _) = recv_cursor_of(&mut subscriber)
                .await
                .expect("l'abonné reçoit avec son curseur");
            assert_eq!(cursor, Some(turn), "le n-ième événement porte le curseur n");
        }
        let page = log
            .read_after(&session("s1"), None, 10)
            .await
            .expect("le journal se relit");
        assert_eq!(
            page.events.iter().map(|logged| logged.seq).collect::<Vec<u64>>(),
            vec![1, 2, 3],
            "le journal contient les trois lignes dans cet ordre"
        );
    }

    /// `Scenario` « sans relais ni journal, le hub est inchangé » : `recv_cursor` rend un
    /// curseur `None` (les trois Scenarios rejoués sont les tests existants, tous verts).
    #[tokio::test(start_paused = true)]
    async fn sans_journal_les_evenements_ne_portent_aucun_curseur() {
        let hub = EventHub::new(None, 16);
        hub.register(&session("s1"), "alice");
        let mut subscriber = hub.subscribe("alice", &session("s1"));
        hub.emit(text_delta("s1", 1, "x"));
        let (cursor, _) = recv_cursor_of(&mut subscriber).await.expect("l'abonné reçoit");
        assert_eq!(cursor, None, "sans journal, `recv_cursor` rend `None`");
    }

    /// `Scenario` « reprise après un curseur sans trou ni doublon » : journal 1 à 5, reprise
    /// depuis 3, sixième émis pendant la relecture — 4, 5, 6 une seule fois chacun.
    #[tokio::test]
    async fn reprise_apres_un_curseur_sans_trou_ni_doublon() {
        let (hub, log) = journal_hub().await;
        hub.register(&session("s1"), "alice");
        let seeded: Vec<SessionEvent> = (1..=5).map(|turn| typed("s1", turn)).collect();
        log.append(&session("s1"), &seeded, chrono::Utc::now())
            .await
            .expect("les curseurs 1 à 5 sont posés");
        let mut subscriber = hub
            .subscribe_from("alice", &session("s1"), Some(3))
            .await
            .expect("la reprise depuis le curseur 3 réussit");
        hub.emit(text_delta("s1", 6, "six"));
        for want in [4, 5] {
            let (cursor, _) = recv_cursor_of(&mut subscriber)
                .await
                .expect("le rejeu sert la ligne attendue");
            assert_eq!(cursor, Some(want), "le rejeu reprend à 4");
        }
        let (cursor, _) = recv_cursor_of(&mut subscriber)
            .await
            .expect("le direct prend le relais");
        assert_eq!(cursor, Some(6), "le sixième arrive une seule fois, en direct");
        let quiet = tokio::time::timeout(Duration::from_millis(100), subscriber.recv()).await;
        assert!(quiet.is_err(), "aucun doublon ne suit");
    }

    /// `Scenario` « reprise après une purge » : 1 à 40 purgés, reprise depuis 10 — `Lagged(30)`
    /// d'abord, puis 41 à 50.
    #[tokio::test]
    async fn reprise_apres_une_purge_annonce_les_lignes_disparues() {
        let (hub, log) = journal_hub().await;
        hub.register(&session("s1"), "alice");
        let t0 = chrono::Utc::now();
        let old: Vec<SessionEvent> = (1..=40).map(|turn| typed("s1", turn)).collect();
        log.append(&session("s1"), &old, t0)
            .await
            .expect("les curseurs 1 à 40 sont posés");
        let fresh: Vec<SessionEvent> = (41..=50).map(|turn| typed("s1", turn)).collect();
        let t2 = t0
            .checked_add_signed(chrono::Duration::hours(2))
            .expect("t0 + 2 h");
        log.append(&session("s1"), &fresh, t2)
            .await
            .expect("les curseurs 41 à 50 sont posés");
        let cutoff = t0
            .checked_add_signed(chrono::Duration::hours(1))
            .expect("t0 + 1 h");
        log.purge_before(cutoff)
            .await
            .expect("la purge des 40 premières réussit");
        let mut subscriber = hub
            .subscribe_from("alice", &session("s1"), Some(10))
            .await
            .expect("la reprise depuis le curseur 10 réussit");
        assert_eq!(
            recv_of(&mut subscriber)
                .await
                .expect_err("le premier recv annonce les lignes disparues"),
            SubscriptionError::Lagged(30),
            "les curseurs 11 à 40 ont disparu"
        );
        for want in 41..=50 {
            let (cursor, _) = recv_cursor_of(&mut subscriber)
                .await
                .expect("la relecture sert la ligne attendue");
            assert_eq!(cursor, Some(want), "41 à 50 sont relus en ordre");
        }
    }

    /// `Scenario` « reprise tronquée à `replay_max` » : `replay_max = 100`, journal de 250,
    /// reprise depuis 0 — un `Lagged` d'abord, puis 100 lignes consécutives, puis le direct.
    #[tokio::test]
    async fn reprise_tronquee_a_replay_max_annonce_puis_sert_cent_lignes() {
        let db = crate::lymaille::store::test_support::test_db().await;
        let log = SeaEventLog::new(db.clone());
        let seeded: Vec<SessionEvent> = (1..=250).map(|turn| typed("s1", turn)).collect();
        log.append(&session("s1"), &seeded, chrono::Utc::now())
            .await
            .expect("deux cent cinquante lignes posées");
        let hub = EventHub::new(None, 256).with_log(
            SeaEventLog::new(db),
            LogConfig {
                replay_max: 100,
                ..journal_config()
            },
        );
        hub.register(&session("s1"), "alice");
        let mut subscriber = hub
            .subscribe_from("alice", &session("s1"), Some(0))
            .await
            .expect("la reprise depuis l'origine réussit");
        match recv_of(&mut subscriber).await {
            Err(SubscriptionError::Lagged(_)) => {}
            other => panic!("un Lagged d'abord (les plus anciens non servis) : {other:?}"),
        }
        for want in 1..=100_u64 {
            let (cursor, _) = recv_cursor_of(&mut subscriber)
                .await
                .expect("les cent lignes consécutive");
            assert_eq!(cursor, Some(want), "le rejeu borne à replay_max sert 1 à 100");
        }
        hub.emit(text_delta("s1", 251, "direct"));
        let (cursor, _) = recv_cursor_of(&mut subscriber)
            .await
            .expect("le direct prend le relais");
        assert_eq!(
            cursor,
            Some(251),
            "après le rejeu tronqué, le flux direct reprend"
        );
    }

    /// `Scenario` « une panne d'écriture n'interrompt pas le flux » : journal dont `append`
    /// échoue (table retirée) — deux événements reçus avec curseur `None`, `emit` jamais
    /// bloquant.
    #[tokio::test]
    async fn une_panne_decriture_n_interrompt_pas_le_flux() {
        let db = crate::lymaille::store::test_support::test_db().await;
        db.execute_unprepared("DROP TABLE miryad_lymaille_session_events")
            .await
            .expect("la table du journal est retirée : tout append échouera");
        let hub = EventHub::new(None, 256).with_log(SeaEventLog::new(db), journal_config());
        hub.register(&session("s1"), "alice");
        let mut subscriber = hub.subscribe("alice", &session("s1"));
        let started = std::time::Instant::now();
        hub.emit(text_delta("s1", 1, "x"));
        hub.emit(text_delta("s1", 2, "y"));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "`emit` n'attend jamais le journal : {:?}",
            started.elapsed()
        );
        for _ in 1..=2 {
            let (cursor, _) = recv_cursor_of(&mut subscriber)
                .await
                .expect("l'abonné reçoit quand même");
            assert_eq!(cursor, None, "faute de curseur journalisé, `None`");
        }
    }

    /// `Scenario` « un curseur arrive par le relais et la réplique lit le journal » : deux hubs
    /// sur le même journal, abonné de `s1` sur `B` — `B` lit et diffuse avec les curseurs ; une
    /// session sans abonné sur `B` ne déclenche aucune lecture.
    #[tokio::test]
    async fn un_curseur_arrive_par_le_relais_et_la_replique_lit_le_journal() {
        let db = crate::lymaille::store::test_support::test_db().await;
        let bus = Bus::new();
        let hub_a = EventHub::new(None, 256)
            .with_log(SeaEventLog::new(db.clone()), journal_config())
            .with_relay(bus.relay(256));
        let hub_b = EventHub::new(None, 256)
            .with_log(SeaEventLog::new(db), journal_config())
            .with_relay(bus.relay(256));
        hub_a.register(&session("s1"), "alice");
        let mut on_b = hub_b.subscribe("alice", &session("s1"));
        hub_a.emit(text_delta("s1", 1, "un"));
        hub_a.emit(text_delta("s1", 2, "deux"));
        for want in 1..=2 {
            let (cursor, _) = recv_cursor_of(&mut on_b)
                .await
                .expect("B lit le journal sur Cursor");
            assert_eq!(
                cursor,
                Some(want),
                "les deux événements arrivent avec leurs curseurs"
            );
        }
        // Une session sans abonné sur B : le Cursor reçu ne déclenche aucune lecture.
        hub_a.emit(text_delta("s2", 1, "seule"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut late = hub_b.subscribe("alice", &session("s2"));
        let quiet = tokio::time::timeout(Duration::from_millis(100), late.recv()).await;
        assert!(
            quiet.is_err(),
            "aucune lecture du journal pour une session sans abonné"
        );
    }

    /// `Scenario` « une réplique neuve ne rejoue pas l'historique » (issue #81, lot 3,
    /// correctif du 2026-11-16) : `A` émet cinq événements avant qu'un abonné de `s1` existe sur
    /// `B` ; `B` ne doit rien lire pour eux. Le sixième arrive seul, avec le curseur `Some(6)`.
    /// Horloge réelle (pas de `start_paused`) : la base de test est connectée, comme dans les
    /// autres tests du journal ; les attentes sont des `sleep` bornés.
    #[tokio::test]
    async fn une_replique_neuve_ne_rejoue_pas_lhistorique() {
        let db = crate::lymaille::store::test_support::test_db().await;
        let bus = Bus::new();
        let hub_a = EventHub::new(None, 256)
            .with_log(SeaEventLog::new(db.clone()), journal_config())
            .with_relay(bus.relay(256));
        let hub_b = EventHub::new(None, 256)
            .with_log(SeaEventLog::new(db), journal_config())
            .with_relay(bus.relay(256));
        hub_a.register(&session("s1"), "alice");
        for turn in 1..=5 {
            hub_a.emit(text_delta("s1", turn, "avant"));
        }
        // Les Cursor 1 à 5 traversent le relais sans abonné sur B : aucune lecture attendue.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut on_b = hub_b.subscribe("alice", &session("s1"));
        hub_a.emit(text_delta("s1", 6, "six"));
        let (cursor, _) = recv_cursor_of(&mut on_b)
            .await
            .expect("le sixième événement arrive sur B");
        assert_eq!(
            cursor,
            Some(6),
            "le premier événement reçu est le sixième, avec son curseur"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let quiet = tokio::time::timeout(Duration::from_millis(100), on_b.recv()).await;
        assert!(quiet.is_err(), "aucun des événements 1 à 5 ne suit");
    }

    /// `Scenario` « un hôte journalisé est rejoué » : `publish_host` puis reprise depuis 0 —
    /// l'abonné reçoit le `Host` tel que publié, avec son curseur.
    #[tokio::test]
    async fn un_hote_journalise_est_rejoue() {
        let (hub, _log) = journal_hub().await;
        hub.register(&session("s1"), "alice");
        hub.publish_host(&session("s1"), "dashboard.show", json!({"w": "chart"}))
            .expect("la publication est valide");
        let mut subscriber = hub
            .subscribe_from("alice", &session("s1"), Some(0))
            .await
            .expect("la reprise depuis l'origine réussit");
        let (cursor, event) = recv_cursor_of(&mut subscriber).await.expect("l'hôte est rejoué");
        assert_eq!(cursor, Some(1), "l'événement d'hôte porte son curseur");
        assert!(
            matches!(&event.kind, SessionEventKind::Host { kind, payload }
                if kind == "dashboard.show" && payload == &json!({"w": "chart"})),
            "l'hôte est rejoué tel que publié : {:?}",
            event.kind
        );
    }
}
