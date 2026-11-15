//! `PgEventRelay` — relais d'événements entre répliques par LISTEN/NOTIFY PostgreSQL
//! (`src/lymaille/interactive/pg_relay.sdd`, DSD `FR-11`, `DEC-31`, issue #81 lot 2).
//!
//! L'implémentation partagée de la couture [`EventRelay`] (./hub.sdd) : les messages que le hub
//! publie (événements, curseurs, `Missed`, commandes `Cancel` et `Close`) partent par
//! `pg_notify` sur un canal unique de la base de l'application et sont reçus par chaque
//! réplique qui y écoute (`LISTEN`) — un abonné SSE d'une réplique voit ainsi les événements
//! d'un tour exécuté sur une autre. Sans PostgreSQL, ou sans la feature de compilation
//! `lymaille-pg`, aucun relais n'est construit et le hub reste en mémoire.
//!
//! Le relais est **volatil** : un message non reçu est perdu (le curseur du journal de
//! `../store/events.sdd` est le filet du lot 3). L'écoute tient **une connexion dédiée**,
//! jamais le pool aux moments des envois ; sa perte déclenche une reconnexion à attente
//! exponentielle et, après chaque reconnexion réussie, un unique `Missed` global
//! (`session = "*"`) : des événements ont pu être manqués pour toutes les sessions.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use sea_orm::ConnectionTrait as _;
use sea_orm::DatabaseBackend;
use sea_orm::DatabaseConnection;
use sea_orm::Statement;
use sea_orm::Value;
use sea_orm::sqlx::postgres::PgListener;
use tokio::sync::mpsc;

use super::hub::EventRelay;
use super::hub::RelayBody;
use super::hub::RelayMessage;
use crate::lymaille::error::LymailleError;

/// Basse de la file interne (émission comme réception), quel que soit le `queue_capacity`
/// demandé.
const QUEUE_MIN: usize = 16;

/// Haute de la file interne, quel que soit le `queue_capacity` demandé.
const QUEUE_MAX: usize = 65_536;

/// Basse de l'attente maximale de reconnexion : sous 1 s, la reconnexion bouclerait plus vite
/// qu'un battement réseau utile.
const RECONNECT_MIN: Duration = Duration::from_secs(1);

/// Haute de l'attente maximale de reconnexion (le plafond de l'attente exponentielle).
const RECONNECT_MAX: Duration = Duration::from_secs(300);

/// Premier terme de l'attente exponentielle de reconnexion (1 s, 2 s, 4 s…, plafonnée à
/// `reconnect_max`).
const RECONNECT_FIRST: Duration = Duration::from_secs(1);

/// Canal par défaut : celui de la base de l'application, nommé pour le module.
const DEFAULT_CHANNEL: &str = "miryad_lymaille_events";

/// Dernière barrière de taille : la charge de `NOTIFY` est plafonnée à 8 000 octets côté
/// PostgreSQL — au-delà de 7 900 le message part avec un corps [`RelayBody::Missed`] (le hub
/// borne déjà les événements à `RELAY_EVENT_MAX`, ./hub.sdd).
const PAYLOAD_MAX: usize = 7_900;

/// Session wildcard du `Missed` global poussé après reconnexion.
const ALL_SESSIONS: &str = "*";

/// Requête d'émission : canal et charge **liés** (`$1`, `$2`), jamais interpolés.
const NOTIFY_SQL: &str = "SELECT pg_notify($1, $2)";

/// Configuration de [`PgEventRelay`] — `queue_capacity` et `reconnect_max` sont ramenés à leur
/// borne la plus proche à la construction (`connect`), `channel` est un identifiant SQL
/// `[a-z0-9_]{1,63}`, refusé sinon.
#[derive(Clone, Debug)]
pub struct PgRelayConfig {
    /// Canal `LISTEN/NOTIFY` de la base de l'application.
    pub channel: String,
    /// File bornée interne (émission et réception), bornée [16, 65 536].
    pub queue_capacity: usize,
    /// Plafond de l'attente exponentielle de reconnexion, borné [1 s, 300 s].
    pub reconnect_max: Duration,
}

