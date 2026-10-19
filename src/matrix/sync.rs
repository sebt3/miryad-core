//! Partie pure de la synchronisation Matrix (`src/matrix/sync.sdd`) : types remis à l'application,
//! filtrage des messages, conversion, règle d'auto-join, durée de bail et backoff.
//!
//! Rien ici ne touche à `matrix-sdk` : les fonctions prennent des champs simples, pour être testées
//! sans homeserver. La boucle `run_sync` (étape suivante, `./sync.sdd`) s'appuie sur elles.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use matrix_sdk::config::{SyncSettings, SyncToken};
use matrix_sdk::deserialized_responses::EncryptionInfo;
use matrix_sdk::event_handler::EventHandlerDropGuard;
use matrix_sdk::ruma::api::error::ErrorKind;
use matrix_sdk::ruma::events::room::encrypted::OriginalSyncRoomEncryptedEvent;
use matrix_sdk::ruma::events::room::member::{MembershipState, StrippedRoomMemberEvent};
use matrix_sdk::ruma::events::room::message::{MessageType, Relation, SyncRoomMessageEvent};
use matrix_sdk::{Client, LoopCtrl, Room};
use matrix_sdk_crypto::store::CryptoStore;

use crate::matrix::client::MatrixClient;
use crate::matrix::error::{MatrixError, from_sdk};

/// Clé du bail de synchronisation dans le store (`src/matrix/sync.sdd`, étape 1).
const SYNC_LEASE_KEY: &str = "miryad.matrix.sync";
/// Attente entre deux tentatives d'acquisition du bail (`src/matrix/sync.sdd`, étape 1).
const LEASE_RETRY: Duration = Duration::from_secs(5);
/// Délais entre les appels de jonction à une invitation (`src/matrix/sync.sdd`, Invitations).
const JOIN_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// Message texte remis à l'application (`src/matrix/sync.sdd`). Identifiants en `String` : aucun
/// type de `matrix-sdk` ni de `ruma` n'entre dans l'API publique.
#[derive(Debug, Clone, PartialEq)]
pub struct MatrixMessage {
    /// Identifiant de la salle, ex. `!room:server`.
    pub room_id: String,
    /// Identifiant de l'événement : clé naturelle d'idempotence côté handler.
    pub event_id: String,
    /// Expéditeur, ex. `@alice:server`.
    pub sender: String,
    /// Corps en texte brut.
    pub body: String,
    /// Corps HTML, quand l'événement en porte un.
    pub formatted_body: Option<String>,
    /// `m.text` ou `m.emote` en remise en direct.
    pub msgtype: String,
    /// Horodatage serveur, en millisecondes depuis l'époque Unix.
    pub timestamp_ms: u64,
    /// Vrai si le message a été déchiffré (salle chiffrée).
    pub encrypted: bool,
}

/// Réception des messages texte par l'application (`src/matrix/sync.sdd`). Le harnais de chatbot
/// l'implémente ; le handler gère ses propres erreurs et doit être idempotent (un message peut être
/// remis deux fois après une bascule de bail).
#[async_trait::async_trait]
pub trait MatrixEventHandler: Send + Sync + 'static {
    /// Reçoit un message nouveau. Aucune erreur n'est remontée à la boucle de synchronisation.
    async fn on_message(&self, message: MatrixMessage);
}

/// Entrées de [`to_message`] regroupées en un seul argument (`src/matrix/sync.sdd`) : huit
/// paramètres nus dépasseraient `clippy::too_many_arguments`. Réutilisée par `recent_messages`.
#[derive(Clone, Copy)]
pub(crate) struct MessageParts<'a> {
    /// Identifiant de la salle.
    pub(crate) room_id: &'a str,
    /// Identifiant de l'événement.
    pub(crate) event_id: &'a str,
    /// Expéditeur.
    pub(crate) sender: &'a str,
    /// Type de message (`m.text`, `m.emote`, `m.notice`).
    pub(crate) msgtype: &'a str,
    /// Corps en texte brut.
    pub(crate) body: &'a str,
    /// Corps HTML, s'il existe.
    pub(crate) formatted_body: Option<&'a str>,
    /// Horodatage serveur, en millisecondes.
    pub(crate) timestamp_ms: u64,
    /// Vrai si la salle est chiffrée.
    pub(crate) encrypted: bool,
}

