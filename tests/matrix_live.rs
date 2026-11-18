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
//! de `recent_messages`, non-remise des messages du bot par `run_sync` puis arrêt propre ; et
//! (tâche D bis, issue #87) relation de fil relue par `recent_messages`, réponse, édition et rejeu
//! du `txn_id` sur chacun, mesure du comportement de Palpo face à une relation vers un événement
//! inconnu (`matrix#OI-7`, point (b)).
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
//!
//! Banc interactif `device_code_login_then_echo` (`#[ignore]`, un humain est requis) : variables
//! `MIRYAD_MATRIX_HOMESERVER`, `MIRYAD_MATRIX_PASSPHRASE`, `MIRYAD_MATRIX_INVITER` (identifiant
//! `@user:server` autorisé à écrire au bot), optionnelles `MIRYAD_MATRIX_CA` (**contenu** PEM, jamais
//! un chemin) et `MIRYAD_MATRIX_ECHO_TIMEOUT_SECS` (défaut 600). Le test affiche
//! `DEVICE_CODE_PROMPT url=… code=…` puis `ECHO_READY user_id=…` ; il répond au premier message de
//! l'invité par `écho de <invité> : <corps>`. Lancement :
//! `cargo test --no-default-features --features matrix --test matrix_live device_code_login_then_echo -- --ignored --nocapture`.

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
// Profondeur de layout des futures asynchrones de cette crate, mesurée : 256 (DSD `matrix` DEC-4).
#![recursion_limit = "256"]

use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use miryad_core::matrix::{
    MatrixClient, MatrixConfig, MatrixError, MatrixEventHandler, MatrixMessage, MessageKind, MessageRelation,
    SessionImport,
};
use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;
use tokio::sync::oneshot;

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
        .send(&room, &body, None, MessageKind::Text, None, Some(&txn))
        .await
        .expect("premier envoi");
    let second = client
        .send(&room, &body, None, MessageKind::Text, None, Some(&txn))
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
        .send(&room, &first_body, None, MessageKind::Text, None, None)
        .await
        .expect("envoi du premier message");
    client
        .send(&room, &second_body, None, MessageKind::Text, None, None)
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
        .send(&room, &body, None, MessageKind::Text, None, None)
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

// ---------------------------------------------------------------------------
// Tâche D bis (2026-11-16, issue #87) : relation de fil, réponse, édition — chacun avec
// rejeu immédiat du même `txn_id` (aucun doublon) — et mesure du comportement de Palpo face
// à une relation vers un événement inconnu (`matrix#OI-7`, point (b)). Le Scenario
// « relation, réponse, édition et rejeu idempotent » de `src/matrix/client.sdd`, dont le
// volet « `edit` d'un message d'un autre compte rend NotAuthor » attend le second compte
// listé en tête du fichier.
// ---------------------------------------------------------------------------

/// Nom de serveur de la session importée, pour construire un `event_id` bien formé mais
/// nécessairement inexistant (`$…:<serveur>`).
fn server_name(live: &Live) -> String {
    live.session
        .user_id
        .split_once(':')
        .map_or_else(|| "localhost".to_owned(), |(_, server)| server.to_owned())
}

