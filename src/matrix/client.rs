//! Client Matrix de bot (`src/matrix/client.sdd`) : connexion par session existante ou importée,
//! persistance des rotations de jetons, identifiant du bot.
//!
//! Le client est une valeur `Clone` construite une fois, partagée entre la boucle de sync, les steps
//! de workflow et le code de l'application. Aucune requête réseau n'est émise à la construction hors
//! restauration de la session ; le démarrage de la synchronisation reste à `run_sync`.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use matrix_sdk::AuthSession;
use matrix_sdk::Room;
use matrix_sdk::SessionChange;
use matrix_sdk::SessionMeta;
use matrix_sdk::SessionTokens;
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::authentication::oauth::{ClientId, OAuthSession, UserSession};
use matrix_sdk::cross_process_lock::CrossProcessLockConfig;
use matrix_sdk::deserialized_responses::TimelineEvent;
use matrix_sdk::room::MessagesOptions;
use matrix_sdk::ruma::events::room::message::{MessageType, RoomMessageEventContent};
use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent};
use matrix_sdk::ruma::{OwnedDeviceId, OwnedRoomId, OwnedTransactionId, RoomId, RoomOrAliasId, UInt, UserId};
use matrix_sdk_crypto::store::CryptoStore;
use sea_orm::DatabaseConnection;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::matrix::config::MatrixConfig;
use crate::matrix::device_code::{self, DeviceCodePrompt, DeviceCodeTokens};
use crate::matrix::error::{MatrixError, from_sdk};
use crate::matrix::session::{self, StoredSession};
use crate::matrix::store::{SeaOrmCryptoStore, StoreError};
use crate::matrix::sync::{self, MatrixMessage, MessageParts};

/// Délai global des requêtes du device code (`src/matrix/client.sdd`). Les attentes entre deux
/// interrogations sont gérées par `device_code`, pas par ce délai.
const DEVICE_CODE_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Type de message envoyé par le bot (`src/matrix/client.sdd`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    /// Message ordinaire (`m.text`).
    Text,
    /// Notice (`m.notice`) : convention des bots, les autres bots ne répondent pas aux notices.
    Notice,
}

/// Session fournie par l'application quand le device code n'est pas possible : jeton de
/// compatibilité émis par MAS ou par l'administrateur du homeserver (`src/matrix/client.sdd`).
#[derive(Clone)]
pub struct SessionImport {
    /// Identifiant Matrix du bot, ex. `@bot:server`.
    pub user_id: String,
    /// Identifiant d'appareil attribué au jeton.
    pub device_id: String,
    /// Jeton d'accès. **Secret** : jamais affiché par `Debug`.
    pub access_token: String,
    /// Jeton de rafraîchissement, s'il existe. **Secret** : jamais affiché par `Debug`.
    pub refresh_token: Option<String>,
}

/// `Debug` ne montre que l'identité ; les jetons et leurs noms de champs sont omis.
impl fmt::Debug for SessionImport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionImport")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

/// Construit le contenu d'un message : texte brut, ou texte brut plus HTML (`formatted_body`,
/// `org.matrix.custom.html`) quand `html` est fourni (`src/matrix/client.sdd`).
pub(crate) fn build_content(body: &str, html: Option<&str>, kind: MessageKind) -> RoomMessageEventContent {
    match (kind, html) {
        (MessageKind::Text, None) => RoomMessageEventContent::text_plain(body),
        (MessageKind::Text, Some(html)) => RoomMessageEventContent::text_html(body, html),
        (MessageKind::Notice, None) => RoomMessageEventContent::notice_plain(body),
        (MessageKind::Notice, Some(html)) => RoomMessageEventContent::notice_html(body, html),
    }
}