impl Default for PgRelayConfig {
    fn default() -> Self {
        Self {
            channel: DEFAULT_CHANNEL.to_owned(),
            queue_capacity: 1_024,
            reconnect_max: Duration::from_secs(30),
        }
    }
}

impl PgRelayConfig {
    /// La configuration ramenée à ses bornes (`queue_capacity` [16, 65 536], `reconnect_max`
    /// [1 s, 300 s]) — la fonction de bornage appliquée par `connect`.
    #[must_use]
    pub fn clamped(&self) -> Self {
        Self {
            channel: self.channel.clone(),
            queue_capacity: self.queue_capacity.clamp(QUEUE_MIN, QUEUE_MAX),
            reconnect_max: self.reconnect_max.clamp(RECONNECT_MIN, RECONNECT_MAX),
        }
    }
}

/// Le canal est-il un identifiant SQL `[a-z0-9_]{1,63}` ? (refus avant toute requête — la
/// valeur part liée dans `pg_notify($1, …)` et interpolée dans `LISTEN`, seule place où elle
/// pourrait le être).
fn channel_valid(channel: &str) -> bool {
    !channel.is_empty()
        && channel.len() <= 63
        && channel
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Verrou des moitiés de canal internes : un verrou empoisonné est récupéré — l'état est un
/// `Option<Receiver>` qui ne peut pas rester à demi écrit (précédent `./hub.rs`).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Le relais `LISTEN/NOTIFY` de la couture [`EventRelay`] (`./pg_relay.sdd`). Construit par
/// [`PgEventRelay::connect`] ; `Arc` de fabrique : les clones partagent les mêmes tâches.
#[derive(Debug)]
pub struct PgEventRelay {
    /// La file d'émission : des charges JSON déjà nées ou remplacées par `Missed`.
    file: mpsc::Sender<Vec<u8>>,
    /// Pousses vers `incoming()` (file bornée ; pleine → message abandonné + `warn`).
    /// Lu jamais directement : le tenir vivant borne la durée de vie du flux — relais déposé,
    /// `incoming()` rend `None` et la tâche du hub se termine.
    _entree: mpsc::Sender<RelayMessage>,
    /// Moitié réception, prise une fois par [`EventHub::with_relay`](super::hub::EventHub::with_relay).
    recepteur: Mutex<Option<mpsc::Receiver<RelayMessage>>>,
}

impl PgEventRelay {
    /// Construit le relais sur la base `db` : backend autre que PostgreSQL →
    /// [`LymailleError::InvalidDefinition`] (`postgres required`) ; `channel` hors
    /// `[a-z0-9_]{1,63}` → `InvalidDefinition` (`invalid relay channel`), **avant toute
    /// requête** ; échec de connexion de l'écouteur → [`LymailleError::Store`].
    ///
    /// # Errors
    ///
    /// Voir ci-dessus ; jamais un `DbErr` nu.
    pub async fn connect(
        db: &DatabaseConnection,
        config: PgRelayConfig,
    ) -> Result<Arc<PgEventRelay>, LymailleError> {
        if !channel_valid(&config.channel) {
            return Err(LymailleError::InvalidDefinition(
                "invalid relay channel".to_owned(),
            ));
        }
        if db.get_database_backend() != DatabaseBackend::Postgres {
            return Err(LymailleError::InvalidDefinition("postgres required".to_owned()));
        }
        let config = config.clamped();
        let (emis_tx, mut emis_rx) = mpsc::channel::<Vec<u8>>(config.queue_capacity);
        let (entree_tx, entree_rx) = mpsc::channel::<RelayMessage>(config.queue_capacity);

        // Émission : une tâche, dans l'ordre de dépôt, sur le pool de l'application —
        // `SELECT pg_notify($1, $2)` lié, jamais de contenu dans les traces.
        let envoi_db = db.clone();
        let canal = config.channel.clone();
        tokio::spawn(async move {
            while let Some(charge) = emis_rx.recv().await {
                // `pg_notify(text, text)` : la charge JSON est du UTF-8 — liée en TEXT, jamais
                // en `bytea` (aucune signature `pg_notify(…, bytea)` ne résoudrait).
                let requete = Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    NOTIFY_SQL,
                    [
                        Value::from(canal.as_str()),
                        Value::from(String::from_utf8_lossy(&charge).into_owned()),
                    ],
                );
                if envoi_db.execute_raw(requete).await.is_err() {
                    tracing::warn!("lymaille pg relay: pg_notify failed (payload dropped)");
                }
            }
        });

        // Réception : une connexion **dédiée** tirée du pool (`PgListener`), une seule par
        // instance ; le premier `LISTEN` est révélé par `connect` lui-même.
        let pool = db.get_postgres_connection_pool().clone();
        let premier = PgListener::connect_with(&pool).await.map_err(|_erreur| {
            tracing::warn!("lymaille pg relay: listener connect failed");
            LymailleError::Store("relay listener connect failed".to_owned())
        })?;
        let mut ecoute = premier;
        ecoute.listen(&config.channel).await.map_err(|_erreur| {
            tracing::warn!("lymaille pg relay: listener LISTEN failed");
            LymailleError::Store("relay listener LISTEN failed".to_owned())
        })?;
        let reveveur = entree_tx.clone();
        let canal_ecoute = config.channel.clone();
        let plafond = config.reconnect_max;
        tokio::spawn(async move {
            let mut ecoute = ecoute;
            ecoute.eager_reconnect(false);
            loop {
                let recu = ecoute.try_recv().await;
                match recu {
                    Ok(Some(notification)) => {
                        match serde_json::from_str::<RelayMessage>(notification.payload()) {
                            Ok(message) => {
                                if reveveur.try_send(message).is_err() {
                                    tracing::warn!("lymaille pg relay: incoming queue full, message dropped");
                                }
                            }
                            Err(_erreur) => {
                                tracing::warn!("lymaille pg relay: unreadable notification payload ignored");
                            }
                        }
                    }
                    Ok(None) => {
                        // Connexion d'écoute perdue : reconnexion à attente exponentielle,
                        // puis un unique `Missed` global (des événements ont pu être manqués
                        // pour toutes les sessions, ./hub.sdd).
                        tracing::warn!("lymaille pg relay: listener connection lost, reconnecting");
                        drop(ecoute);
                        ecoute = reecouter(&pool, &canal_ecoute, plafond).await;
                        pousser_missed_global(&reveveur);
                    }
                    Err(_erreur) => {
                        tracing::warn!("lymaille pg relay: listener failed, reconnecting");
                        drop(ecoute);
                        ecoute = reecouter(&pool, &canal_ecoute, plafond).await;
                        pousser_missed_global(&reveveur);
                    }
                }
            }
        });

        Ok(Arc::new(Self {
            file: emis_tx,
            _entree: entree_tx,
            recepteur: Mutex::new(Some(entree_rx)),
        }))
    }
}