/// Cas (a) + (d) : message envoyé dans un fil (`MessageRelation::Thread`, `latest = None`) avec
/// un `txn_id` rejoué deux fois — `recent_messages` doit le relire une seule fois, avec
/// `thread_root` = la racine et le repli `in_reply_to` vers cette même racine (Scenario
/// « relation, réponse, édition et rejeu idempotent », `matrix#FR-9` / `matrix#DEC-3`).
#[ignore = "nécessite un homeserver Matrix réel : voir les variables MIRYAD_MATRIX_*"]
#[tokio::test]
async fn send_in_thread_relreads_thread_root() {
    let live = live_env();
    let client = connect(&live).await;
    let room = client
        .join_room(&live.room)
        .await
        .expect("join_room de la salle de test");

    let root_body = format!("miryad live test fil racine {}", live.suffix);
    let root_id = client
        .send(&room, &root_body, None, MessageKind::Text, None, None)
        .await
        .expect("envoi du message racine");

    let thread_body = format!("miryad live test fil réponse {}", live.suffix);
    let relation = MessageRelation::Thread {
        root: root_id.clone(),
        latest: None,
    };
    let txn = format!("live-thread-{}", live.suffix);
    let first = client
        .send(
            &room,
            &thread_body,
            None,
            MessageKind::Text,
            Some(&relation),
            Some(&txn),
        )
        .await
        .expect("envoi dans le fil");
    let second = client
        .send(
            &room,
            &thread_body,
            None,
            MessageKind::Text,
            Some(&relation),
            Some(&txn),
        )
        .await
        .expect("rejeu du même txn_id dans le fil");
    assert_eq!(first, second, "même txn_id : même event_id attendu");

    let messages = client
        .recent_messages(&room, 50)
        .await
        .expect("recent_messages de la salle de test");
    let copies: Vec<&MatrixMessage> = messages
        .iter()
        .filter(|message| message.body == thread_body)
        .collect();
    assert_eq!(copies.len(), 1, "aucun doublon attendu pour le même txn_id");
    let message = copies
        .first()
        .copied()
        .expect("message de fil présent dans recent_messages");
    assert_eq!(
        message.thread_root.as_deref(),
        Some(root_id.as_str()),
        "thread_root doit porter la racine du fil (m.thread relu par recent_messages)"
    );
    assert_eq!(
        message.in_reply_to.as_deref(),
        Some(root_id.as_str()),
        "repli m.in_reply_to vers la racine attendu (latest = None, is_falling_back)"
    );

    let root_message = messages
        .iter()
        .find(|message| message.body == root_body)
        .expect("message racine présent dans recent_messages");
    assert_eq!(root_message.thread_root, None, "la racine est un message à plat");
    assert_eq!(root_message.in_reply_to, None, "la racine ne répond à rien");
}

/// Cas (b) + (d) : réponse (`MessageRelation::Reply`) vers un premier message, `txn_id` rejoué
/// deux fois — `recent_messages` doit la relire une seule fois avec `in_reply_to` = l'événement
/// visé et `thread_root = None` (arbitré le 2026-11-16 : `Reply` ne forme pas de fil).
#[ignore = "nécessite un homeserver Matrix réel : voir les variables MIRYAD_MATRIX_*"]
#[tokio::test]
async fn send_reply_relreads_in_reply_to() {
    let live = live_env();
    let client = connect(&live).await;
    let room = client
        .join_room(&live.room)
        .await
        .expect("join_room de la salle de test");

    let target_body = format!("miryad live test réponse cible {}", live.suffix);
    let target_id = client
        .send(&room, &target_body, None, MessageKind::Text, None, None)
        .await
        .expect("envoi du message visé");

    let reply_body = format!("miryad live test réponse {}", live.suffix);
    let relation = MessageRelation::Reply {
        to: target_id.clone(),
    };
    let txn = format!("live-reply-{}", live.suffix);
    let first = client
        .send(
            &room,
            &reply_body,
            None,
            MessageKind::Text,
            Some(&relation),
            Some(&txn),
        )
        .await
        .expect("envoi de la réponse");
    let second = client
        .send(
            &room,
            &reply_body,
            None,
            MessageKind::Text,
            Some(&relation),
            Some(&txn),
        )
        .await
        .expect("rejeu du même txn_id pour la réponse");
    assert_eq!(first, second, "même txn_id : même event_id attendu");

    let messages = client
        .recent_messages(&room, 50)
        .await
        .expect("recent_messages de la salle de test");
    let copies: Vec<&MatrixMessage> = messages
        .iter()
        .filter(|message| message.body == reply_body)
        .collect();
    assert_eq!(copies.len(), 1, "aucun doublon attendu pour le même txn_id");
    let message = copies
        .first()
        .copied()
        .expect("réponse présente dans recent_messages");
    assert_eq!(
        message.in_reply_to.as_deref(),
        Some(target_id.as_str()),
        "in_reply_to doit porter l'événement visé (m.in_reply_to relu par recent_messages)"
    );
    assert_eq!(
        message.thread_root, None,
        "une Reply ne forme pas de fil (EnforceThread::Unthreaded, arbitré le 2026-11-16)"
    );
}