/// État partagé du client (`src/matrix/client.sdd`). Visibilité `pub(crate)` : `sync.rs` lit ses
/// champs (arbitrage 2026-10-18). Les tâches de fond sont annulées au `Drop` (dernier clone disparu).
pub(crate) struct Inner {
    pub(crate) client: matrix_sdk::Client,
    pub(crate) store: SeaOrmCryptoStore,
    pub(crate) config: MatrixConfig,
    tasks: Vec<JoinHandle<()>>,
    /// Vrai une fois la boucle de sync démarrée (`run_sync`, `src/matrix/sync.sdd`).
    pub(crate) sync_started: AtomicBool,
}

impl Drop for Inner {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Client Matrix de bot (`src/matrix/client.sdd`). Ne rend jamais `matrix_sdk::Client` publiquement.
#[derive(Clone)]
pub struct MatrixClient {
    pub(crate) inner: Arc<Inner>,
}

impl MatrixClient {
    /// Connecte le bot à partir de la session déjà enregistrée dans le store du compte.
    ///
    /// # Errors
    ///
    /// `MatrixError::Config` si la configuration est invalide, `MatrixError::NotLoggedIn` si aucune
    /// session n'est enregistrée, `MatrixError::Store` si le store ou la session sont illisibles.
    pub async fn connect(db: DatabaseConnection, config: MatrixConfig) -> Result<Self, MatrixError> {
        config.validate()?;
        let (store, client) = build_client(db, &config).await?;
        let stored = session::load(&store).await?.ok_or(MatrixError::NotLoggedIn)?;
        Self::start(client, store, config, stored).await
    }

    /// Connecte le bot par device code (RFC 8628, `src/matrix/device_code.sdd`), puis enregistre la
    /// session OAuth obtenue (en remplaçant l'existante) et la restaure. `on_prompt` reçoit le code
    /// à montrer à l'opérateur, une seule fois.
    ///
    /// # Errors
    ///
    /// `MatrixError::Config` si la configuration est invalide, `MatrixError::Login` si le serveur
    /// n'annonce pas le device code, si le code expire ou est refusé, `MatrixError::InvalidId` si
    /// l'identifiant du compte est mal formé, `MatrixError::Store` ou `MatrixError::Sdk` à
    /// l'enregistrement ou à la restauration, `MatrixError::Network` pour les échecs réseau.
    pub async fn login_with_device_code(
        db: DatabaseConnection,
        config: MatrixConfig,
        on_prompt: impl FnOnce(DeviceCodePrompt) + Send,
    ) -> Result<Self, MatrixError> {
        config.validate()?;
        let (store, client) = build_client(db, &config).await?;
        let existing = load_client_id(&store).await?;
        let http = device_code_http_client()?;
        let tokens = device_code::login(
            &config.homeserver_url,
            &config.device_display_name,
            existing.as_deref(),
            on_prompt,
            &http,
        )
        .await?;
        let client_id = tokens.client_id.clone();
        let stored = oauth_session(tokens)?;
        CryptoStore::set_custom_value(&store, OAUTH_CLIENT_KEY, encode_client_id(&client_id)).await?;
        session::save(&store, &stored).await?;
        Self::start(client, store, config, stored).await
    }

    /// Enregistre une session importée (en remplaçant l'existante) puis la restaure. La session
    /// reste en base si la restauration échoue (arbitrage 2026-10-18).
    ///
    /// # Errors
    ///
    /// `MatrixError::Config` si la configuration est invalide, `MatrixError::InvalidId` si
    /// `user_id` est mal formé, `MatrixError::Store` ou `MatrixError::Sdk` à l'enregistrement ou à
    /// la restauration.
    pub async fn import_session(
        db: DatabaseConnection,
        config: MatrixConfig,
        session: SessionImport,
    ) -> Result<Self, MatrixError> {
        config.validate()?;
        let user_id =
            UserId::parse(&session.user_id).map_err(|_| MatrixError::InvalidId(session.user_id.clone()))?;
        let stored = StoredSession::Matrix(MatrixSession {
            meta: SessionMeta {
                user_id,
                device_id: OwnedDeviceId::from(session.device_id),
            },
            tokens: SessionTokens {
                access_token: session.access_token,
                refresh_token: session.refresh_token,
            },
        });
        let (store, client) = build_client(db, &config).await?;
        session::save(&store, &stored).await?;
        Self::start(client, store, config, stored).await
    }