impl EventRelay for PgEventRelay {
    /// Sérialise en JSON compact et dépose dans la file bornée : **synchrone, sans attente,
    /// infaillible** — file pleine → message abandonné et `warn` (jamais bloquant, jamais de
    /// contenu).
    fn publish(&self, message: RelayMessage) {
        let charge = encode_payload(&message);
        if self.file.try_send(charge).is_err() {
            tracing::warn!("lymaille pg relay: outgoing queue full, message dropped");
        }
    }

    /// Le flux des messages reçus, pris **une fois** ; un second appel rend un récepteur déjà
    /// clos (jamais de panique).
    fn incoming(&self) -> mpsc::Receiver<RelayMessage> {
        let mut garde = lock(&self.recepteur);
        if let Some(receiver) = garde.take() {
            return receiver;
        }
        let (sender, receiver) = mpsc::channel(1);
        drop(sender);
        receiver
    }
}

/// Reconnexion de l'écoute : attente exponentielle (1 s, 2 s, 4 s…, plafonnée à `plafond`),
/// nouvelle connexion dédiée, `LISTEN` reposé — jamais de panique, la boucle tient jusqu'au
/// succès.
async fn reecouter(
    pool: &sea_orm::sqlx::Pool<sea_orm::sqlx::Postgres>,
    canal: &str,
    plafond: Duration,
) -> PgListener {
    let mut attente = RECONNECT_FIRST;
    loop {
        tokio::time::sleep(attente).await;
        attente = attente.saturating_mul(2).min(plafond);
        let nouveau = match PgListener::connect_with(pool).await {
            Ok(nouveau) => nouveau,
            Err(_erreur) => {
                tracing::warn!("lymaille pg relay: reconnect failed");
                continue;
            }
        };
        let mut nouveau = nouveau;
        nouveau.eager_reconnect(false);
        match nouveau.listen(canal).await {
            Ok(()) => return nouveau,
            Err(_erreur) => {
                tracing::warn!("lymaille pg relay: LISTEN after reconnect failed");
            }
        }
    }
}