/// Cas (c) + (d) : `MatrixClient::edit` avec un `txn_id` rejoué deux fois — le corps remplacé
/// doit être visible dans `recent_messages`, une seule fois, et `edit` doit rendre l'`event_id`
/// de l'événement de **remplacement** (différent de l'original, `matrix#FR-10`). L'historique
/// étant brut (les `m.replace` ne sont pas filtrés ni agrégés), le remplacement apparaît avec son
/// corps de repli, préfixé ou non par la convention « ` * ` » : d'où `contains`.
#[ignore = "nécessite un homeserver Matrix réel : voir les variables MIRYAD_MATRIX_*"]
#[tokio::test]
async fn edit_body_visible_in_recent_messages() {
    let live = live_env();
    let client = connect(&live).await;
    let room = client
        .join_room(&live.room)
        .await
        .expect("join_room de la salle de test");

    let original_body = format!("miryad live test édition avant {}", live.suffix);
    let original_id = client
        .send(&room, &original_body, None, MessageKind::Text, None, None)
        .await
        .expect("envoi du message à éditer");

    let edited_body = format!("miryad live test édition après {}", live.suffix);
    let txn = format!("live-edit-{}", live.suffix);
    let first = client
        .edit(
            &room,
            &original_id,
            &edited_body,
            None,
            MessageKind::Text,
            Some(&txn),
        )
        .await
        .expect("édition du message");
    let second = client
        .edit(
            &room,
            &original_id,
            &edited_body,
            None,
            MessageKind::Text,
            Some(&txn),
        )
        .await
        .expect("rejeu du même txn_id pour l'édition");
    assert_eq!(
        first, second,
        "même txn_id : même event_id de remplacement attendu"
    );
    assert_ne!(
        first, original_id,
        "edit rend l'event_id de l'événement de remplacement, pas celui de l'original"
    );

    let messages = client
        .recent_messages(&room, 50)
        .await
        .expect("recent_messages de la salle de test");
    let editions: Vec<&MatrixMessage> = messages
        .iter()
        .filter(|message| message.body.contains(&edited_body))
        .collect();
    assert_eq!(
        editions.len(),
        1,
        "le corps édité doit être visible une seule fois dans recent_messages (aucun doublon de \
         l'édition rejouée)"
    );
}

/// Cas (e) — **mesure du comportement de Palpo** pour une relation vers un événement inconnu
/// (`matrix#OI-7`, point (b)) :
///
/// - `Reply` : la spec promet `Err(MatrixError::EventNotFound)` (`from_fetch_error` : 404 du
///   `make_reply_event` traduit, toute autre erreur non retryable devenant `Sdk`). **MESURE À
///   CONSIGNER dans matrix#OI-7(b)** : vérifier que Palpo répond bien 404 sur
///   `/rooms/{id}/event/{unknown}` — un 403 ou un 203 ferait échouer ce test et se consignerait.
/// - `Thread` : construit à la main (`apply_thread`, aucune récupération) — la spec ne promet
///   donc aucune erreur côté client ; c'est précisément le point à mesurer : le serveur refuse
///   ou accepte une relation `m.thread` vers une racine inconnue. Le résultat est consigné sur
///   stdout (`MESURE_OI7B`), sans assertion sur le fond.
#[ignore = "nécessite un homeserver Matrix réel : voir les variables MIRYAD_MATRIX_*"]
#[tokio::test]
async fn relation_to_unknown_event_measures_palpo() {
    let live = live_env();
    let client = connect(&live).await;
    let room = client
        .join_room(&live.room)
        .await
        .expect("join_room de la salle de test");
    let unknown = format!("$miryad-inexistant-{}:{}", live.suffix, server_name(&live));

    let reply = client
        .send(
            &room,
            &format!("miryad live test inconnu reply {}", live.suffix),
            None,
            MessageKind::Text,
            Some(&MessageRelation::Reply { to: unknown.clone() }),
            None,
        )
        .await;
    assert!(
        matches!(reply, Err(MatrixError::EventNotFound(_))),
        "une Reply vers un événement inconnu doit rendre EventNotFound \
         (matrix#FR-13, from_fetch_error) — obtenu {reply:?}"
    );

    let thread = client
        .send(
            &room,
            &format!("miryad live test inconnu thread {}", live.suffix),
            None,
            MessageKind::Text,
            Some(&MessageRelation::Thread {
                root: unknown,
                latest: None,
            }),
            None,
        )
        .await;
    match &thread {
        Ok(event_id) => println!("MESURE_OI7B thread_root_inconnu: accepté, event_id={event_id}"),
        Err(err) => println!("MESURE_OI7B thread_root_inconnu: refusé : {err}"),
    }
    assert!(
        !matches!(thread, Err(MatrixError::InvalidId(_))),
        "un identifiant bien formé ne doit pas être refusé en validation — obtenu {thread:?}"
    );
}