/// Décide si un message est remis en direct au handler (`src/matrix/sync.sdd`). Faux pour le bot
/// lui-même, pour les `m.notice` (convention anti-boucle entre bots) et pour les remplacements
/// (éditions).
#[must_use]
pub(crate) fn should_deliver(own_user: &str, sender: &str, msgtype: &str, is_replacement: bool) -> bool {
    sender != own_user && matches!(msgtype, "m.text" | "m.emote") && !is_replacement
}

/// Convertit les entrées déjà extraites d'un événement en [`MatrixMessage`] (`src/matrix/sync.sdd`).
#[must_use]
pub(crate) fn to_message(parts: MessageParts<'_>) -> MatrixMessage {
    MatrixMessage {
        room_id: parts.room_id.to_owned(),
        event_id: parts.event_id.to_owned(),
        sender: parts.sender.to_owned(),
        body: parts.body.to_owned(),
        formatted_body: parts.formatted_body.map(str::to_owned),
        msgtype: parts.msgtype.to_owned(),
        timestamp_ms: parts.timestamp_ms,
        encrypted: parts.encrypted,
    }
}

/// Une invitation n'est acceptée que si son inviteur figure dans `auto_join_from`
/// (`src/matrix/sync.sdd`). Liste vide : aucune invitation acceptée.
#[must_use]
pub(crate) fn invite_allowed(auto_join_from: &[String], inviter: &str) -> bool {
    auto_join_from.iter().any(|entry| entry == inviter)
}

/// Durée de bail de `miryad.matrix.sync` pour un `sync_timeout_ms` donné : `2 * sync_timeout_ms +
/// 15_000` millisecondes (`src/matrix/sync.sdd`). Saturée à `u32::MAX` plutôt que débordante.
#[must_use]
pub(crate) fn lease_ms(sync_timeout_ms: u64) -> u32 {
    let ms = sync_timeout_ms.saturating_mul(2).saturating_add(15_000);
    u32::try_from(ms).unwrap_or(u32::MAX)
}

/// Délai avant la reprise après une erreur retryable (`src/matrix/sync.sdd`) : 1 s doublant à
/// chaque tentative, plafonné à 60 s.
#[must_use]
pub(crate) fn backoff(attempt: u32) -> Duration {
    // `checked_shl` rend `None` à partir de 64 : le plafond l'absorbe sans panique.
    let secs = 1_u64.checked_shl(attempt).unwrap_or(u64::MAX).min(60);
    Duration::from_secs(secs)
}

/// Issue d'une erreur de synchronisation (`src/matrix/sync.sdd`, étape 5).
#[derive(Debug)]
pub(crate) enum SyncFailure {
    /// Jeton refusé par le serveur : `NotLoggedIn`.
    UnknownToken,
    /// Erreur transitoire : reprise après `backoff`.
    Retryable,
    /// Toute autre erreur : `Err` immédiat.
    Fatal(MatrixError),
}

/// Classe une erreur de synchronisation (`src/matrix/sync.sdd`, étape 5) : jeton refusé
/// (`M_UNKNOWN_TOKEN`), transitoire (`MatrixError::retryable`), ou définitive.
#[must_use]
pub(crate) fn classify_sync_error(err: &matrix_sdk::Error) -> SyncFailure {
    if matches!(err.client_api_error_kind(), Some(ErrorKind::UnknownToken(_))) {
        return SyncFailure::UnknownToken;
    }
    let translated = from_sdk(err);
    if translated.retryable() {
        SyncFailure::Retryable
    } else {
        SyncFailure::Fatal(translated)
    }
}

/// Drapeau `sync_started` relâché à la sortie de `run_sync`, y compris sur annulation de sa future.
struct SyncSlot<'a>(&'a AtomicBool);

impl Drop for SyncSlot<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl MatrixClient {
    /// Boucle de synchronisation de la lecture « en direct » (`src/matrix/sync.sdd`) : prend le bail
    /// `miryad.matrix.sync`, fait un sync initial sans remise de messages, puis remet chaque message
    /// texte au `handler` jusqu'à `shutdown`. Libère le bail à la sortie (au mieux).
    ///
    /// # Errors
    ///
    /// `MatrixError::Config` si une boucle tourne déjà dans ce processus, `MatrixError::NotLoggedIn`
    /// si le jeton est refusé par le serveur, `MatrixError::Sdk` ou `MatrixError::Network` pour les
    /// erreurs non reprenables.
    pub async fn run_sync<F>(
        &self,
        handler: Arc<dyn MatrixEventHandler>,
        shutdown: F,
    ) -> Result<(), MatrixError>
    where
        F: Future<Output = ()> + Send,
    {
        if self.inner.sync_started.swap(true, Ordering::SeqCst) {
            return Err(MatrixError::Config("sync déjà démarré".into()));
        }
        let _slot = SyncSlot(&self.inner.sync_started);
        let outcome = tokio::select! {
            result = self.drive(handler) => result,
            () = shutdown => Ok(()),
        };
        self.release_lease().await;
        outcome
    }

