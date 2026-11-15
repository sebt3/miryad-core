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

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::sync::mpsc;

use ::lymaille::Event;
use ::lymaille::EventKind;
use ::lymaille::EventSink;
use ::lymaille::SessionId;

use super::event::SessionEvent;
use crate::lymaille::error::LymailleError;
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

/// Ce que la file d'une session transporte : un événement à diffuser, ou la marque « des
/// événements ont pu être manqués » injectée par un `Missed` venu d'une autre réplique
/// (amendement #81 lot 2 de ./hub.sdd). `Clone` : exigé par `broadcast::Sender::send`.
#[derive(Clone)]
enum Frame {
    Event(Arc<SessionEvent>),
    Lagged(u64),
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
        }
    }

    /// Ferme le canal de `id` : les abonnés reçoivent [`SubscriptionError::Closed`]. Appelé quand
    /// le gestionnaire retire la session ; libère aussi son `subject`.
    pub fn close(&self, id: &SessionId) {
        lock(&self.inner.slots).remove(id.as_str());
        lock(&self.inner.subjects).remove(id.as_str());
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
        self.inner.deliver(id.as_str(), Frame::Event(Arc::clone(&event)));
        // Amendement #81 lot 2 : un `publish_host` valide publie aussi vers le relais, **après**
        // la diffusion locale. Sans relais : rien ne change.
        self.inner.relay_event(&event);
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
                inner.apply_relay_message(&message);
            }
            tracing::debug!("lymaille hub: relay incoming stream closed");
        });
        self
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
        self.inner
            .deliver(event.session.as_str(), Frame::Event(Arc::clone(&shared)));
        // Amendement #81 lot 2 : un `emit` valide publie aussi vers le relais, **après** la
        // diffusion locale (y compris sans abonné local : d'autres répliques peuvent en avoir).
        // Sans relais : rien ne change.
        self.inner.relay_event(&shared);
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

    /// Traite un message **venu d'une autre réplique** : le message de la réplique elle-même est
    /// ignoré (aucune boucle) ; un `Event` est diffusé localement **sans audit** (l'audit
    /// appartient à la réplique émettrice) et **sans re-relais** ; `Missed` devient `Lagged(1)`
    /// pour les abonnés locaux (wildcard `*` : toutes les sessions) ; `Cancel`/`Close` sont remis
    /// au gestionnaire ; `Cursor` est la variante du lot 3 (journal) — sans journal, ignorée.
    fn apply_relay_message(&self, message: &RelayMessage) {
        if message.origin == self.origin {
            return;
        }
        match &message.body {
            RelayBody::Event(event) => {
                self.deliver(&message.session, Frame::Event(Arc::new(event.clone())));
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
            RelayBody::Cursor(_sequence) => {
                // Lot 3 de ./hub.sdd (journal et curseurs) : la réplique réceptrice relit le
                // journal. Le lot 2 n'a pas de journal — la variante est traitée explicitement.
                tracing::debug!("lymaille hub: relay cursor ignored without a journal");
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

/// Abonnement à la file d'une session, rendu par [`EventHub::subscribe`].
#[derive(Debug)]
pub struct Subscription {
    receiver: broadcast::Receiver<Frame>,
}

impl Subscription {
    /// Rend l'événement suivant de sa session.
    ///
    /// # Errors
    ///
    /// [`SubscriptionError::Lagged`] si la file a débordé (n événements perdus), ou si un
    /// `Missed` est venu du relais (« des événements ont pu être manqués », 1 au minimum —
    /// amendement #81 lot 2 de ./hub.sdd) ; [`SubscriptionError::Closed`] si le canal a été
    /// fermé par [`EventHub::close`] ou retiré.
    pub async fn recv(&mut self) -> Result<SessionEvent, SubscriptionError> {
        match self.receiver.recv().await {
            Ok(Frame::Event(shared)) => Ok((*shared).clone()),
            Ok(Frame::Lagged(missed)) => Err(SubscriptionError::Lagged(missed)),
            Err(broadcast::error::RecvError::Lagged(n)) => Err(SubscriptionError::Lagged(n)),
            Err(broadcast::error::RecvError::Closed) => Err(SubscriptionError::Closed),
        }
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
}