/// Pousse l'unique `Missed` global (`origin` vide, `session = "*"`) après une reconnexion
/// réussie ; file pleine → abandonné et `warn`.
fn pousser_missed_global(reveveur: &mpsc::Sender<RelayMessage>) {
    let global = RelayMessage {
        origin: String::new(),
        session: ALL_SESSIONS.to_owned(),
        body: RelayBody::Missed,
    };
    if reveveur.try_send(global).is_err() {
        tracing::warn!("lymaille pg relay: incoming queue full, missed dropped");
    }
}

/// La charge déposée dans la file d'émission : le message sérialisé en JSON compact ; au-delà
/// de [`PAYLOAD_MAX`] octets, le même message avec le corps [`RelayBody::Missed`] (dernière
/// barrière avant la limite de `NOTIFY`).
fn encode_payload(message: &RelayMessage) -> Vec<u8> {
    match serde_json::to_vec(message) {
        Ok(charge) if charge.len() > PAYLOAD_MAX => {
            let remplace = RelayMessage {
                origin: message.origin.clone(),
                session: message.session.clone(),
                body: RelayBody::Missed,
            };
            serde_json::to_vec(&remplace).unwrap_or_default()
        }
        Ok(charge) => charge,
        Err(_erreur) => {
            // Une forme infalsifiable ne se présente pas pour ces types ; si elle se
            // présentait, rien n'est émis (la trace ne cite ni la session ni le contenu).
            tracing::warn!("lymaille pg relay: message not serializable, dropped");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sea_orm::ConnectOptions;
    use sea_orm::ConnectionTrait as _;
    use sea_orm::Database;
    use sea_orm::DatabaseConnection;
    use sea_orm::TransactionTrait as _;

    use super::PgEventRelay;
    use super::PgRelayConfig;
    use super::channel_valid;
    use super::encode_payload;
    use crate::lymaille::LymailleError;
    use crate::lymaille::interactive::event::SessionEvent;
    use crate::lymaille::interactive::event::SessionEventKind;
    use crate::lymaille::interactive::hub::EventRelay as _;
    use crate::lymaille::interactive::hub::RelayBody;
    use crate::lymaille::interactive::hub::RelayMessage;

    /// Scenario « connect refuse un backend qui n'est pas PostgreSQL » : SQLite mémoire →
    /// `InvalidDefinition("postgres required")`.
    #[tokio::test]
    async fn connect_refuse_un_backend_qui_nest_pas_postgresql() {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("base SQLite mémoire de test");
        let erreur = PgEventRelay::connect(&db, PgRelayConfig::default())
            .await
            .expect_err("SQLite ne fait pas un relais");
        assert!(
            matches!(&erreur, LymailleError::InvalidDefinition(texte) if texte == "postgres required"),
            "refus nommé : {erreur:?}"
        );
    }

    /// Scenario « un nom de canal invalide est refusé » : `a;drop table x`, vide, puis 64
    /// caractères — trois `InvalidDefinition("invalid relay channel")`, aucune requête émise
    /// (le contrôle précède le backend et la base n'est même pas touchée).
    #[tokio::test]
    async fn un_nom_de_canal_invalide_est_refuse() {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("base SQLite mémoire de test");
        for canal in ["a;drop table x", "", &"a".repeat(64)] {
            let erreur = PgEventRelay::connect(
                &db,
                PgRelayConfig {
                    channel: canal.to_owned(),
                    ..PgRelayConfig::default()
                },
            )
            .await
            .expect_err("le canal est refusé");
            assert!(
                matches!(&erreur, LymailleError::InvalidDefinition(texte) if texte == "invalid relay channel"),
                "canal {canal:?} refusé sous son nom : {erreur:?}"
            );
        }
    }

    /// Le contrôle pur du format `[a-z0-9_]{1,63}` : 63 caractères passent, les autres non.
    #[test]
    fn le_canal_est_un_identifiant_sql() {
        assert!(channel_valid("miryad_lymaille_events"));
        assert!(channel_valid(&"a".repeat(63)));
        assert!(!channel_valid(&"a".repeat(64)));
        assert!(!channel_valid(""));
        assert!(!channel_valid("A_channel"));
        assert!(!channel_valid("a-b"));
        assert!(!channel_valid("a b"));
    }

    /// Scenario « la configuration est bornée » : `queue_capacity` 0 → 16, 1 000 000 →
    /// 65 536 ; `reconnect_max` 0 s → 1 s, 1 h → 300 s ; l'intérieur des bornes passe.
    #[test]
    fn la_configuration_est_bornee() {
        let bas = PgRelayConfig {
            queue_capacity: 0,
            reconnect_max: Duration::from_secs(0),
            ..PgRelayConfig::default()
        }
        .clamped();
        assert_eq!(bas.queue_capacity, 16);
        assert_eq!(bas.reconnect_max, Duration::from_secs(1));
        let haut = PgRelayConfig {
            queue_capacity: 1_000_000,
            reconnect_max: Duration::from_secs(3_600),
            ..PgRelayConfig::default()
        }
        .clamped();
        assert_eq!(haut.queue_capacity, 65_536);
        assert_eq!(haut.reconnect_max, Duration::from_secs(300));
        let median = PgRelayConfig {
            queue_capacity: 4_096,
            reconnect_max: Duration::from_secs(45),
            ..PgRelayConfig::default()
        }
        .clamped();
        assert_eq!(median.queue_capacity, 4_096);
        assert_eq!(median.reconnect_max, Duration::from_secs(45));
    }

    /// Test des tâches sans PostgreSQL : sérialisation/désérialisation d'un `RelayMessage` —
    /// aller-retour à l'identique pour les cinq corps.
    #[test]
    fn un_relay_message_fait_laller_retour_json() {
        let evenements = [
            RelayBody::Event(SessionEvent {
                session: "s1".to_owned(),
                at: "2026-11-15T10:00:00.000Z".to_owned(),
                kind: SessionEventKind::TextDelta {
                    turn: 3,
                    text: "bonjour".to_owned(),
                },
            }),
            RelayBody::Cursor(42),
            RelayBody::Missed,
            RelayBody::Cancel,
            RelayBody::Close,
        ];
        for body in evenements {
            let message = RelayMessage {
                origin: "replique-a".to_owned(),
                session: "s1".to_owned(),
                body,
            };
            let charge = serde_json::to_string(&message).expect("le message se sérialise");
            let rendu: RelayMessage = serde_json::from_str(&charge).expect("le message se relit");
            assert_eq!(rendu.origin, "replique-a");
            assert_eq!(rendu.session, "s1");
            match (&rendu.body, &message.body) {
                (RelayBody::Event(a), RelayBody::Event(b)) => {
                    assert_eq!(a, b);
                    assert_eq!(a.session, "s1");
                }
                (RelayBody::Cursor(a), RelayBody::Cursor(b)) => assert_eq!(a, b),
                (RelayBody::Missed, RelayBody::Missed)
                | (RelayBody::Cancel, RelayBody::Cancel)
                | (RelayBody::Close, RelayBody::Close) => {}
                (autre, _) => panic!("le corps a changé de forme : {autre:?}"),
            }
        }
    }

    /// Scenario « une charge trop grosse devient Missed » (volet sans base) : un événement
    /// sérialisé de 9 000 octets part avec le corps `Missed` pour la même session.
    #[test]
    fn une_charge_trop_grosse_devient_missed() {
        let message = RelayMessage {
            origin: "replique-a".to_owned(),
            session: "s9".to_owned(),
            body: RelayBody::Event(SessionEvent {
                session: "s9".to_owned(),
                at: "2026-11-15T10:00:00.000Z".to_owned(),
                kind: SessionEventKind::TextDelta {
                    turn: 1,
                    text: "x".repeat(9_000),
                },
            }),
        };
        let charge = encode_payload(&message);
        assert!(
            charge.len() <= 7_900,
            "la charge est sous la barrière : {}",
            charge.len()
        );
        let rendu: RelayMessage = serde_json::from_slice(&charge).expect("la charge relue est un message");
        assert!(
            matches!(rendu.body, RelayBody::Missed),
            "le corps est Missed : {:?}",
            rendu.body
        );
        assert_eq!(rendu.session, "s9", "la session est conservée");
        assert_eq!(rendu.origin, "replique-a");
    }

    /// Une charge raisonnable passe telle quelle (la barrière ne mord pas d'office).
    #[test]
    fn une_charge_normale_passe_telle_quelle() {
        let message = RelayMessage {
            origin: "replique-a".to_owned(),
            session: "s1".to_owned(),
            body: RelayBody::Missed,
        };
        let charge = encode_payload(&message);
        let rendu: RelayMessage = serde_json::from_slice(&charge).expect("la charge se relit");
        assert!(matches!(rendu.body, RelayBody::Missed));
    }

    // ── Scénarios contre PostgreSQL réel (`#[ignore]`, convention `MIRYAD_TEST_PG_URL`) ──

    /// Variable d'environnement portant l'URL PostgreSQL de test (jamais affichée).
    const PG_URL_VAR: &str = "MIRYAD_TEST_PG_URL";

    fn pg_url() -> Option<String> {
        std::env::var(PG_URL_VAR).ok().filter(|v| !v.is_empty())
    }

    /// Connexion dédiée (pool borné) ; l'URL n'est jamais affichée.
    async fn pg_db(url: &str, max: u32) -> DatabaseConnection {
        let mut opts = ConnectOptions::new(url);
        opts.max_connections(max).min_connections(1);
        Database::connect(opts).await.unwrap_or_else(|err| {
            panic!(
                "connexion PostgreSQL impossible : {}",
                scrub_url(&err.to_string(), url)
            )
        })
    }

    fn scrub_url(text: &str, url: &str) -> String {
        text.replace(url, "<url>")
    }

    /// Canal propre à l'exécution : `[a-z0-9_]{1,63}`, jamais partagé avec un autre test.
    fn canal_unique() -> String {
        format!("miryad_lymaille_t_{:016x}", rand::random::<u64>())
    }

    fn config(canal: &str) -> PgRelayConfig {
        PgRelayConfig {
            channel: canal.to_owned(),
            ..PgRelayConfig::default()
        }
    }

    fn message(session: &str, texte: &str) -> RelayMessage {
        RelayMessage {
            origin: "emetteur".to_owned(),
            session: session.to_owned(),
            body: RelayBody::Event(SessionEvent {
                session: session.to_owned(),
                at: "2026-11-15T10:00:00.000Z".to_owned(),
                kind: SessionEventKind::TextDelta {
                    turn: 1,
                    text: texte.to_owned(),
                },
            }),
        }
    }

    /// Reçoit au plus `attendus` messages dans `délai`.
    async fn collect(
        receveur: &mut tokio::sync::mpsc::Receiver<RelayMessage>,
        attendus: usize,
        delai: Duration,
    ) -> Vec<RelayMessage> {
        let mut vus = Vec::new();
        let echeance = tokio::time::Instant::now() + delai;
        while vus.len() < attendus {
            let reste = echeance.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(reste, receveur.recv()).await {
                Ok(Some(message)) => vus.push(message),
                Ok(None) | Err(_) => break,
            }
        }
        vus
    }

    /// Scenario « un message émis sur une réplique est reçu sur l'autre » (PostgreSQL réel) :
    /// trois messages en ordre avec leur `origin` et leur `session` ; le premier reçoit aussi
    /// les siens (le hub les ignore) ; un second `incoming()` rend un canal déjà clos.
    #[tokio::test]
    #[ignore = "exige PostgreSQL (MIRYAD_TEST_PG_URL)"]
    async fn un_message_emis_sur_une_replique_est_recu_sur_autre() {
        let Some(url) = pg_url() else { return };
        let canal = canal_unique();
        let un = PgEventRelay::connect(&pg_db(&url, 2).await, config(&canal))
            .await
            .expect("premier relais");
        let deux = PgEventRelay::connect(&pg_db(&url, 2).await, config(&canal))
            .await
            .expect("second relais");
        let mut recu_un = un.incoming();
        let mut recu_deux = deux.incoming();
        for index in 1..=3 {
            un.publish(message("s1", &format!("m{index}")));
        }
        let recus = collect(&mut recu_deux, 3, Duration::from_secs(10)).await;
        assert_eq!(recus.len(), 3, "les trois messages arrivent");
        for (index, recu) in recus.iter().enumerate() {
            assert_eq!(recu.origin, "emetteur");
            assert_eq!(recu.session, "s1");
            let RelayBody::Event(event) = &recu.body else {
                panic!("le message {} est un événement : {:?}", index + 1, recu.body)
            };
            assert!(
                matches!(&event.kind, SessionEventKind::TextDelta { text, .. } if text == &format!("m{}", index + 1)),
                "l'ordre est conservé : {event:?}"
            );
        }
        let echo = collect(&mut recu_un, 3, Duration::from_secs(10)).await;
        assert_eq!(echo.len(), 3, "le premier reçoit aussi ses propres NOTIF");
        // `incoming()` ne peut être appelé qu'une fois : le second rend un canal déjà clos.
        let mut second = un.incoming();
        assert!(
            second.recv().await.is_none(),
            "un second incoming rend un récepteur déjà clos"
        );
    }

    /// Scenario « une charge trop grosse devient Missed » (PostgreSQL réel) : le récepteur
    /// voit `Missed` pour la même session, sans erreur d'envoi.
    #[tokio::test]
    #[ignore = "exige PostgreSQL (MIRYAD_TEST_PG_URL)"]
    async fn une_charge_trop_grosse_devient_missed_postgres() {
        let Some(url) = pg_url() else { return };
        let canal = canal_unique();
        let un = PgEventRelay::connect(&pg_db(&url, 2).await, config(&canal))
            .await
            .expect("relais émetteur");
        let deux = PgEventRelay::connect(&pg_db(&url, 2).await, config(&canal))
            .await
            .expect("relais récepteur");
        let mut recu = deux.incoming();
        un.publish(RelayMessage {
            origin: "emetteur".to_owned(),
            session: "s9".to_owned(),
            body: RelayBody::Event(SessionEvent {
                session: "s9".to_owned(),
                at: "2026-11-15T10:00:00.000Z".to_owned(),
                kind: SessionEventKind::TextDelta {
                    turn: 1,
                    text: "x".repeat(9_000),
                },
            }),
        });
        let recus = collect(&mut recu, 1, Duration::from_secs(10)).await;
        assert_eq!(recus.len(), 1, "le message remplacé arrive");
        assert_eq!(recus[0].session, "s9");
        assert!(
            matches!(recus[0].body, RelayBody::Missed),
            "le corps est Missed : {:?}",
            recus[0].body
        );
    }

    /// Scenario « un canal plein abandonne sans bloquer » (PostgreSQL réel) : la base ne
    /// répond plus (pool de deux connexions — l'écouteur en tient une, une transaction retient
    /// l'autre, la tâche d'envoi est bloquée à l'acquisition) ; 1 000 publications ne bloquent
    /// jamais et le récepteur n'en voit qu'une fraction.
    #[tokio::test]
    #[ignore = "exige PostgreSQL (MIRYAD_TEST_PG_URL)"]
    async fn un_canal_plein_abandonne_sans_bloquer() {
        let Some(url) = pg_url() else { return };
        let canal = canal_unique();
        let db_envoi = pg_db(&url, 2).await;
        let relais = PgEventRelay::connect(
            &db_envoi,
            PgRelayConfig {
                channel: canal.clone(),
                queue_capacity: 16,
                ..PgRelayConfig::default()
            },
        )
        .await
        .expect("relais (file de 16)");
        let recepteur = PgEventRelay::connect(&pg_db(&url, 2).await, config(&canal))
            .await
            .expect("relais récepteur");
        let mut recu = recepteur.incoming();
        let garde = db_envoi
            .begin()
            .await
            .expect("la seconde connexion du pool est retenue : la base ne répond plus");
        let started = std::time::Instant::now();
        for index in 0..1_000 {
            relais.publish(message("s1", &format!("m{index}")));
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "`publish` ne bloque jamais : {:?}",
            started.elapsed()
        );
        drop(garde);
        let recus = collect(&mut recu, 1_000, Duration::from_secs(10)).await;
        assert!(
            recus.len() < 1_000,
            "la file de 16 a abandonné le surplus : {} reçus",
            recus.len()
        );
    }

    /// Scenario « une charge illisible est ignorée » (PostgreSQL réel) : un `pg_notify` non
    /// JSON ne passe pas vers `incoming()` ; le message valide suivant arrive.
    #[tokio::test]
    #[ignore = "exige PostgreSQL (MIRYAD_TEST_PG_URL)"]
    async fn une_charge_illisible_est_ignoriee() {
        let Some(url) = pg_url() else { return };
        let canal = canal_unique();
        let sonde = pg_db(&url, 2).await;
        let relais = PgEventRelay::connect(&pg_db(&url, 2).await, config(&canal))
            .await
            .expect("relais");
        let mut recu = relais.incoming();
        sonde
            .execute_unprepared(&format!("SELECT pg_notify('{canal}', 'pas du json')"))
            .await
            .expect("notification illisible emise");
        let delai = tokio::time::timeout(Duration::from_millis(500), recu.recv()).await;
        assert!(
            delai.is_err(),
            "rien n'est poussé vers incoming() pour une charge illisible"
        );
        relais.publish(message("s1", "valide"));
        let recus = collect(&mut recu, 1, Duration::from_secs(10)).await;
        assert_eq!(recus.len(), 1, "le message valide suivant est reçu");
    }

    /// Scenario « la perte de l'écoute est signalée après reconnexion » (PostgreSQL réel) :
    /// la session PostgreSQL de l'écouteur est tuée côté serveur ; après la reconnexion
    /// (attente exponentielle plafonnée), `incoming()` rend le `Missed` global, puis les
    /// messages suivants arrivent normalement.
    #[tokio::test]
    #[ignore = "exige PostgreSQL (MIRYAD_TEST_PG_URL)"]
    async fn la_perte_de_lecoute_est_signalee_apres_reconnexion() {
        let Some(url) = pg_url() else { return };
        let canal = canal_unique();
        let premier = PgEventRelay::connect(&pg_db(&url, 2).await, config(&canal))
            .await
            .expect("premier relais");
        let deux = PgEventRelay::connect(&pg_db(&url, 2).await, config(&canal))
            .await
            .expect("second relais");
        let mut recu_deux = deux.incoming();
        let tueur = pg_db(&url, 2).await;
        tuelecoute(&tueur, &canal).await;
        let recus = collect(&mut recu_deux, 1, Duration::from_secs(15)).await;
        assert_eq!(recus.len(), 1, "le Missed global arrive après reconnexion");
        assert_eq!(recus[0].session, "*");
        assert!(
            matches!(recus[0].body, RelayBody::Missed),
            "corps Missed : {:?}",
            recus[0].body
        );
        premier.publish(message("s1", "apres"));
        let recus_apres = collect(&mut recu_deux, 1, Duration::from_secs(15)).await;
        assert_eq!(
            recus_apres.len(),
            1,
            "les messages suivants arrivent après la reconnexion"
        );
        assert_eq!(recus_apres[0].session, "s1");
        let RelayBody::Event(event) = &recus_apres[0].body else {
            panic!("le message suivant est un événement : {:?}", recus_apres[0].body)
        };
        assert!(
            matches!(&event.kind, SessionEventKind::TextDelta { text, .. } if text == "apres"),
            "{event:?}"
        );
    }

    /// Tue le backend dont la dernière requête est le `LISTEN` du canal (l'écouteur) — le
    /// canal vient de `canal_unique`, que des minuscules, chiffres et `_` : sûr à interpoler
    /// dans la sonde de test.
    async fn tuelecoute(db: &DatabaseConnection, canal: &str) {
        db.execute_unprepared(&format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE query LIKE 'LISTEN \"{canal}\"%'"
        ))
        .await
        .expect("tue lecoute du canal");
    }
}