    /// Identifiant Matrix du bot, ex. `@bot:server` ; chaîne vide si le client n'est pas connecté.
    #[must_use]
    pub fn user_id(&self) -> String {
        self.inner
            .client
            .user_id()
            .map(ToString::to_string)
            .unwrap_or_default()
    }

    /// Envoie un message dans une salle (`src/matrix/client.sdd`). Rend l'identifiant de l'événement.
    ///
    /// # Errors
    ///
    /// `MatrixError::InvalidId` si `room_id` n'est pas un identifiant de salle `!x:y`,
    /// `MatrixError::RoomUnavailable` si la salle est inaccessible, `MatrixError::Sdk` ou
    /// `MatrixError::Network` pour les autres échecs.
    pub async fn send(
        &self,
        room_id: &str,
        body: &str,
        html: Option<&str>,
        kind: MessageKind,
        txn_id: Option<&str>,
    ) -> Result<String, MatrixError> {
        let room_id = parse_room_id(room_id)?;
        let content = build_content(body, html, kind);
        let room = self.joined_room(&room_id).await?;
        let event_id = match txn_id {
            Some(txn) => {
                room.send(content)
                    .with_transaction_id(OwnedTransactionId::from(txn))
                    .await
            }
            None => room.send(content).await,
        }
        .map_err(|err| from_sdk(&err))?
        .response
        .event_id;
        Ok(event_id.to_string())
    }

    /// Salle à laquelle envoyer : celle du cache du client, sinon jonction par identifiant.
    async fn joined_room(&self, room_id: &RoomId) -> Result<Room, MatrixError> {
        if let Some(room) = self.inner.client.get_room(room_id) {
            return Ok(room);
        }
        self.inner.client.join_room_by_id(room_id).await.map_err(|err| {
            let translated = from_sdk(&err);
            tracing::warn!(room_id = %room_id, error = %translated, "jonction à la salle échouée");
            // Une erreur transitoire reste retryable (retry du step) ; le reste est une salle inaccessible.
            if translated.retryable() {
                translated
            } else {
                MatrixError::RoomUnavailable(room_id.to_string())
            }
        })
    }

    /// Rejoint une salle par identifiant ou par alias et rend son identifiant (`src/matrix/client.sdd`).
    ///
    /// # Errors
    ///
    /// `MatrixError::InvalidId` si `id_or_alias` est mal formé, `MatrixError::RoomUnavailable` si la
    /// jonction est refusée, `MatrixError::Sdk` ou `MatrixError::Network` pour les autres échecs.
    pub async fn join_room(&self, id_or_alias: &str) -> Result<String, MatrixError> {
        let joined = if let Ok(room_id) = RoomId::parse(id_or_alias) {
            self.inner.client.join_room_by_id(&room_id).await
        } else {
            let alias = RoomOrAliasId::parse(id_or_alias)
                .map_err(|_| MatrixError::InvalidId(id_or_alias.to_owned()))?;
            self.inner.client.join_room_by_id_or_alias(&alias, &[]).await
        };
        let room = joined.map_err(|err| from_sdk(&err))?;
        Ok(room.room_id().to_string())
    }

