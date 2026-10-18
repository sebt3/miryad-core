//! Tests live du client Matrix contre un homeserver réel (`src/matrix/client.sdd` Tâche D,
//! `src/matrix/sync.sdd` Scenarios « test live »). Tous `#[ignore]` : lancés avec
//! `cargo test --features matrix --test matrix_live -- --ignored`, ils exigent les variables
//! d'environnement ci-dessous et échouent explicitement si l'une manque (pas de skip silencieux).
//!
//! - `MIRYAD_MATRIX_HOMESERVER` : URL du homeserver de développement.
//! - `MIRYAD_MATRIX_PASSPHRASE` : passphrase du store de chiffrement (jamais affichée).
//! - `MIRYAD_MATRIX_ROOM` : identifiant de la salle de test, forme `!x:y` (`send` n'accepte pas d'alias).
//! - `MIRYAD_MATRIX_SESSION` : JSON `{"user_id", "device_id", "access_token", "refresh_token"?}`
//!   (`SessionImport`). Les jetons ne sont jamais affichés, y compris en cas d'échec.
//!
//! Couverts ici : connexion par `import_session`, envoi idempotent par `txn_id`, ordre chronologique
//! de `recent_messages`, non-remise des messages du bot par `run_sync` puis arrêt propre.
//!
//! À écrire quand un second compte de test existe (scénarios à deux comptes ou deux processus,
//! non écrits à ce jour) :
//! - « un seul synchronisateur par compte » : deux processus appellent `run_sync` avec le même
//!   compte et deux `holder_name` ; un message d'un second compte n'est reçu que par un des deux.
//! - « un arrêt propre libère le bail » : le détenteur reçoit `shutdown` et rend `Ok(())` ; l'autre
//!   obtient le bail en moins de 10 secondes.
//! - « réception d'un message d'un autre compte » : un message émis par un second compte est remis
//!   au handler, avec son corps (aucun test ne prouve encore la boucle de remise positive).
//! - envoi depuis un réplica qui ne tient pas le bail, salle chiffrée (`client.sdd`, hypothèse
//!   retenue le 2026-10-06 : à éprouver avec deux processus).

// Famille panic/unwrap/indexation tolérée dans cette crate de test : en-tête d'exemption
// équivalent à celui de `src/lib.rs`, posé d'après le `Must` de `tooling.sdd` — une crate
// d'intégration n'hérite pas des attributs de la librairie. Groupes `pedantic` et `cargo`
// restent `deny` sous `cfg(test)`.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::dbg_macro,
        clippy::todo,
        clippy::unimplemented,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing,
        clippy::unwrap_in_result,
        clippy::panic_in_result_fn
    )
)]
#![cfg(feature = "matrix")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use miryad_core::matrix::{
    MatrixClient, MatrixConfig, MatrixEventHandler, MatrixMessage, MessageKind, SessionImport,
};
use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;

/// Variables requises par les tests live, listées dans chaque message d'erreur.
const REQUIRED_ENV: [&str; 4] = [
    "MIRYAD_MATRIX_HOMESERVER",
    "MIRYAD_MATRIX_PASSPHRASE",
    "MIRYAD_MATRIX_ROOM",
    "MIRYAD_MATRIX_SESSION",
];

/// Délai laissé à la boucle de sync pour recevoir (ou non) le message envoyé par le bot lui-même.
const OWN_MESSAGE_WAIT: Duration = Duration::from_secs(8);

/// Paramètres lus dans l'environnement. `Debug` absent : la session contient des jetons.
struct Live {
    homeserver: String,
    passphrase: String,
    room: String,
    session: SessionImport,
    suffix: String,
}

/// Décodage de `MIRYAD_MATRIX_SESSION`. Le message d'erreur ne reprend jamais la valeur reçue.
#[derive(serde::Deserialize)]
struct SessionJson {
    user_id: String,
    device_id: String,
    access_token: String,
    refresh_token: Option<String>,
}

struct StoreMigrator;

impl MigratorTrait for StoreMigrator {
    fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        miryad_core::matrix::store::migrations()
    }
}