/// Variables du banc interactif, listées dans le message d'erreur si l'une manque.
const INTERACTIVE_REQUIRED_ENV: [&str; 3] = [
    "MIRYAD_MATRIX_HOMESERVER",
    "MIRYAD_MATRIX_PASSPHRASE",
    "MIRYAD_MATRIX_INVITER",
];

/// Délai d'attente de l'écho par défaut, en secondes.
const DEFAULT_ECHO_TIMEOUT_SECS: u64 = 600;

/// Délai laissé à `run_sync` pour démarrer (sync initial) avant d'annoncer `ECHO_READY`.
const SYNC_STARTUP_DELAY: Duration = Duration::from_secs(3);

/// Paramètres du banc interactif. `Debug` absent : la passphrase y figure.
struct Interactive {
    homeserver: String,
    passphrase: String,
    inviter: String,
    ca_cert: Option<String>,
    timeout: Duration,
}

/// Lit les variables du banc interactif ; échoue en nommant celles qui manquent.
fn interactive_env() -> Interactive {
    let missing: Vec<&str> = INTERACTIVE_REQUIRED_ENV
        .iter()
        .copied()
        .filter(|name| !std::env::var(name).is_ok_and(|value| !value.is_empty()))
        .collect();
    assert!(
        missing.is_empty(),
        "variables d'environnement absentes : {}. Variables requises pour device_code_login_then_echo : {}",
        missing.join(", "),
        INTERACTIVE_REQUIRED_ENV.join(", ")
    );
    // Contenu PEM attendu : un chemin de fichier est refusé sans écho de sa valeur.
    let ca_cert = match std::env::var("MIRYAD_MATRIX_CA") {
        Ok(pem) if !pem.trim().is_empty() => {
            assert!(
                pem.contains("-----BEGIN CERTIFICATE-----"),
                "MIRYAD_MATRIX_CA : contenu PEM attendu (le certificat lui-même, pas un chemin de fichier)"
            );
            Some(pem)
        }
        _ => None,
    };
    let timeout_secs = match std::env::var("MIRYAD_MATRIX_ECHO_TIMEOUT_SECS") {
        Ok(raw) if !raw.trim().is_empty() => raw.trim().parse::<u64>().unwrap_or_else(|_| {
            panic!("MIRYAD_MATRIX_ECHO_TIMEOUT_SECS : nombre entier de secondes attendu")
        }),
        _ => DEFAULT_ECHO_TIMEOUT_SECS,
    };
    Interactive {
        homeserver: std::env::var("MIRYAD_MATRIX_HOMESERVER").unwrap_or_default(),
        passphrase: std::env::var("MIRYAD_MATRIX_PASSPHRASE").unwrap_or_default(),
        inviter: std::env::var("MIRYAD_MATRIX_INVITER").unwrap_or_default(),
        ca_cert,
        timeout: Duration::from_secs(timeout_secs),
    }
}

/// Répond `écho de <invité> : <corps>` au premier message de l'invité, puis signale au test
/// l'identifiant de l'écho (ou l'erreur d'envoi). Les messages suivants sont ignorés.
struct EchoOnce {
    inviter: String,
    client: MatrixClient,
    signal: Mutex<Option<oneshot::Sender<Result<String, String>>>>,
}

#[async_trait::async_trait]
impl MatrixEventHandler for EchoOnce {
    async fn on_message(&self, message: MatrixMessage) {
        if message.sender != self.inviter {
            return;
        }
        // Le signal est pris avant l'envoi : une seule réponse, même si la boucle remet deux fois.
        let taken = match self.signal.lock() {
            Ok(mut slot) => slot.take(),
            Err(_) => None,
        };
        let Some(tx) = taken else {
            return;
        };
        let body = format!("écho de {} : {}", self.inviter, message.body);
        let outcome = self
            .client
            .send(
                &message.room_id,
                &body,
                None,
                MessageKind::Text,
                None,
                Some(&message.event_id),
            )
            .await
            .map_err(|err| format!("envoi de l'écho échoué : {err}"));
        let _ = tx.send(outcome);
    }
}