    /// Lit les `limit` derniers messages d'une salle, par ordre chronologique croissant
    /// (`src/matrix/client.sdd`). `limit` doit être compris entre 1 et 100.
    ///
    /// # Errors
    ///
    /// `MatrixError::Config` si `limit` est hors bornes, `MatrixError::InvalidId` si `room_id` n'est
    /// pas un identifiant de salle `!x:y`, `MatrixError::RoomUnavailable` ou `MatrixError::Sdk` /
    /// `MatrixError::Network` pour les échecs de lecture.
    pub async fn recent_messages(
        &self,
        room_id: &str,
        limit: u16,
    ) -> Result<Vec<MatrixMessage>, MatrixError> {
        if !(1..=100).contains(&limit) {
            return Err(MatrixError::Config(format!(
                "limit doit être compris entre 1 et 100 (reçu {limit})"
            )));
        }
        let room_id = parse_room_id(room_id)?;
        let room = self
            .inner
            .client
            .get_room(&room_id)
            .ok_or_else(|| MatrixError::RoomUnavailable(room_id.to_string()))?;
        let mut options = MessagesOptions::backward();
        options.limit = UInt::from(limit);
        let page = room.messages(options).await.map_err(|err| from_sdk(&err))?;
        // Pagination en arrière : le plus récent d'abord, d'où l'inversion pour l'ordre croissant.
        Ok(page
            .chunk
            .iter()
            .rev()
            .filter_map(|event| text_message(room_id.as_str(), event))
            .collect())
    }

    /// Abonne la persistance des rotations, restaure la session, puis lance la tâche de fond.
    /// L'abonnement précède la restauration pour ne pas manquer un `TokensRefreshed` émis pendant
    /// celle-ci ; la tâche n'écrit qu'une fois la restauration réussie.
    async fn start(
        client: matrix_sdk::Client,
        store: SeaOrmCryptoStore,
        config: MatrixConfig,
        stored: StoredSession,
    ) -> Result<Self, MatrixError> {
        let changes = client.subscribe_to_session_changes();
        client
            .restore_session(to_auth_session(stored))
            .await
            .map_err(|err| from_sdk(&err))?;
        let tasks = vec![spawn_session_persistence(client.clone(), store.clone(), changes)];
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                store,
                config,
                tasks,
                sync_started: AtomicBool::new(false),
            }),
        })
    }
}

/// Ouvre le store du compte et construit le `Client` (sans requête réseau : `homeserver_url` seul).
async fn build_client(
    db: DatabaseConnection,
    config: &MatrixConfig,
) -> Result<(SeaOrmCryptoStore, matrix_sdk::Client), MatrixError> {
    let store = SeaOrmCryptoStore::open(db, &config.account_key, &config.store_passphrase).await?;
    let lock = CrossProcessLockConfig::multi_process(config.holder_name.clone());
    let client = matrix_sdk::Client::builder()
        .homeserver_url(&config.homeserver_url)
        .store_config(store.store_config(lock))
        .build()
        .await
        .map_err(|err| MatrixError::Sdk(err.to_string()))?;
    Ok((store, client))
}

/// Identifiant de salle `!x:y`. Un alias (`#x:y`) ou une forme invalide est `InvalidId`.
fn parse_room_id(room_id: &str) -> Result<OwnedRoomId, MatrixError> {
    RoomId::parse(room_id).map_err(|_| MatrixError::InvalidId(room_id.to_owned()))
}

/// Message texte, notice ou emote d'un événement de l'historique (`src/matrix/client.sdd`). Rend
/// `None` pour les autres types, et pour les événements indéchiffrables (journalisés sans contenu).
fn text_message(room_id: &str, event: &TimelineEvent) -> Option<MatrixMessage> {
    if event.kind.is_utd() {
        let event_id = event
            .event_id()
            .map_or_else(|| "inconnu".to_owned(), |id| id.to_string());
        tracing::warn!(room_id, event_id, "événement indéchiffrable omis de l'historique");
        return None;
    }
    let Ok(any) = event.raw().deserialize() else {
        return None;
    };
    let AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
        SyncMessageLikeEvent::Original(original),
    )) = any
    else {
        return None;
    };
    let (msgtype, body, formatted) = match &original.content.msgtype {
        MessageType::Text(text) => ("m.text", text.body.as_str(), text.formatted.as_ref()),
        MessageType::Notice(notice) => ("m.notice", notice.body.as_str(), notice.formatted.as_ref()),
        MessageType::Emote(emote) => ("m.emote", emote.body.as_str(), emote.formatted.as_ref()),
        _ => return None,
    };
    Some(sync::to_message(MessageParts {
        room_id,
        event_id: original.event_id.as_str(),
        sender: original.sender.as_str(),
        msgtype,
        body,
        formatted_body: formatted.map(|f| f.body.as_str()),
        timestamp_ms: u64::from(original.origin_server_ts.0),
        encrypted: event.encryption_info().is_some(),
    }))
}