    /// Cycle bail → sync initial → handlers → boucle, relancé à chaque perte de bail.
    async fn drive(&self, handler: Arc<dyn MatrixEventHandler>) -> Result<(), MatrixError> {
        let own = self.user_id();
        if own.is_empty() {
            return Err(MatrixError::NotLoggedIn);
        }
        let attempt = AtomicU32::new(0);
        loop {
            // Étape 1 : bail. Attente tant qu'un autre réplica le tient.
            while !self.try_lease().await? {
                tokio::time::sleep(LEASE_RETRY).await;
            }
            // Étape 2 : sync initial sans jeton, sans remise de messages.
            match initial_sync(&self.inner.client, &self.inner.config.auto_join_from).await {
                Ok(()) => attempt.store(0, Ordering::SeqCst),
                Err(err) => match classify_sync_error(&err) {
                    SyncFailure::UnknownToken => return Err(MatrixError::NotLoggedIn),
                    SyncFailure::Fatal(err) => return Err(err),
                    SyncFailure::Retryable => {
                        back_off(&attempt).await;
                        continue;
                    }
                },
            }
            // Étape 3 : handlers enregistrés après le sync initial, retirés à leur chute.
            let _handlers = self.register_handlers(&handler, &own);
            // Étape 4 : boucle. Rend Ok(()) seulement quand le bail est perdu.
            self.sync_while_leased(&attempt).await?;
        }
    }

    /// Boucle `sync_with_callback` tant que le bail est tenu (`src/matrix/sync.sdd`, étapes 4 et 5).
    /// Rend `Ok(())` quand le bail est perdu, `Err` pour une erreur non reprenable.
    async fn sync_while_leased(&self, attempt: &AtomicU32) -> Result<(), MatrixError> {
        let config = &self.inner.config;
        let lease = lease_ms(config.sync_timeout_ms);
        let holder = config.holder_name.as_str();
        let store = &self.inner.store;
        loop {
            let settings = SyncSettings::new().timeout(Duration::from_millis(config.sync_timeout_ms));
            let result = self
                .inner
                .client
                .sync_with_callback(settings, move |_response| async move {
                    attempt.store(0, Ordering::SeqCst);
                    match store.try_take_leased_lock(lease, SYNC_LEASE_KEY, holder).await {
                        Ok(Some(_)) => LoopCtrl::Continue,
                        Ok(None) => {
                            tracing::warn!("bail de synchronisation perdu : retour à l'attente");
                            LoopCtrl::Break
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "renouvellement du bail de synchronisation en échec");
                            LoopCtrl::Break
                        }
                    }
                })
                .await;
            match result {
                Ok(()) => return Ok(()),
                Err(err) => match classify_sync_error(&err) {
                    SyncFailure::UnknownToken => return Err(MatrixError::NotLoggedIn),
                    SyncFailure::Fatal(err) => return Err(err),
                    SyncFailure::Retryable => {
                        back_off(attempt).await;
                        if !self.try_lease().await? {
                            return Ok(());
                        }
                    }
                },
            }
        }
    }

    /// Prend ou prolonge le bail de synchronisation ; `false` si un autre réplica le tient.
    async fn try_lease(&self) -> Result<bool, MatrixError> {
        let lease = lease_ms(self.inner.config.sync_timeout_ms);
        self.inner
            .store
            .try_take_leased_lock(lease, SYNC_LEASE_KEY, &self.inner.config.holder_name)
            .await
            .map(|generation| generation.is_some())
            .map_err(|err| MatrixError::Sdk(format!("bail de synchronisation : {err}")))
    }

    /// Libère le bail d'une milliseconde (`src/matrix/sync.sdd`, étape 4b) ; un échec est journalisé.
    async fn release_lease(&self) {
        let outcome = self
            .inner
            .store
            .try_take_leased_lock(1, SYNC_LEASE_KEY, &self.inner.config.holder_name)
            .await;
        if let Err(err) = outcome {
            tracing::warn!(error = %err, "libération du bail de synchronisation en échec (expiration naturelle)");
        }
    }

    /// Enregistre les trois handlers d'événements (messages, invitations, indéchiffrables). Les
    /// garde-fous les retirent à leur chute.
    fn register_handlers(
        &self,
        handler: &Arc<dyn MatrixEventHandler>,
        own: &str,
    ) -> Vec<EventHandlerDropGuard> {
        let client = &self.inner.client;
        let allowed = Arc::new(self.inner.config.auto_join_from.clone());

        let message_handler = Arc::clone(handler);
        let message_own = own.to_owned();
        let messages = client.add_event_handler(
            move |event: SyncRoomMessageEvent, room: Room, encryption: Option<EncryptionInfo>| {
                let handler = Arc::clone(&message_handler);
                let own = message_own.clone();
                async move {
                    deliver_message(&handler, &own, &room, event, encryption.is_some()).await;
                }
            },
        );

        let invite_allowed_from = Arc::clone(&allowed);
        let invite_own = own.to_owned();
        let invites = client.add_event_handler(move |event: StrippedRoomMemberEvent, room: Room| {
            let allowed = Arc::clone(&invite_allowed_from);
            let own = invite_own.clone();
            async move {
                if event.content.membership == MembershipState::Invite && event.state_key.as_str() == own {
                    accept_invite(&room, event.sender.as_str(), &allowed).await;
                }
            }
        });

        let undecryptable =
            client.add_event_handler(|event: OriginalSyncRoomEncryptedEvent, room: Room| async move {
                tracing::warn!(
                    room_id = %room.room_id(),
                    event_id = %event.event_id,
                    "événement indéchiffrable non remis au handler"
                );
            });

        vec![
            client.event_handler_drop_guard(messages),
            client.event_handler_drop_guard(invites),
            client.event_handler_drop_guard(undecryptable),
        ]
    }
}