#[ignore = "interactif : exige un humain (device code) et un homeserver Matrix, voir MIRYAD_MATRIX_*"]
#[tokio::test]
async fn device_code_login_then_echo() {
    let env = interactive_env();
    let suffix = unique_suffix();
    let db = fresh_db().await;
    let mut config = MatrixConfig::new(&env.homeserver, &env.passphrase, format!("echo-test-{suffix}"));
    config.auto_join_from = vec![env.inviter.clone()];
    config.ca_cert = env.ca_cert.clone();

    let client = MatrixClient::login_with_device_code(db, config, |prompt| {
        println!(
            "DEVICE_CODE_PROMPT url={} code={}",
            prompt.verification_uri, prompt.user_code
        );
        let _ = std::io::stdout().flush();
    })
    .await
    .expect("login_with_device_code : connexion du bot par device code");
    let bot_id = client.user_id();

    let (echo_tx, mut echo_rx) = oneshot::channel::<Result<String, String>>();
    let handler: Arc<dyn MatrixEventHandler> = Arc::new(EchoOnce {
        inviter: env.inviter.clone(),
        client: client.clone(),
        signal: Mutex::new(Some(echo_tx)),
    });
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let runner = client.clone();
    let mut task = tokio::spawn(async move {
        runner
            .run_sync(handler, async move {
                let _ = stop_rx.await;
            })
            .await
    });

    // Si run_sync meurt pendant le démarrage, l'échec est explicite plutôt qu'un ECHO_READY trompeur.
    tokio::select! {
        done = &mut task => panic!("run_sync terminé au démarrage : {done:?}"),
        () = tokio::time::sleep(SYNC_STARTUP_DELAY) => {}
    }
    println!("ECHO_READY user_id={bot_id}");
    let _ = std::io::stdout().flush();

    let mut early = None;
    let echo: Result<String, String> = tokio::select! {
        waited = tokio::time::timeout(env.timeout, &mut echo_rx) => match waited {
            Err(_elapsed) => Err(format!(
                "aucun message de {} reçu dans les {} s",
                env.inviter,
                env.timeout.as_secs()
            )),
            Ok(Err(_closed)) => Err("handler abandonné sans signal".to_owned()),
            Ok(Ok(outcome)) => outcome,
        },
        done = &mut task => {
            early = Some(done);
            Err(format!("run_sync terminé avant l'écho de {}", env.inviter))
        }
    };

    // Dans tous les cas, run_sync est arrêté proprement avant de juger le résultat.
    let sync_outcome = if let Some(done) = early {
        done.expect("tâche run_sync jointe")
    } else {
        let _ = stop_tx.send(());
        task.await.expect("tâche run_sync jointe")
    };
    let event_id = match echo {
        Ok(event_id) => event_id,
        Err(reason) => panic!("{reason} ; run_sync a rendu : {sync_outcome:?}"),
    };
    assert!(!event_id.is_empty(), "identifiant de l'écho vide");
    sync_outcome.expect("run_sync doit rendre Ok(()) après shutdown");
    println!("ECHO_OK event_id={event_id}");
}