/// Forme persistée → session d'authentification `matrix-sdk`, dans la variante correspondante.
fn to_auth_session(stored: StoredSession) -> AuthSession {
    match stored {
        StoredSession::Matrix(session) => AuthSession::Matrix(session),
        StoredSession::OAuth { client_id, user } => AuthSession::OAuth(Box::new(OAuthSession {
            client_id: ClientId::new(client_id),
            user,
        })),
    }
}

/// Clé de la valeur personnalisée qui porte l'identifiant de client OAuth (`src/matrix/client.sdd`).
pub(crate) const OAUTH_CLIENT_KEY: &str = "miryad.matrix.oauth_client";

/// Session OAuth persistable depuis les jetons du device code (`src/matrix/client.sdd`). Rend
/// `InvalidId` si `user_id` est mal formé, comme `import_session`.
fn oauth_session(tokens: DeviceCodeTokens) -> Result<StoredSession, MatrixError> {
    let user_id =
        UserId::parse(&tokens.user_id).map_err(|_| MatrixError::InvalidId(tokens.user_id.clone()))?;
    Ok(StoredSession::OAuth {
        client_id: tokens.client_id,
        user: UserSession {
            meta: SessionMeta {
                user_id,
                device_id: OwnedDeviceId::from(tokens.device_id),
            },
            tokens: SessionTokens {
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token,
            },
        },
    })
}

/// Valeur persistée de l'identifiant de client : ses octets UTF-8 (`src/matrix/client.sdd`).
fn encode_client_id(client_id: &str) -> Vec<u8> {
    client_id.as_bytes().to_vec()
}

/// Décode la valeur persistée de l'identifiant de client. Des octets non UTF-8 sont
/// `StoreError::Corrupt`, sans contenu affiché (`src/matrix/client.sdd`).
fn decode_client_id(bytes: &[u8]) -> Result<String, MatrixError> {
    std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| {
        MatrixError::Store(StoreError::Corrupt(
            "identifiant de client OAuth non UTF-8".into(),
        ))
    })
}

/// Lit l'identifiant de client enregistré dans le store, `None` s'il n'y en a pas
/// (`src/matrix/client.sdd`).
async fn load_client_id(store: &SeaOrmCryptoStore) -> Result<Option<String>, MatrixError> {
    let Some(bytes) = CryptoStore::get_custom_value(store, OAUTH_CLIENT_KEY).await? else {
        return Ok(None);
    };
    decode_client_id(&bytes).map(Some)
}

/// Client HTTP du device code : aucune redirection suivie (`src/matrix/device_code.sdd`, `Must not`)
/// et délai global borné.
fn device_code_http_client() -> Result<reqwest::Client, MatrixError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(DEVICE_CODE_HTTP_TIMEOUT)
        .build()
        .map_err(|err| MatrixError::Network {
            message: format!("client HTTP du device code : {err}"),
            retryable: false,
        })
}

/// Réécrit la session courante du client dans le store (`src/matrix/client.sdd`). Une erreur est
/// journalisée sans jeton, jamais remontée.
async fn persist_current_session(client: &matrix_sdk::Client, store: &SeaOrmCryptoStore) {
    let Some(current) = client.session() else {
        tracing::warn!("aucune session à persister après rotation des jetons");
        return;
    };
    let stored = match current {
        AuthSession::Matrix(session) => StoredSession::Matrix(session),
        AuthSession::OAuth(session) => StoredSession::from(session.as_ref()),
        // `AuthSession` est `#[non_exhaustive]` : une variante future n'est pas persistée à l'aveugle.
        _ => {
            tracing::warn!("variante de session Matrix inconnue : rien à persister");
            return;
        }
    };
    if let Err(err) = session::save(store, &stored).await {
        tracing::error!(error = %err, "échec de la sauvegarde de la session Matrix rafraîchie");
    }
}