/// Attend avant la reprise après une erreur retryable (`src/matrix/sync.sdd`, étape 5).
async fn back_off(attempt: &AtomicU32) {
    let current = attempt.load(Ordering::SeqCst);
    attempt.store(current.saturating_add(1), Ordering::SeqCst);
    tokio::time::sleep(backoff(current)).await;
}

/// Sync initial sans jeton, délai nul : rien n'est remis au handler, les invitations rapportées
/// sont traitées (`src/matrix/sync.sdd`, étape 2).
async fn initial_sync(client: &Client, allowed: &[String]) -> Result<(), matrix_sdk::Error> {
    tracing::info!("sync initial : les messages reçus pendant l'arrêt ne sont pas rattrapés");
    let settings = SyncSettings::new()
        .token(SyncToken::NoToken)
        .timeout(Duration::ZERO);
    client.sync_once(settings).await?;
    for room in client.invited_rooms() {
        match room.invite_details().await {
            Ok(invite) => accept_invite(&room, invite.inviter_id.as_str(), allowed).await,
            Err(err) => {
                tracing::warn!(room_id = %room.room_id(), error = %err, "détail d'invitation illisible");
            }
        }
    }
    Ok(())
}

/// Accepte une invitation si son inviteur est autorisé ; jusqu'à quatre appels de jonction, espacés
/// de 1 s, 2 s puis 4 s (`src/matrix/sync.sdd`, Invitations).
async fn accept_invite(room: &Room, inviter: &str, allowed: &[String]) {
    if !invite_allowed(allowed, inviter) {
        tracing::info!(inviter, "invitation ignorée : inviteur non autorisé");
        return;
    }
    let mut delays = JOIN_RETRY_DELAYS.iter();
    loop {
        match room.join().await {
            Ok(()) => {
                tracing::info!(room_id = %room.room_id(), inviter, "invitation acceptée");
                return;
            }
            Err(err) => {
                let translated = from_sdk(&err);
                let Some(delay) = delays.next() else {
                    tracing::warn!(room_id = %room.room_id(), error = %translated, "jonction abandonnée");
                    return;
                };
                tracing::warn!(room_id = %room.room_id(), error = %translated, "jonction refusée, nouvelle tentative");
                tokio::time::sleep(*delay).await;
            }
        }
    }
}

