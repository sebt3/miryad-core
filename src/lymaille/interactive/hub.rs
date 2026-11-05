//! Diffusion des événements d'une session vers ses abonnés (`src/lymaille/interactive/hub.sdd`).
//!
//! Le `EventSink` des sessions interactives (DSD `FR-11`, `DEC-24`, amendé `DEC-30`) : `lymaille`
//! émet ses événements vers [`EventHub::emit`], synchrone, qui ne bloque pas et ne peut pas
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

use tokio::sync::broadcast;

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

/// File d'une session : le canal `broadcast` partagé entre les abonnés de cette session.
struct Slot {
    sender: broadcast::Sender<Arc<SessionEvent>>,
}

/// État partagé de l'hub (`Clone` : tous les clones partagent les mêmes canaux).
struct Inner {
    audit: Option<AuditWriter>,
    capacity: usize,
    /// `subject` de chaque session enregistrée — lu par l'audit des erreurs de script.
    subjects: Mutex<HashMap<String, String>>,
    /// Canal de diffusion de chaque session ayant un abonné (ou attendant le premier).
    slots: Mutex<HashMap<String, Arc<Slot>>>,
}

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
                subjects: Mutex::new(HashMap::new()),
                slots: Mutex::new(HashMap::new()),
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
        let mut slots = lock(&self.inner.slots);
        if let Some(slot) = slots.get(id.as_str()) {
            if slot.sender.receiver_count() == 0 {
                slots.remove(id.as_str());
            } else {
                let _sent = slot.sender.send(event);
            }
        }
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
        let mut slots = lock(&self.inner.slots);
        if let Some(slot) = slots.get(event.session.as_str()) {
            if slot.sender.receiver_count() == 0 {
                // Sans abonné, rien ne s'accumule : l'événement est abandonné et le canal
                // libéré (`Must not` de ./hub.sdd : retenir des événements sans abonné).
                slots.remove(event.session.as_str());
            } else {
                // `send` n'attend jamais ; `SendError` (abonné parti entre-temps) est ignoré.
                let _sent = slot.sender.send(shared);
            }
        }
    }
}

/// Abonnement à la file d'une session, rendu par [`EventHub::subscribe`].
#[derive(Debug)]
pub struct Subscription {
    receiver: broadcast::Receiver<Arc<SessionEvent>>,
}

impl Subscription {
    /// Rend l'événement suivant de sa session.
    ///
    /// # Errors
    ///
    /// [`SubscriptionError::Lagged`] si la file a débordé (n événements perdus) ;
    /// [`SubscriptionError::Closed`] si le canal a été fermé par [`EventHub::close`] ou retiré.
    pub async fn recv(&mut self) -> Result<SessionEvent, SubscriptionError> {
        self.receiver
            .recv()
            .await
            .map(|shared| (*shared).clone())
            .map_err(|error| match error {
                broadcast::error::RecvError::Lagged(n) => SubscriptionError::Lagged(n),
                broadcast::error::RecvError::Closed => SubscriptionError::Closed,
            })
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::EventHub;
    use super::HOST_PAYLOAD_MAX;
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
}
