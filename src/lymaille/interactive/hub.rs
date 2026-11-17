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
    /// n'attend jamais et `SendError` est ignoré. Rend `true` si la trame a atteint un canal avec
    /// abonné (diffusion effective : seul cas où l'appelant retient son curseur).
    fn deliver(&self, session: &str, frame: Frame) -> bool {
        let mut slots = lock(&self.slots);
        let Some(slot) = slots.get(session) else {
            return false;
        };
        if slot.sender.receiver_count() == 0 {
            self.evict_without_subscribers(&mut slots, session);
            return false;
        }
        let _sent = slot.sender.send(frame);
        true
    }

    /// Marque une session dont le dernier abonné local est parti : son canal est libéré
    /// (`Must not` de ./hub.sdd : retenir des événements sans abonné) **et** son dernier curseur
    /// diffusé retiré — le prochain abonné repart de `seq − 1` sur le premier `Cursor` reçu,
    /// sans rejeu périmé (arbitrage du 2026-11-16 de ./hub.sdd, issue #81 lot 3). Le verrou de
    /// `slots` est détenu par l'appelant ; aucun chemin ne verrouille `slots` sous le verrou de
    /// `cursor_since`, l'ordre d'acquisition est sûr.
    fn evict_without_subscribers(
        &self,
        slots: &mut MutexGuard<'_, HashMap<String, Arc<Slot>>>,
        session: &str,
    ) {
        slots.remove(session);
        lock(&self.cursor_since).remove(session);
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
                    mpsc::error::TrySendError::Full(dropped) => {
                        tracing::warn!("lymaille hub: journal queue full, event delivered without a cursor");
                        dropped
                    }
                    mpsc::error::TrySendError::Closed(dropped) => {
                        tracing::warn!(
                            "lymaille hub: journal queue closed, event delivered without a cursor"
                        );
                        dropped
                    }
                };
                let _delivered = self.deliver(&dropped.session, Frame::Event(dropped.event, None));
                self.relay_missed(&dropped.session);
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

    /// Publie `Missed` pour `session` vers le relais s'il y en a un (arbitrage du 2026-11-16 de
    /// ./hub.sdd, issue #81 lot 3) : file pleine ou panne d'écriture, les autres répliques
    /// voient `Lagged(1)` et le client relit l'historique. Jamais le wildcard `"*"` : seule la
    /// session concernée est signalée.
    fn relay_missed(&self, session: &str) {
        let Some(relay) = lock(&self.relay).clone() else {
            return;
        };
        relay.publish(RelayMessage {
            origin: self.origin.clone(),
            session: session.to_owned(),
            body: RelayBody::Missed,
        });
    }

    /// Mémorise le dernier curseur **effectivement diffusé** par cette réplique pour `session`
    /// (point de reprise des lectures du journal déclenchées par le relais). N'est appelé qu'après
    /// une diffusion à un abonné local : une session sans abonné ne retient aucun curseur
    /// (arbitrage du 2026-11-16 de ./hub.sdd, issue #81 lot 3).
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
            // Pas de canal : aucun curseur ne doit subsister (course « dernier abonné parti »
            // entre la diffusion et la mémorisation du curseur), purge défensive.
            lock(&self.cursor_since).remove(session);
            return false;
        };
        if slot.sender.receiver_count() == 0 {
            self.evict_without_subscribers(&mut slots, session);
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
                    slots.retain(|session, slot| {
                        if slot.sender.receiver_count() == 0 {
                            // Le dernier abonné local est parti : même purge que partout
                            // ailleurs (canal et curseur de reprise, arbitrage du 2026-11-16).
                            lock(&self.cursor_since).remove(session);
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
                let after = Some(since);
                // Verrou C-TESTS de ./hub.sdd (2026-11-17) : le point de départ réellement
                // passé à `read_after` est observable en test — jamais depuis 0, depuis `seq − 1`.
                tracing::debug!(
                    after = ?after,
                    "lymaille hub: relay cursor replay from journal"
                );
                match handle.log.read_after(&id, after, handle.replay_max).await {
                    Ok(page) => {
                        for logged in page.events {
                            if logged.seq <= since {
                                continue;
                            }
                            let delivered = self.deliver(
                                &message.session,
                                Frame::Event(Arc::new(logged.event), Some(logged.seq)),
                            );
                            if delivered {
                                self.note_cursor(&message.session, logged.seq);
                            }
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
                    // Un `Missed` par session du lot en panne : les autres répliques voient
                    // `Lagged(1)` (arbitrage du 2026-11-16 de ./hub.sdd).
                    inner.relay_missed(session);
                }
            }
        }
        for (index, item) in batch.iter().enumerate() {
            let cursor = cursors.get(&index).copied();
            let delivered = inner.deliver(&item.session, Frame::Event(Arc::clone(&item.event), cursor));
            if let Some(sequence) = cursor {
                // Curseur retenu seulement après diffusion effective à un abonné local.
                if delivered {
                    inner.note_cursor(&item.session, sequence);
                }
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
pub(crate) mod tests {
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
    /// arrivent en ordre aux deux.
    ///
    /// Volet « l'audit d'un `Script` en erreur n'est appelé que sur `A` » (réécriture du
    /// 2026-11-16 — le test précédent donnait à `B` un `audit = None` : `B` ne pouvait rien
    /// auditer même cassé). `B` porte désormais son **propre** `AuditWriter` espion sur la même
    /// base. Le drainage de la file de `B` est prouvé par un marqueur local : il n'entre dans
    /// cette file qu'après le `record` éventuel du `Script` relayé (ce `record`, s'il avait lieu,
    /// précède ou accompagne la diffusion locale dans `apply_relay_message`, donc avant que
    /// l'abonné de `B` ne reçoive le `Script` — et le marqueur est émis après ce `recv`). La
    /// file d'audit étant FIFO, `drained` sur le marqueur garantit que toute ligne espionne de
    /// `s1` née sur `B` est déjà écrite quand on compte.
    #[tokio::test]
    async fn un_evenement_emis_sur_une_replique_arrive_aux_abonnes_de_autre() {
        let db = crate::lymaille::store::test_support::test_db().await;
        let audit_a = AuditWriter::spawn(db.clone());
        let audit_b = AuditWriter::spawn(db.clone());
        let bus = Bus::new();
        let hub_a = EventHub::new(Some(audit_a), 256).with_relay(bus.relay(256));
        let hub_b = EventHub::new(Some(audit_b), 256).with_relay(bus.relay(256));
        for hub in [&hub_a, &hub_b] {
            hub.register(&session("s1"), "alice");
        }
        hub_b.register(&session("s-marqueur"), "alice");
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
        // La file d'audit de `A` est drainée par le record du `Script` lui-même (émis sur `A`).
        drained(&db, "script cancelled").await;
        // Marqueur dans la file d'audit de `B` : son drainage prouve que tout record que `B`
        // aurait eu tort de pousser pour le `Script` relayé est déjà écrit.
        hub_b.emit(script_error("s-marqueur", "marqueur-audit-b"));
        drained(&db, "marqueur-audit-b").await;
        let rows = audit_rows(&db).await;
        assert_eq!(
            rows.iter().filter(|(session_id, _)| session_id == "s1").count(),
            1,
            "l'audit n'est appelé que sur A, jamais sur B : {rows:?}"
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
    /// aux limites de leur capacité (au moins une rupture `Lagged`, jamais `Closed`), et un
    /// `warn` sans contenu d'événement est tracé à chaque débordement (assertion du
    /// 2026-11-16 — le contrat de débordement de la couture `EventRelay` est porté ici par le
    /// relais de test, que le hub appelle sur le fil de `emit`).
    #[tokio::test(start_paused = true)]
    async fn le_relais_ne_bloque_jamais_lemission() {
        let hub = EventHub::new(None, 4_096).with_relay(stuck_relay());
        hub.register(&session("s1"), "alice");
        let mut subscriber = hub.subscribe("alice", &session("s1"));
        let (_guard, captured) = capture_traces();
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
        let lignes = captured.lines();
        let debordements: Vec<&String> = lignes
            .iter()
            .filter(|line| line.contains("file pleine"))
            .collect();
        assert!(
            !debordements.is_empty(),
            "le debordement du relais est trace en `warn` ({} lignes capturees)",
            lignes.len()
        );
        assert!(
            debordements
                .iter()
                .all(|line| !line.contains("s1") && !line.contains("text_delta")),
            "aucun `warn` de debordement ne porte de contenu d'evenement"
        );
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
    /// depuis 3, sixième émis **pendant la relecture** — 4, 5, 6 une seule fois chacun.
    ///
    /// Réécriture du 2026-11-16 : le test précédent émettait le sixième événement APRÈS le
    /// retour de `subscribe_from` — la course n'avait jamais lieu et la déduplication par
    /// curseur n'était pas exercée. Sans couture dans `subscribe_from` l'interleaving exact
    /// n'est pas branchable à l'octet près (`[?]` dans la spec) ; la construction ci-dessous le
    /// rend déterministe par l'ordre du pool SQLite (pool d'une connexion, sémaphore
    /// d'acquisition en FIFO chez sqlx/tokio) :
    /// 1. le test retient l'unique connexion du pool ;
    /// 2. le sixième événement est émis : la tâche d'écriture s'aligne sur le pool, PREMIÈRE
    ///    attendante ;
    /// 3. la reprise démarre dans une tâche dédiée : son `subscribe()` — exécuté avant toute
    ///    attente — crée le récepteur du canal `s1`, puis sa lecture s'aligne sur le pool depuis
    ///    derrière la tâche d'écriture ;
    /// 4. la connexion est rendue : le FIFO la sert d'abord à la tâche d'écriture — elle
    ///    commite la ligne 6 et diffuse la trame directe, que le récepteur déjà créé met en
    ///    tampon ; la relecture, réveillée après le relâchement, ne peut exécuter sa `SELECT`
    ///    qu'APRÈS le commit et rejeu donc 4, 5 **et 6** ;
    /// 5. `replay.len() == 3` atteste que la course a bien eu lieu ; si le pool servait les
    ///    attendants dans l'autre ordre, cette affirmation rougit au lieu de laisser un test
    ///    silencieusement non discriminant.
    /// Ensuite : la trame directe du 6 est déduppliquée par curseur, puis silence.
    #[tokio::test]
    async fn reprise_apres_un_curseur_sans_trou_ni_doublon() {
        use sea_orm::TransactionTrait as _;

        let db = crate::lymaille::store::test_support::test_db().await;
        let log = SeaEventLog::new(db.clone());
        let hub = EventHub::new(None, 256).with_log(SeaEventLog::new(db.clone()), journal_config());
        hub.register(&session("s1"), "alice");
        let seeded: Vec<SessionEvent> = (1..=5).map(|turn| typed("s1", turn)).collect();
        log.append(&session("s1"), &seeded, chrono::Utc::now())
            .await
            .expect("les curseurs 1 à 5 sont posés");
        // 1. L'unique connexion du pool est retenue : rien ne lit, rien n'écrit.
        let garde = db
            .begin()
            .await
            .expect("l'unique connexion du pool de test est retenue");
        // 2. Le sixième événement est émis ; la tâche d'écriture s'aligne sur le pool —
        //    premier attendant (battement de lot de 10 ms compris).
        hub.emit(text_delta("s1", 6, "six"));
        tokio::time::sleep(Duration::from_millis(40)).await;
        // 3. La reprise démarre dans sa tâche : le canal est créé, la lecture s'aligne à son
        //    tour sur le pool — depuis derrière la tâche d'écriture.
        let hub_reprise = hub.clone();
        let s1 = session("s1");
        let reprise = tokio::spawn(async move { hub_reprise.subscribe_from("alice", &s1, Some(3)).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        // 4. Connexion rendue : le FIFO sert la tâche d'écriture avant la relecture ; elle
        //    committe puis diffuse, la SELECT de la relecture s'exécute après le commit.
        drop(garde);
        // 5. La course est attestée, puis le rejeu et la déduplication sont vérifiés.
        let mut subscriber = reprise
            .await
            .expect("la tâche de reprise termine")
            .expect("la reprise depuis le curseur 3 reussit");
        assert_eq!(
            subscriber.replay.len(),
            3,
            "la course a bien eu lieu : la relecture voit deja le sixieme, sa trame directe \
             doit suivre en tampon"
        );
        let mut vus = Vec::new();
        for _ in 0..3 {
            let (cursor, _event) = recv_cursor_of(&mut subscriber)
                .await
                .expect("l'abonné reçoit la ligne attendue");
            vus.push(cursor);
        }
        assert_eq!(
            vus,
            vec![Some(4), Some(5), Some(6)],
            "les curseurs 4, 5, 6 en ordre, chacun une seule fois"
        );
        let quiet = tokio::time::timeout(Duration::from_millis(100), subscriber.recv()).await;
        assert!(
            quiet.is_err(),
            "la trame directe du sixième, déjà servie par le rejeu, est déduppliquée : aucun doublon ne suit"
        );
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
    /// bloquant, et un `warn` est tracé (assertion du 2026-11-16 — le `warn` part de la tâche
    /// d'écriture : runtime `current_thread` pour que la capture thread-local le voie, même
    /// motif que `panne_decriture_le_relais_publie_missed`).
    #[tokio::test(flavor = "current_thread")]
    async fn une_panne_decriture_n_interrompt_pas_le_flux() {
        let db = crate::lymaille::store::test_support::test_db().await;
        db.execute_unprepared("DROP TABLE miryad_lymaille_session_events")
            .await
            .expect("la table du journal est retirée : tout append échouera");
        let hub = EventHub::new(None, 256).with_log(SeaEventLog::new(db), journal_config());
        hub.register(&session("s1"), "alice");
        let mut subscriber = hub.subscribe("alice", &session("s1"));
        let (_guard, captured) = capture_traces();
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
        assert!(
            captured
                .lines()
                .iter()
                .any(|line| line.contains("journal write failed")),
            "la panne d'écriture est tracée en `warn`, sans contenu d'événement : {:?}",
            captured.lines()
        );
    }

    /// `Scenario` « un curseur arrive par le relais et la réplique lit le journal » : deux hubs
    /// sur le même journal, abonné de `s1` sur `B` — `B` lit et diffuse avec les curseurs.
    ///
    /// Volet « une session sans abonné sur `B` ne déclenche aucune lecture » (réécriture du
    /// 2026-11-16 — le test précédent n'était pas discriminant : l'abonné tardif créé après le
    /// `Cursor` n'aurait rien reçu même avec une lecture). Le journal est un `SeaEventLog` sur
    /// pool partagé et ni SQLite ni PostgreSQL ne peuvent armer un déclencheur sur `SELECT` :
    /// la LECTURE elle-même est témoinée par deux traces de la branche `Cursor` —
    /// - la trace `debug` « no journal read » prouve que `B` a pris la branche « sans abonné
    ///   local » (et y est resté : c'est avant toute lecture) ;
    /// - `cursor_since` de `B` pour `s2` reste vide : une lecture déclencherait `note_cursor`
    ///   sur chaque ligne relue. Un `emit` local de `B` est impossible ici (`B` n'émet rien),
    ///   la table ne peut être touchée que par le chemin `Cursor`.
    /// Le silence de l'abonné tardif est gardé en troisième verrou. Runtime `current_thread` :
    /// la capture thread-local doit voir les traces de la tâche de relais de `B`.
    #[tokio::test(flavor = "current_thread")]
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
        let (_guard, captured) = capture_traces();
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
        let sans_lecture = || {
            captured
                .lines()
                .iter()
                .any(|line| line.contains("no journal read"))
        };
        wait_until(sans_lecture).await;
        assert!(
            lock(&hub_b.inner.cursor_since).get("s2").is_none(),
            "aucune lecture du journal pour s2 : aucune ligne relue n'a été notée comme diffusée"
        );
        let mut late = hub_b.subscribe("alice", &session("s2"));
        let quiet = tokio::time::timeout(Duration::from_millis(100), late.recv()).await;
        assert!(
            quiet.is_err(),
            "l'abonné tardif de la session sans abonné ne reçoit rien rétrospectivement"
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

    /// `Scenario` « le départ du dernier abonné remet la reprise à zéro » (issue #81, lot 3,
    /// arbitré le 2026-11-16) : l'abonné de `B` qui reçoit 1 à 5 se désabonne, `A` émet 6 à 10
    /// (ignorés par `B`, sans abonné) ; le nouvel abonné simple de `s1` ne doit pas repartir du
    /// curseur périmé 5 — il reçoit **uniquement** l'événement 11 avec `Some(11)`. Horloge réelle
    /// (pas de `start_paused`) : la base de test est connectée, comme dans les autres tests du
    /// journal ; les attentes sont des `sleep` bornés.
    #[tokio::test]
    async fn le_depart_du_dernier_abonne_remet_la_reprise_a_zero() {
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
        for turn in 1..=5 {
            hub_a.emit(text_delta("s1", turn, "avant"));
        }
        for want in 1..=5 {
            let (cursor, _) = recv_cursor_of(&mut on_b)
                .await
                .expect("l'abonné de B reçoit les événements 1 à 5");
            assert_eq!(cursor, Some(want), "l'abonné de B a reçu le curseur attendu");
        }
        drop(on_b);
        for turn in 6..=10 {
            hub_a.emit(text_delta("s1", turn, "silence"));
        }
        // Les Cursor 6 à 10 traversent le relais sans abonné sur B : rien ne doit être retenu
        // pour la reprise, y compris le dernier curseur diffusé.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut fresh = hub_b.subscribe("alice", &session("s1"));
        hub_a.emit(text_delta("s1", 11, "onze"));
        let (cursor, _) = recv_cursor_of(&mut fresh)
            .await
            .expect("le onzième événement arrive sur B");
        assert_eq!(
            cursor,
            Some(11),
            "le premier événement du nouvel abonné est le onzième, avec son curseur"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let quiet = tokio::time::timeout(Duration::from_millis(100), fresh.recv()).await;
        assert!(
            quiet.is_err(),
            "aucun des événements 6 à 10 n'est livré au nouvel abonné"
        );
    }

    /// Attend que le journal porte au moins `count` lignes de `session_id` : la tâche d'écriture a
    /// alors commité le lot, et le `sleep` court laisse la diffusion qui suit l'`append` se faire.
    async fn journal_settled(log: &SeaEventLog, session_id: &str, count: usize) {
        for _ in 0..200 {
            let page = log
                .read_after(&session(session_id), None, 1_000)
                .await
                .expect("le journal se relit");
            if page.events.len() >= count {
                tokio::time::sleep(Duration::from_millis(50)).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("le journal n'a pas reçu les événements dans le délai");
    }

    /// `Scenario` « une session émettrice sans abonné ne laisse aucun curseur » (issue #81, lot 3,
    /// arbitré le 2026-11-16) : trois émissions locales sur `s1` sans aucun abonné ; une fois le
    /// journal commité, `cursor_since` ne retient rien pour `s1` (une entrée par session émettrice
    /// serait une fuite, et un rejeu périmé au prochain abonné).
    #[tokio::test]
    async fn une_session_emettrice_sans_abonne_ne_laisse_aucun_curseur() {
        let (hub, log) = journal_hub().await;
        hub.register(&session("s1"), "alice");
        for turn in 1..=3 {
            hub.emit(text_delta("s1", turn, "sans abonné"));
        }
        journal_settled(&log, "s1", 3).await;
        assert_eq!(
            hub.inner.last_cursor("s1"),
            None,
            "aucun curseur n'est retenu pour une session sans abonné local"
        );
    }

    /// `Scenario` « un abonné tardif ne rejoue pas ce que la réplique a émis sans abonné » (issue
    /// #81, lot 3, arbitré le 2026-11-16) : `B` émet 1 à 3 sur `s1` sans abonné local, puis un
    /// abonné simple apparaît sur `B` ; `A` émet 4 — l'abonné ne reçoit que le 4 avec `Some(4)`.
    /// L'assertion sur `cursor_since` de `B` juste après l'arrivée de l'abonné est le témoin
    /// discriminant : le résultat observable seul ne distingue pas un curseur retenu à 3 d'une
    /// reprise depuis `seq − 1`.
    ///
    /// Verrou C-TESTS du 2026-11-17 : la reprise est en plus observée **au point de lecture** —
    /// la trace `debug` « relay cursor replay from journal » porte le paramètre `after` réellement
    /// passé à `read_after`, qui doit être `Some(3)` (soit `seq − 1`), jamais `Some(0)` ni `None`.
    /// Un filtre `seq <= since` qui masquerait une lecture partie de 0 (résultat observable
    /// identique, journal relu en entier) rougit ici alors qu'il laissait le test muet. Runtime
    /// `current_thread` : la capture thread-local doit voir la trace de la tâche de relais de `B`.
    #[tokio::test(flavor = "current_thread")]
    async fn un_abonne_tardif_ne_rejoue_pas_ce_que_la_replique_a_emis_sans_abonne() {
        let db = crate::lymaille::store::test_support::test_db().await;
        let bus = Bus::new();
        let hub_a = EventHub::new(None, 256)
            .with_log(SeaEventLog::new(db.clone()), journal_config())
            .with_relay(bus.relay(256));
        let hub_b = EventHub::new(None, 256)
            .with_log(SeaEventLog::new(db.clone()), journal_config())
            .with_relay(bus.relay(256));
        let log = SeaEventLog::new(db);
        hub_a.register(&session("s1"), "alice");
        hub_b.register(&session("s1"), "alice");
        for turn in 1..=3 {
            hub_b.emit(text_delta("s1", turn, "b sans abonné"));
        }
        journal_settled(&log, "s1", 3).await;
        let mut on_b = hub_b.subscribe("alice", &session("s1"));
        assert_eq!(
            hub_b.inner.last_cursor("s1"),
            None,
            "B n'a retenu aucun curseur pour ce qu'il a émis sans abonné"
        );
        let (_guard, captured) = capture_traces();
        hub_a.emit(text_delta("s1", 4, "a"));
        let (cursor, _) = recv_cursor_of(&mut on_b)
            .await
            .expect("le quatrième événement arrive sur B");
        assert_eq!(
            cursor,
            Some(4),
            "le premier événement reçu par l'abonné tardif est le quatrième"
        );
        let reads = || {
            captured
                .lines()
                .iter()
                .filter(|line| line.contains("relay cursor replay from journal"))
                .cloned()
                .collect::<Vec<String>>()
        };
        wait_until(|| !reads().is_empty()).await;
        assert_eq!(
            reads()
                .iter()
                .filter(|line| line.contains("after=Some(3)"))
                .count(),
            1,
            "la lecture déclenchée par Cursor(4) part de seq − 1 = 3 : {:?}",
            reads()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let quiet = tokio::time::timeout(Duration::from_millis(100), on_b.recv()).await;
        assert!(
            quiet.is_err(),
            "aucun des événements 1 à 3 n'est rejoué à l'abonné tardif"
        );
    }

    /// Verrou C-TESTS du 2026-11-17 — « pas de doublon à la reprise » (nom proposé
    /// `cursor_recu_pendant_une_diffusion_locale_ne_double_pas`, ./hub.sdd Tasks) : un `Cursor`
    /// distant arrive **pendant** la diffusion locale du même `seq` ; l'abonné ne reçoit
    /// l'événement qu'une seule fois — la seconde trame, de curseur ≤ au dernier servi, est
    /// ignorée par `recv_cursor` (`Must not` de ./hub.sdd : servir un événement dont le curseur
    /// est ≤ au dernier déjà servi à ce même abonné).
    ///
    /// La course est jouée de façon déterministe, même recette que
    /// `reprise_apres_un_curseur_sans_trou_ni_doublon` (sémaphore FIFO du pool SQLite à une
    /// connexion) :
    /// 1. le test retient l'unique connexion du pool ;
    /// 2. l'émission locale de `B` est mise en file : la tâche d'écriture s'aligne sur le pool,
    ///    PREMIÈRE attendante (battement de lot de 10 ms compris) ;
    /// 3. un `Cursor(1)` d'une autre réplique est injecté sur le bus : la branche `Cursor` de `B`
    ///    constate l'abonné local, calcule `since = 0` (aucun curseur retenu) et s'aligne sur le
    ///    pool depuis derrière la tâche d'écriture ;
    /// 4. connexion rendue : le FIFO sert l'écriture d'abord — commit de la ligne 1, trame
    ///    directe avec curseur ; la relecture s'exécute après le commit, retrouve la ligne 1 et
    ///    diffuse sa propre trame du même curseur ;
    /// 5. témoin de la course : un **second récepteur brut** du même canal (le `broadcast`
    ///    diffuse à tous) lit deux trames portant le curseur 1 — si le pool servait les
    ///    attendants dans l'autre ordre, cette affirmation rougit au lieu de laisser un test
    ///    silencieusement non discriminant (précédent l.108).
    /// Ensuite : `recv_cursor` sert le curseur 1 une fois, puis silence — la trame dupliquée est
    /// consumée par le filtre de déduplication.
    #[tokio::test]
    async fn cursor_recu_pendant_une_diffusion_locale_ne_double_pas() {
        use sea_orm::TransactionTrait as _;

        let db = crate::lymaille::store::test_support::test_db().await;
        let bus = Bus::new();
        let hub_b = EventHub::new(None, 256)
            .with_log(SeaEventLog::new(db.clone()), journal_config())
            .with_relay(bus.relay(256));
        // Relais témoin branché sur le bus : le test l'utilise pour injecter un message « venu
        // d'une autre réplique » dont l'origin n'est celle d'aucun hub.
        let inject = bus.relay(256);
        hub_b.register(&session("s1"), "alice");
        let mut on_b = hub_b.subscribe("alice", &session("s1"));
        // Second abonné-témoin, lu BRUT (champ `receiver`) : la déduplication verrouillée reste
        // exercée par `on_b` seul.
        let mut watcher = hub_b.subscribe("alice", &session("s1"));
        // 1. L'unique connexion du pool est retenue : rien n'écrit, rien ne lit.
        let garde = db
            .begin()
            .await
            .expect("l'unique connexion du pool de test est retenue");
        // 2. L'émission locale part vers le journal ; la tâche d'écriture s'aligne sur le pool.
        hub_b.emit(text_delta("s1", 1, "course"));
        tokio::time::sleep(Duration::from_millis(40)).await;
        // 3. Le curseur d'une autre réplique arrive pendant cette diffusion : la relecture de
        //    `B` s'aligne sur le pool derrière la tâche d'écriture.
        inject.publish(RelayMessage {
            origin: "autre-replique".to_owned(),
            session: "s1".to_owned(),
            body: RelayBody::Cursor(1),
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        // 4. Connexion rendue : le FIFO sert l'écriture avant la relecture ; la `SELECT` de
        //    celle-ci s'exécute après le commit et retrouve la ligne 1.
        drop(garde);
        // 5. Témoin de la course, puis service et déduplication.
        for want in 1..=2 {
            let frame = tokio::time::timeout(Duration::from_millis(2_000), watcher.receiver.recv())
                .await
                .unwrap_or_else(|_| panic!("la {want}-ième trame du curseur 1 est parvenue au canal-témoin"))
                .expect("le canal n'est pas fermé");
            assert!(
                matches!(frame, super::Frame::Event(_event, Some(1))),
                "les DEUX trames parvenues au canal portent le curseur 1 : la trame directe et \
                 la trame rejouée — la course a bien eu lieu"
            );
        }
        let (cursor, received) = recv_cursor_of(&mut on_b)
            .await
            .expect("l'abonné reçoit l'événement");
        assert_eq!(cursor, Some(1), "l'événement porte le curseur 1");
        assert!(
            matches!(received.kind, SessionEventKind::TextDelta { turn: 1, .. }),
            "l'événement reçu est bien celui de la course : {:?}",
            received.kind
        );
        let quiet = tokio::time::timeout(Duration::from_millis(100), on_b.recv_cursor()).await;
        assert!(
            quiet.is_err(),
            "la seconde trame du même `seq`, parvenue au canal, est ignorée par `recv_cursor` : \
             l'abonné ne reçoit l'événement qu'une seule fois"
        );
    }

    /// Libellé propre à la file du journal fermée (tâche d'écriture morte, `TrySendError::Closed`)
    /// — distinct de « queue full » ; `Missed` reste publié dans les deux cas (arbitrage du
    /// 2026-11-16 de ./hub.sdd, issue #81 lot 3). La tâche morte est simulée par une file dont le
    /// récepteur a été lâché.
    #[tokio::test(flavor = "current_thread")]
    async fn file_fermee_du_journal_a_son_propre_warn() {
        let db = crate::lymaille::store::test_support::test_db().await;
        let hub = EventHub::new(None, 256).with_log(SeaEventLog::new(db.clone()), journal_config());
        hub.register(&session("s1"), "alice");
        let (closed_queue, closed_receiver) = mpsc::channel(1);
        drop(closed_receiver);
        *lock(&hub.inner.journal) = Some(Arc::new(super::JournalHandle {
            log: SeaEventLog::new(db),
            queue: closed_queue,
            replay_max: 10,
        }));
        let (_guard, captured) = capture_traces();
        hub.emit(text_delta("s1", 1, "perdu"));
        assert!(
            captured
                .lines()
                .iter()
                .any(|line| line.contains("journal queue closed")),
            "une file fermée a son libellé : {:?}",
            captured.lines()
        );
        assert!(
            !captured.lines().iter().any(|line| line.contains("queue full")),
            "une file fermée n'est pas annoncée comme pleine : {:?}",
            captured.lines()
        );
    }

    // ── File pleine et panne d'écriture : `Missed` au relais (issue #81, lot 3,
    //    arbitrage du 2026-11-16) ──────────────────────────────────────────────

    /// Capture des lignes `tracing` (`tracing-subscriber` en dev-dependency, même motif que
    /// `src/auth/oidc.rs`). Le `DefaultGuard` est thread-local : les tests qui assurent un
    /// `warn` tournent donc sur un runtime `current_thread`, où la tâche d'écriture — émettrice
    /// de ces traces — est rejouée sur le fil du test.
    /// Capture thread-local des traces — `pub(crate)` : `./pg_relay.rs` la rejoue sous
    /// `lymaille-pg` (Scenario « un canal plein abandonne sans bloquer »).
    #[derive(Clone, Default)]
    pub(crate) struct CapturedTraces(Arc<std::sync::Mutex<Vec<String>>>);

    impl CapturedTraces {
        pub(crate) fn lines(&self) -> Vec<String> {
            lock(&self.0).clone()
        }
    }

    struct CaptureLayer(Arc<std::sync::Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            if !event.metadata().target().starts_with("miryad_core") {
                return;
            }
            let mut visitor = MessageVisitor::default();
            event.record(&mut visitor);
            lock(&self.0).push(visitor.finish());
        }
    }

    #[derive(Default)]
    struct MessageVisitor {
        rendered: String,
    }

    impl MessageVisitor {
        fn finish(&self) -> String {
            self.rendered.clone()
        }
    }

    impl tracing::field::Visit for MessageVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;

            if !self.rendered.is_empty() {
                self.rendered.push(' ');
            }
            let _ = write!(&mut self.rendered, "{}={value:?}", field.name());
        }
    }

    pub(crate) fn capture_traces() -> (tracing::subscriber::DefaultGuard, CapturedTraces) {
        use tracing_subscriber::layer::SubscriberExt as _;
        let captured = CapturedTraces::default();
        let subscriber =
            tracing_subscriber::registry::Registry::default().with(CaptureLayer(Arc::clone(&captured.0)));
        let guard = tracing::subscriber::set_default(subscriber);
        (guard, captured)
    }

    /// `Scenario` « file pleine ou panne d'écriture : le relais publie Missed » — cas (a), la
    /// file du journal pleine. Runtime `current_thread` (horloge réelle, base connectée) : la
    /// boucle d'émission synchrone ne cède jamais la main, la tâche d'écriture ne peut donc pas
    /// vider la file de 64 pendant les 200 émissions — le débordement est déterministe et la
    /// capture thread-local voit les traces du test comme celles de la tâche.
    #[tokio::test(flavor = "current_thread")]
    async fn file_pleine_du_journal_le_relais_publie_missed() {
        let db = crate::lymaille::store::test_support::test_db().await;
        let bus = Bus::new();
        let hub_a = EventHub::new(None, 256)
            .with_log(
                SeaEventLog::new(db.clone()),
                LogConfig {
                    queue_capacity: 64,
                    flush_interval: Duration::from_secs(1),
                    ..journal_config()
                },
            )
            .with_relay(bus.relay(256));
        let hub_b = EventHub::new(None, 256).with_relay(bus.relay(256));
        hub_a.register(&session("s1"), "alice");
        let mut on_a = hub_a.subscribe("alice", &session("s1"));
        let mut on_b = hub_b.subscribe("alice", &session("s1"));
        let mut on_b_s2 = hub_b.subscribe("alice", &session("s2"));
        let (_guard, captured) = capture_traces();
        let started = std::time::Instant::now();
        for turn in 1..=200 {
            hub_a.emit(text_delta("s1", turn, "x"));
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "`emit` n'attend jamais la file du journal : {:?}",
            started.elapsed()
        );
        let (cursor, _event) = recv_cursor_of(&mut on_a)
            .await
            .expect("l'abonné local reçoit les événements déversés par la file pleine");
        assert_eq!(cursor, None, "file pleine : l'événement part sans curseur");
        let lagged = tokio::time::timeout(Duration::from_millis(500), on_b.recv())
            .await
            .expect("l'abonné de l'autre hub est averti")
            .expect_err("un Missed devient Lagged(1)");
        assert_eq!(lagged, SubscriptionError::Lagged(1), "le relais a publié Missed");
        let quiet = tokio::time::timeout(Duration::from_millis(100), on_b_s2.recv()).await;
        assert!(
            quiet.is_err(),
            "Missed vise la session concernée, pas le wildcard `*`"
        );
        assert!(
            captured.lines().iter().any(|line| line.contains("queue full")),
            "un `warn` est tracé, sans contenu d'événement : {:?}",
            captured.lines()
        );
    }

    /// `Scenario` « file pleine ou panne d'écriture : le relais publie Missed » — cas (b), la
    /// panne d'écriture (table du journal retirée, même recette que
    /// `une_panne_decriture_n_interrompt_pas_le_flux`). Runtime `current_thread` : le `warn`
    /// part de la tâche d'écriture, qui doit être rejouée sur le fil du test pour être capturée.
    #[tokio::test(flavor = "current_thread")]
    async fn panne_decriture_le_relais_publie_missed() {
        let db = crate::lymaille::store::test_support::test_db().await;
        db.execute_unprepared("DROP TABLE miryad_lymaille_session_events")
            .await
            .expect("la table du journal est retirée : tout append échouera");
        let bus = Bus::new();
        let hub_a = EventHub::new(None, 256)
            .with_log(SeaEventLog::new(db), journal_config())
            .with_relay(bus.relay(256));
        let hub_b = EventHub::new(None, 256).with_relay(bus.relay(256));
        hub_a.register(&session("s1"), "alice");
        let mut on_a = hub_a.subscribe("alice", &session("s1"));
        let mut on_b = hub_b.subscribe("alice", &session("s1"));
        let mut on_b_s2 = hub_b.subscribe("alice", &session("s2"));
        let (_guard, captured) = capture_traces();
        let started = std::time::Instant::now();
        hub_a.emit(text_delta("s1", 1, "x"));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "`emit` n'attend jamais le journal : {:?}",
            started.elapsed()
        );
        let (cursor, _event) = recv_cursor_of(&mut on_a)
            .await
            .expect("l'abonné reçoit malgré la panne du journal");
        assert_eq!(cursor, None, "panne d'écriture : le lot part sans curseur");
        let lagged = tokio::time::timeout(Duration::from_millis(500), on_b.recv())
            .await
            .expect("l'abonné de l'autre hub est averti")
            .expect_err("un Missed devient Lagged(1)");
        assert_eq!(lagged, SubscriptionError::Lagged(1), "le relais a publié Missed");
        let quiet = tokio::time::timeout(Duration::from_millis(100), on_b_s2.recv()).await;
        assert!(
            quiet.is_err(),
            "Missed vise la session concernée, pas le wildcard `*`"
        );
        assert!(
            captured
                .lines()
                .iter()
                .any(|line| line.contains("journal write failed")),
            "un `warn` est tracé, sans contenu d'événement : {:?}",
            captured.lines()
        );
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