/// Remet un message de salle au handler si `should_deliver` l'accepte (`src/matrix/sync.sdd`).
async fn deliver_message(
    handler: &Arc<dyn MatrixEventHandler>,
    own: &str,
    room: &Room,
    event: SyncRoomMessageEvent,
    encrypted: bool,
) {
    let SyncRoomMessageEvent::Original(original) = event else {
        return;
    };
    let (msgtype, body, formatted_body) = match &original.content.msgtype {
        MessageType::Text(text) => ("m.text", text.body.as_str(), text.formatted.as_ref()),
        MessageType::Emote(emote) => ("m.emote", emote.body.as_str(), emote.formatted.as_ref()),
        MessageType::Notice(notice) => ("m.notice", notice.body.as_str(), notice.formatted.as_ref()),
        _ => return,
    };
    let is_replacement = matches!(original.content.relates_to, Some(Relation::Replacement(_)));
    let sender = original.sender.as_str();
    if !should_deliver(own, sender, msgtype, is_replacement) {
        return;
    }
    let message = to_message(MessageParts {
        room_id: room.room_id().as_str(),
        event_id: original.event_id.as_str(),
        sender,
        msgtype,
        body,
        formatted_body: formatted_body.map(|f| f.body.as_str()),
        timestamp_ms: u64::from(original.origin_server_ts.0),
        encrypted,
    });
    deliver(handler, message).await;
}