/// Tâche de persistance des rotations (`src/matrix/client.sdd`). Sur `UnknownToken`, elle journalise
/// sans rien écrire : les appels suivants rendront `NotLoggedIn`.
fn spawn_session_persistence(
    client: matrix_sdk::Client,
    store: SeaOrmCryptoStore,
    mut changes: tokio::sync::broadcast::Receiver<SessionChange>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match changes.recv().await {
                Ok(SessionChange::TokensRefreshed) | Err(RecvError::Lagged(_)) => {
                    persist_current_session(&client, &store).await;
                }
                Ok(SessionChange::UnknownToken(_)) => {
                    tracing::error!(
                        "session Matrix refusée par le serveur (jeton inconnu) : reconnexion nécessaire"
                    );
                }
                Err(RecvError::Closed) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{
        MatrixClient, MessageKind, OAUTH_CLIENT_KEY, SessionImport, build_content, decode_client_id,
        encode_client_id, load_client_id, oauth_session,
    };
    use crate::matrix::config::MatrixConfig;
    use crate::matrix::device_code::DeviceCodeTokens;
    use crate::matrix::error::MatrixError;
    use crate::matrix::session::StoredSession;
    use crate::matrix::store::{SeaOrmCryptoStore, StoreError};
    use matrix_sdk::ruma::events::room::message::{MessageType, RoomMessageEventContent};
    use matrix_sdk_crypto::store::CryptoStore;
    use sea_orm::{ConnectOptions, Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;

    struct StoreMigrator;

    impl MigratorTrait for StoreMigrator {
        fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
            crate::matrix::store::migrations()
        }
    }

    /// Base SQLite mémoire migrée. Une seule connexion : sinon chaque connexion du pool
    /// verrait sa propre base mémoire (même pattern que `session.rs`).
    async fn fresh_db() -> DatabaseConnection {
        let mut opts = ConnectOptions::new("sqlite::memory:");
        opts.max_connections(1).min_connections(1);
        let db = Database::connect(opts).await.expect("in-memory sqlite connects");
        StoreMigrator::up(&db, None)
            .await
            .expect("store migrations apply over an empty base");
        db
    }

    /// `(msgtype, body, formatted_body)` d'un contenu construit.
    fn parts(content: &RoomMessageEventContent) -> (String, String, Option<String>) {
        let formatted = match &content.msgtype {
            MessageType::Text(text) => text.formatted.as_ref().map(|f| f.body.clone()),
            MessageType::Notice(notice) => notice.formatted.as_ref().map(|f| f.body.clone()),
            _ => None,
        };
        (content.msgtype().to_owned(), content.body().to_owned(), formatted)
    }

    // Scenario: build_content produit texte brut ou HTML selon les entrées
    #[test]
    fn build_content_text_or_html() {
        assert_eq!(
            parts(&build_content("salut", None, MessageKind::Text)),
            ("m.text".into(), "salut".into(), None)
        );
        assert_eq!(
            parts(&build_content("salut", Some("<b>salut</b>"), MessageKind::Text)),
            ("m.text".into(), "salut".into(), Some("<b>salut</b>".into()))
        );
        assert_eq!(
            parts(&build_content("salut", None, MessageKind::Notice)),
            ("m.notice".into(), "salut".into(), None)
        );
        assert_eq!(
            parts(&build_content("salut", Some("<b>salut</b>"), MessageKind::Notice)),
            ("m.notice".into(), "salut".into(), Some("<b>salut</b>".into()))
        );
    }

    // Scenario: SessionImport masque ses jetons
    #[test]
    fn session_import_debug_hides_tokens() {
        let session = SessionImport {
            user_id: "@bot:x".into(),
            device_id: "DEVX".into(),
            access_token: "tok".into(),
            refresh_token: Some("ref".into()),
        };
        let shown = format!("{session:?}");
        assert!(!shown.contains("tok"), "jeton d'accès affiché : {shown}");
        assert!(
            !shown.contains("ref"),
            "jeton de rafraîchissement affiché : {shown}"
        );
    }

    // Scenario: connect sans session rend NotLoggedIn
    #[tokio::test]
    async fn connect_without_session_is_not_logged_in() {
        let db = fresh_db().await;
        let config = MatrixConfig::new("https://m.example", "passphrase-1", "pod-1");
        let Err(err) = MatrixClient::connect(db, config).await else {
            panic!("connect sans session doit échouer");
        };
        assert!(matches!(err, MatrixError::NotLoggedIn), "obtenu : {err}");
    }

    /// Client construit par `import_session` sur base mémoire. Aucun appel de `send`, `join_room` ou
    /// `recent_messages` n'a lieu avant les validations testées ici : elles rendent leur erreur sans
    /// toucher au réseau.
    async fn imported_client() -> MatrixClient {
        let db = fresh_db().await;
        let config = MatrixConfig::new("https://m.example", "passphrase-1", "pod-1");
        let session = SessionImport {
            user_id: "@bot:example.org".into(),
            device_id: "DEVICE1".into(),
            access_token: "tok".into(),
            refresh_token: None,
        };
        MatrixClient::import_session(db, config, session)
            .await
            .expect("import_session sur base mémoire")
    }

    // Scenario: un alias est refusé à l'envoi
    #[tokio::test]
    async fn send_refuses_room_alias() {
        let client = imported_client().await;
        let result = client
            .send("#salon:example.org", "salut", None, MessageKind::Text, None)
            .await;
        assert!(
            matches!(result, Err(MatrixError::InvalidId(_))),
            "obtenu : {result:?}"
        );
    }

    // Scenario: limit hors bornes est refusée
    #[tokio::test]
    async fn recent_messages_refuses_limit_out_of_bounds() {
        let client = imported_client().await;
        for limit in [0_u16, 101] {
            let result = client.recent_messages("!salon:example.org", limit).await;
            assert!(
                matches!(result, Err(MatrixError::Config(_))),
                "limit {limit} : obtenu {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn join_room_refuses_malformed_id() {
        let client = imported_client().await;
        let result = client.join_room("salon sans forme").await;
        assert!(
            matches!(result, Err(MatrixError::InvalidId(_))),
            "obtenu : {result:?}"
        );
    }

    /// Jetons du device code. Les secrets ont des sous-chaînes distinctes de l'identité affichée.
    fn device_tokens(user_id: &str, refresh: Option<&str>) -> DeviceCodeTokens {
        DeviceCodeTokens {
            client_id: "client-7".into(),
            user_id: user_id.into(),
            device_id: "DEVICE7".into(),
            access_token: "tok-acc-secret".into(),
            refresh_token: refresh.map(str::to_owned),
            expires_in_secs: Some(300),
        }
    }

    // Garde hors scénario (Tâche C, aucun Scenario dédié dans client.sdd) : la session OAuth porte
    // client_id, device_id, jetons et user_id issus des jetons du device code.
    #[test]
    fn oauth_session_carries_client_device_and_tokens() {
        let stored = oauth_session(device_tokens("@bot:example.org", Some("tok-ref-secret")))
            .expect("session OAuth construite");
        let StoredSession::OAuth { client_id, user } = stored else {
            panic!("variante OAuth attendue");
        };
        assert_eq!(client_id, "client-7");
        assert_eq!(user.meta.user_id.as_str(), "@bot:example.org");
        assert_eq!(user.meta.device_id.as_str(), "DEVICE7");
        assert_eq!(user.tokens.access_token, "tok-acc-secret");
        assert_eq!(user.tokens.refresh_token.as_deref(), Some("tok-ref-secret"));
    }

    // Garde hors scénario : un refresh_token absent reste absent (session OAuth valide sans refresh).
    #[test]
    fn oauth_session_accepts_absent_refresh_token() {
        let stored =
            oauth_session(device_tokens("@bot:example.org", None)).expect("session OAuth construite");
        let StoredSession::OAuth { user, .. } = stored else {
            panic!("variante OAuth attendue");
        };
        assert!(user.tokens.refresh_token.is_none(), "refresh_token inattendu");
    }

    // Garde hors scénario : user_id mal formé → InvalidId, comme import_session.
    #[test]
    fn oauth_session_refuses_malformed_user_id() {
        let result = oauth_session(device_tokens("pas un identifiant", None));
        assert!(
            matches!(result, Err(MatrixError::InvalidId(_))),
            "obtenu : {result:?}"
        );
    }

    // Garde hors scénario : ni la session ni les jetons bruts ne montrent un jeton en `Debug`.
    #[test]
    fn oauth_session_and_tokens_debug_hide_secrets() {
        let tokens = device_tokens("@bot:example.org", Some("tok-ref-secret"));
        let shown_tokens = format!("{tokens:?}");
        let stored = oauth_session(device_tokens("@bot:example.org", Some("tok-ref-secret")))
            .expect("session OAuth construite");
        let shown_session = format!("{stored:?}");
        for shown in [shown_tokens, shown_session] {
            assert!(
                !shown.contains("tok-acc-secret") && !shown.contains("tok-ref-secret"),
                "jeton affiché : {shown}"
            );
        }
    }

    // Garde hors scénario : la valeur `miryad.matrix.oauth_client` fait l'aller-retour en UTF-8.
    #[test]
    fn client_id_value_round_trips() {
        let bytes = encode_client_id("client-7");
        assert_eq!(
            decode_client_id(&bytes).expect("UTF-8 décodé").as_str(),
            "client-7"
        );
    }

    // Garde hors scénario : des octets non UTF-8 sont une erreur de store, pas un identifiant.
    #[test]
    fn client_id_value_rejects_non_utf8() {
        let result = decode_client_id(&[0xff, 0xfe]);
        assert!(
            matches!(result, Err(MatrixError::Store(StoreError::Corrupt(_)))),
            "obtenu : {result:?}"
        );
    }

    async fn fresh_store() -> SeaOrmCryptoStore {
        let db = fresh_db().await;
        SeaOrmCryptoStore::open(db, "acc-oauth", "passphrase-1")
            .await
            .expect("store opens")
    }

    // Garde hors scénario : valeur absente → None ; après écriture, l'identifiant est relu.
    #[tokio::test]
    async fn existing_client_id_is_absent_then_round_trips() {
        let store = fresh_store().await;
        let before = load_client_id(&store).await.expect("lecture sur store vide");
        assert_eq!(before, None);
        CryptoStore::set_custom_value(&store, OAUTH_CLIENT_KEY, encode_client_id("client-7"))
            .await
            .expect("valeur écrite");
        let after = load_client_id(&store).await.expect("lecture après écriture");
        assert_eq!(after, Some("client-7".to_owned()));
    }

    // Garde hors scénario : une valeur non UTF-8 en base est StoreError::Corrupt.
    #[tokio::test]
    async fn non_utf8_stored_client_id_is_store_corrupt() {
        let store = fresh_store().await;
        CryptoStore::set_custom_value(&store, OAUTH_CLIENT_KEY, vec![0xff, 0xfe])
            .await
            .expect("valeur brute écrite");
        let result = load_client_id(&store).await;
        assert!(
            matches!(result, Err(MatrixError::Store(StoreError::Corrupt(_)))),
            "obtenu : {result:?}"
        );
    }
}
