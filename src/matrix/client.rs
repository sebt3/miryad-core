//! Client Matrix de bot (`src/matrix/client.sdd`) : connexion par session existante ou importée,
//! persistance des rotations de jetons, identifiant du bot.
//!
//! Le client est une valeur `Clone` construite une fois, partagée entre la boucle de sync, les steps
//! de workflow et le code de l'application. Aucune requête réseau n'est émise à la construction hors
//! restauration de la session ; le démarrage de la synchronisation reste à `run_sync`.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use matrix_sdk::AuthSession;
use matrix_sdk::SessionChange;
use matrix_sdk::SessionMeta;
use matrix_sdk::SessionTokens;
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::authentication::oauth::{ClientId, OAuthSession};
use matrix_sdk::cross_process_lock::CrossProcessLockConfig;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::{OwnedDeviceId, UserId};
use sea_orm::DatabaseConnection;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::matrix::config::MatrixConfig;
use crate::matrix::error::{MatrixError, from_sdk};
use crate::matrix::session::{self, StoredSession};
use crate::matrix::store::SeaOrmCryptoStore;

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
    use super::{MatrixClient, MessageKind, SessionImport, build_content};
    use crate::matrix::config::MatrixConfig;
    use crate::matrix::error::MatrixError;
    use matrix_sdk::ruma::events::room::message::{MessageType, RoomMessageEventContent};
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
}