/// Exécute `on_message` dans une tâche dédiée : un panic est journalisé, la boucle continue.
async fn deliver(handler: &Arc<dyn MatrixEventHandler>, message: MatrixMessage) {
    let room_id = message.room_id.clone();
    let event_id = message.event_id.clone();
    let task = tokio::task::spawn({
        let handler = Arc::clone(handler);
        async move { handler.on_message(message).await }
    });
    match task.await {
        Ok(()) => {}
        Err(err) if err.is_panic() => {
            tracing::error!(
                room_id,
                event_id,
                "le handler de message a paniqué ; la synchronisation continue"
            );
        }
        Err(err) => tracing::warn!(room_id, event_id, error = %err, "remise du message interrompue"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MatrixEventHandler, MatrixMessage, MessageParts, SyncFailure, backoff, classify_sync_error,
        invite_allowed, lease_ms, should_deliver, to_message,
    };
    use crate::matrix::client::MatrixClient;
    use crate::matrix::config::MatrixConfig;
    use crate::matrix::error::MatrixError;
    use matrix_sdk::HttpError;
    use ruma::api::EndpointError;
    use ruma::api::error::FromHttpResponseError;
    use sea_orm::{ConnectOptions, Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    struct StoreMigrator;

    impl MigratorTrait for StoreMigrator {
        fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
            crate::matrix::store::migrations()
        }
    }

    /// Base SQLite mémoire migrée, une seule connexion (même pattern que `client.rs`).
    async fn fresh_db() -> DatabaseConnection {
        let mut opts = ConnectOptions::new("sqlite::memory:");
        opts.max_connections(1).min_connections(1);
        let db = Database::connect(opts).await.expect("in-memory sqlite connects");
        StoreMigrator::up(&db, None)
            .await
            .expect("store migrations apply");
        db
    }

    /// Client importé sur base mémoire, hors réseau (même construction que `client.rs`).
    async fn imported_client() -> MatrixClient {
        let db = fresh_db().await;
        let config = MatrixConfig::new("https://m.example", "passphrase-1", "pod-1");
        let session = crate::matrix::client::SessionImport {
            user_id: "@bot:example.org".into(),
            device_id: "DEVICE1".into(),
            access_token: "tok".into(),
            refresh_token: None,
        };
        MatrixClient::import_session(db, config, session)
            .await
            .expect("import_session sur base mémoire")
    }

    struct Silent;

    #[async_trait::async_trait]
    impl MatrixEventHandler for Silent {
        async fn on_message(&self, _message: MatrixMessage) {}
    }

    /// Erreur `matrix-sdk` construite par le chemin public réel (même helper que `error.rs`).
    fn sdk_http(status: u16, body: &str) -> matrix_sdk::Error {
        let response = ruma::exports::http::Response::builder()
            .status(status)
            .body(body.as_bytes().to_vec())
            .expect("réponse de test valide");
        let api_error = matrix_sdk::RumaApiError::from_http_response(response);
        matrix_sdk::Error::Http(Box::new(HttpError::Api(Box::new(FromHttpResponseError::Server(
            api_error,
        )))))
    }

    // Scenario: une seconde boucle dans le même processus est refusée (drapeau forcé)
    #[tokio::test]
    async fn second_sync_loop_in_same_process_is_refused() {
        let client = imported_client().await;
        client.inner.sync_started.store(true, Ordering::SeqCst);
        let result = client
            .run_sync(Arc::new(Silent), std::future::pending::<()>())
            .await;
        assert!(
            matches!(&result, Err(MatrixError::Config(message)) if message.contains("sync déjà démarré")),
            "obtenu : {result:?}"
        );
        assert!(
            client.inner.sync_started.load(Ordering::SeqCst),
            "le refus ne doit pas remettre à faux le drapeau d'une boucle en cours"
        );
    }

    // Décision de reprise : jeton refusé, erreur transitoire, erreur définitive
    #[test]
    fn unknown_token_is_classified_as_not_logged_in() {
        let err = sdk_http(
            401,
            r#"{"errcode":"M_UNKNOWN_TOKEN","error":"Unrecognised access token","soft_logout":false}"#,
        );
        assert!(matches!(classify_sync_error(&err), SyncFailure::UnknownToken));
    }

    #[test]
    fn bad_gateway_is_classified_as_retryable() {
        let err = sdk_http(502, "<html>Bad Gateway</html>");
        assert!(matches!(classify_sync_error(&err), SyncFailure::Retryable));
    }

    #[test]
    fn forbidden_is_classified_as_fatal() {
        let err = sdk_http(403, r#"{"errcode":"M_FORBIDDEN","error":"Forbidden"}"#);
        assert!(matches!(classify_sync_error(&err), SyncFailure::Fatal(_)));
    }

    // Scenario: should_deliver filtre soi-même, les notices et les éditions
    #[test]
    fn should_deliver_filters_self_notices_and_edits() {
        let own = "@bot:x";
        assert!(!should_deliver(own, "@bot:x", "m.text", false));
        assert!(!should_deliver(own, "@a:x", "m.notice", false));
        assert!(!should_deliver(own, "@a:x", "m.text", true));
        assert!(should_deliver(own, "@a:x", "m.text", false));
        assert!(should_deliver(own, "@a:x", "m.emote", false));
    }

    // Scenario: invite_allowed n'accepte que les inviteurs listés
    #[test]
    fn invite_allowed_accepts_only_listed_inviters() {
        let allowed = vec!["@xavier:x".to_owned()];
        assert!(invite_allowed(&allowed, "@xavier:x"));
        assert!(!invite_allowed(&allowed, "@inconnu:x"));
        assert!(!invite_allowed(&[], "@xavier:x"));
    }

    // Scenario: le bail couvre un cycle de sync complet
    #[test]
    fn lease_covers_a_full_sync_cycle() {
        assert_eq!(lease_ms(30_000), 75_000);
    }

    #[test]
    fn lease_saturates_instead_of_overflowing() {
        assert_eq!(lease_ms(u64::MAX), u32::MAX);
    }

    // Scenario: le backoff double et plafonne
    #[test]
    fn backoff_doubles_then_caps_at_sixty_seconds() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(2), Duration::from_secs(4));
        assert_eq!(backoff(6), Duration::from_secs(60));
        assert_eq!(backoff(20), Duration::from_secs(60));
    }

    #[test]
    fn backoff_never_panics_on_large_attempts() {
        assert_eq!(backoff(64), Duration::from_secs(60));
        assert_eq!(backoff(u32::MAX), Duration::from_secs(60));
    }

    // Scenario: to_message reprend tous les champs
    #[test]
    fn to_message_copies_every_field() {
        let message = to_message(MessageParts {
            room_id: "!r:x",
            event_id: "$e",
            sender: "@a:x",
            msgtype: "m.text",
            body: "salut",
            formatted_body: Some("<b>salut</b>"),
            timestamp_ms: 1_700_000_000_000,
            encrypted: true,
        });
        assert_eq!(
            message,
            MatrixMessage {
                room_id: "!r:x".into(),
                event_id: "$e".into(),
                sender: "@a:x".into(),
                body: "salut".into(),
                formatted_body: Some("<b>salut</b>".into()),
                msgtype: "m.text".into(),
                timestamp_ms: 1_700_000_000_000,
                encrypted: true,
            }
        );
    }

    #[test]
    fn to_message_keeps_absent_formatted_body() {
        let message = to_message(MessageParts {
            room_id: "!r:x",
            event_id: "$e",
            sender: "@a:x",
            msgtype: "m.emote",
            body: "fait",
            formatted_body: None,
            timestamp_ms: 1,
            encrypted: false,
        });
        assert_eq!(message.formatted_body, None);
        assert_eq!(message.msgtype, "m.emote");
        assert!(!message.encrypted);
    }
}