/// Banc de persistance sur PostgreSQL : une session de bot doit survivre à un redémarrage **et** à
/// l'expiration du jeton d'accès (300 s côté MAS), c'est-à-dire que la rotation du jeton de
/// rafraîchissement est écrite dans la base de l'application. Se lance en trois temps, sans état
/// local (tout vit dans le schéma PostgreSQL, `MIRYAD_MATRIX_PG_SCHEMA`, défaut `miryad_live`) :
/// 1. `MIRYAD_MATRIX_LOGIN=1` : login par device code (invite `DEVICE_CODE_PROMPT`), puis synchronisation ;
/// 2. plus de 5 minutes plus tard, sans `MIRYAD_MATRIX_LOGIN` : `connect` restaure la session, le jeton
///    d'accès a expiré, le client doit le rafraîchir tout seul ;
/// 3. relancer une troisième fois : ne réussit que si la rotation du 2 a été persistée.
///
/// Variables : `MIRYAD_MATRIX_HOMESERVER`, `MIRYAD_MATRIX_PASSPHRASE`, `MIRYAD_MATRIX_INVITER`,
/// `MIRYAD_MATRIX_PG_URL` (porte un mot de passe, jamais affichée), `MIRYAD_MATRIX_CA` (PEM, optionnelle),
/// `MIRYAD_MATRIX_SYNC_SECS` (durée de la synchronisation, défaut 20).
#[ignore = "interactif : nécessite un homeserver Matrix réel et un PostgreSQL (MIRYAD_MATRIX_PG_URL)"]
#[tokio::test]
async fn persisted_session_survives_token_expiry() {
    const NEEDED: [&str; 4] = [
        "MIRYAD_MATRIX_HOMESERVER",
        "MIRYAD_MATRIX_PASSPHRASE",
        "MIRYAD_MATRIX_INVITER",
        "MIRYAD_MATRIX_PG_URL",
    ];
    let missing: Vec<&str> = NEEDED
        .iter()
        .copied()
        .filter(|name| !std::env::var(name).is_ok_and(|value| !value.is_empty()))
        .collect();
    assert!(
        missing.is_empty(),
        "variables d'environnement absentes : {}. Variables requises pour persisted_session_survives_token_expiry : {}",
        missing.join(", "),
        NEEDED.join(", ")
    );
    let env = |name: &str| std::env::var(name).unwrap_or_default();
    let schema = std::env::var("MIRYAD_MATRIX_PG_SCHEMA")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "miryad_live".to_owned());
    assert!(
        schema.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "MIRYAD_MATRIX_PG_SCHEMA : lettres, chiffres et _ seulement"
    );
    let sync_secs: u64 = std::env::var("MIRYAD_MATRIX_SYNC_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .unwrap_or(20);

    // Base persistante : schéma créé une fois, migrations idempotentes (les appliquées sont sautées).
    let url = env("MIRYAD_MATRIX_PG_URL");
    let boot = Database::connect(url.as_str())
        .await
        .map_err(|err| err.to_string().replace(url.as_str(), "<url>"))
        .expect("connexion PostgreSQL d'amorçage");
    sea_orm::ConnectionTrait::execute_unprepared(&boot, &format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""))
        .await
        .expect("création du schéma de persistance");
    boot.close().await.expect("fermeture de l'amorçage");
    let mut opts = ConnectOptions::new(url.clone());
    opts.set_schema_search_path(schema);
    let db = Database::connect(opts)
        .await
        .map_err(|err| err.to_string().replace(url.as_str(), "<url>"))
        .expect("pool PostgreSQL sur le schéma de persistance");
    StoreMigrator::up(&db, None)
        .await
        .expect("migrations du store Matrix appliquées");

    let mut config = MatrixConfig::new(
        env("MIRYAD_MATRIX_HOMESERVER"),
        env("MIRYAD_MATRIX_PASSPHRASE"),
        "live-persist",
    );
    config.auto_join_from = vec![env("MIRYAD_MATRIX_INVITER")];
    config.ca_cert = std::env::var("MIRYAD_MATRIX_CA")
        .ok()
        .filter(|pem| !pem.trim().is_empty());

    let client = match MatrixClient::connect(db.clone(), config.clone()).await {
        Ok(client) => {
            println!("SESSION_RESTORED");
            client
        }
        Err(miryad_core::matrix::MatrixError::NotLoggedIn) if env("MIRYAD_MATRIX_LOGIN") == "1" => {
            MatrixClient::login_with_device_code(db, config, |prompt| {
                println!(
                    "DEVICE_CODE_PROMPT url={} code={}",
                    prompt.verification_uri, prompt.user_code
                );
                std::io::stdout().flush().expect("flush stdout");
            })
            .await
            .expect("login par device code")
        }
        Err(err) => panic!("connect a échoué : {err} (première fois : relancer avec MIRYAD_MATRIX_LOGIN=1)"),
    };

    let calls = Arc::new(AtomicUsize::new(0));
    let handler: Arc<dyn MatrixEventHandler> = Arc::new(Recorder {
        calls: Arc::clone(&calls),
    });
    client
        .run_sync(handler, tokio::time::sleep(Duration::from_secs(sync_secs)))
        .await
        .expect("run_sync doit rendre Ok(()) : la session restaurée doit rester valide");
    println!("RECONNECT_OK user_id={}", client.user_id());
}