/// Handler enregistreur : compte les messages remis, sans jamais en afficher le contenu.
struct Recorder {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl MatrixEventHandler for Recorder {
    async fn on_message(&self, _message: MatrixMessage) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

/// Lit les quatre variables ; échoue en nommant toutes celles qui manquent.
fn live_env() -> Live {
    let missing: Vec<&str> = REQUIRED_ENV
        .iter()
        .copied()
        .filter(|name| !std::env::var(name).is_ok_and(|value| !value.is_empty()))
        .collect();
    assert!(
        missing.is_empty(),
        "variables d'environnement absentes : {}. Variables requises pour les tests live : {}",
        missing.join(", "),
        REQUIRED_ENV.join(", ")
    );
    let session_raw = std::env::var("MIRYAD_MATRIX_SESSION").unwrap_or_default();
    let parsed: SessionJson = serde_json::from_str(&session_raw).unwrap_or_else(|_| {
        panic!(
            "MIRYAD_MATRIX_SESSION : JSON invalide, attendu {{\"user_id\", \"device_id\", \"access_token\", \"refresh_token\"?}}"
        )
    });
    Live {
        homeserver: std::env::var("MIRYAD_MATRIX_HOMESERVER").unwrap_or_default(),
        passphrase: std::env::var("MIRYAD_MATRIX_PASSPHRASE").unwrap_or_default(),
        room: std::env::var("MIRYAD_MATRIX_ROOM").unwrap_or_default(),
        session: SessionImport {
            user_id: parsed.user_id,
            device_id: parsed.device_id,
            access_token: parsed.access_token,
            refresh_token: parsed.refresh_token,
        },
        suffix: unique_suffix(),
    }
}

/// Suffixe unique par exécution (processus + horodatage) : corps et `txn_id` ne collisionnent pas
/// avec ceux d'une exécution précédente.
fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

/// Base SQLite mémoire migrée, une seule connexion (sinon chaque connexion verrait sa base).
async fn fresh_db() -> DatabaseConnection {
    let mut opts = ConnectOptions::new("sqlite::memory:");
    opts.max_connections(1).min_connections(1);
    let db = Database::connect(opts)
        .await
        .expect("base SQLite mémoire ouverte");
    StoreMigrator::up(&db, None)
        .await
        .expect("migrations du store Matrix appliquées");
    db
}

/// Client connecté par `import_session` sur une base neuve.
async fn connect(live: &Live) -> MatrixClient {
    let db = fresh_db().await;
    let config = MatrixConfig::new(
        &live.homeserver,
        &live.passphrase,
        format!("live-test-{}", live.suffix),
    );
    MatrixClient::import_session(db, config, live.session.clone())
        .await
        .expect("import_session : connexion au homeserver de test")
}

#[ignore = "nécessite un homeserver Matrix réel : voir les variables MIRYAD_MATRIX_*"]
#[tokio::test]
async fn import_session_connects_bot() {
    let live = live_env();
    let client = connect(&live).await;
    let user_id = client.user_id();
    assert!(!user_id.is_empty(), "user_id vide après import_session");
    assert_eq!(
        user_id, live.session.user_id,
        "user_id du client différent de celui de la session importée"
    );
}

#[ignore = "nécessite un homeserver Matrix réel : voir les variables MIRYAD_MATRIX_*"]
#[tokio::test]
async fn send_with_same_txn_id_is_idempotent() {
    let live = live_env();
    let client = connect(&live).await;
    let room = client
        .join_room(&live.room)
        .await
        .expect("join_room de la salle de test");
    let body = format!("miryad live test idempotent {}", live.suffix);
    let txn = format!("live-idem-{}", live.suffix);

    let first = client
        .send(&room, &body, None, MessageKind::Text, Some(&txn))
        .await
        .expect("premier envoi");
    let second = client
        .send(&room, &body, None, MessageKind::Text, Some(&txn))
        .await
        .expect("second envoi avec le même txn_id");
    assert_eq!(first, second, "même txn_id : même event_id attendu");

    let messages = client
        .recent_messages(&room, 50)
        .await
        .expect("recent_messages de la salle de test");
    let copies = messages.iter().filter(|message| message.body == body).count();
    assert_eq!(
        copies, 1,
        "un seul message doit porter le corps unique de ce test"
    );
}

#[ignore = "nécessite un homeserver Matrix réel : voir les variables MIRYAD_MATRIX_*"]
#[tokio::test]
async fn recent_messages_is_chronological() {
    let live = live_env();
    let client = connect(&live).await;
    let room = client
        .join_room(&live.room)
        .await
        .expect("join_room de la salle de test");
    let first_body = format!("miryad live test ordre a {}", live.suffix);
    let second_body = format!("miryad live test ordre b {}", live.suffix);

    client
        .send(&room, &first_body, None, MessageKind::Text, None)
        .await
        .expect("envoi du premier message");
    client
        .send(&room, &second_body, None, MessageKind::Text, None)
        .await
        .expect("envoi du second message");

    let messages = client
        .recent_messages(&room, 50)
        .await
        .expect("recent_messages de la salle de test");
    let position_first = messages
        .iter()
        .position(|message| message.body == first_body)
        .expect("premier message présent dans recent_messages");
    let position_second = messages
        .iter()
        .position(|message| message.body == second_body)
        .expect("second message présent dans recent_messages");
    assert!(
        position_first < position_second,
        "ordre croissant attendu : le premier envoi doit précéder le second"
    );
    assert!(
        messages
            .windows(2)
            .all(|pair| pair[0].timestamp_ms <= pair[1].timestamp_ms),
        "horodatages non croissants dans recent_messages"
    );
}

#[ignore = "nécessite un homeserver Matrix réel : voir les variables MIRYAD_MATRIX_*"]
#[tokio::test]
async fn run_sync_never_delivers_own_messages_and_stops_cleanly() {
    let live = live_env();
    let client = connect(&live).await;
    let room = client
        .join_room(&live.room)
        .await
        .expect("join_room de la salle de test");

    let calls = Arc::new(AtomicUsize::new(0));
    let handler: Arc<dyn MatrixEventHandler> = Arc::new(Recorder {
        calls: Arc::clone(&calls),
    });
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let runner = client.clone();
    let task = tokio::spawn(async move {
        runner
            .run_sync(handler, async move {
                let _ = stop_rx.await;
            })
            .await
    });

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        !task.is_finished(),
        "run_sync s'est terminé avant shutdown (bail refusé ou erreur de sync)"
    );

    let body = format!("miryad live test bot {}", live.suffix);
    client
        .send(&room, &body, None, MessageKind::Text, None)
        .await
        .expect("envoi du message du bot");
    tokio::time::sleep(OWN_MESSAGE_WAIT).await;

    assert!(
        !task.is_finished(),
        "run_sync s'est terminé pendant l'attente du message du bot"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "le handler ne doit jamais recevoir un message du bot lui-même"
    );

    let _ = stop_tx.send(());
    let result = task.await.expect("tâche run_sync jointe");
    result.expect("run_sync doit rendre Ok(()) après shutdown");
}
