//! Persistance de la session Matrix du bot dans le store de chiffrement (`src/matrix/session.sdd`).
//!
//! La session est une valeur personnalisée du store (`SESSION_KEY`), scellée par le `StoreCipher`
//! du compte : jamais écrite en clair, jamais dans un fichier. `Debug` ne montre aucun jeton.

use std::fmt;

use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::authentication::oauth::{ClientId, OAuthSession, UserSession};
use matrix_sdk_crypto::store::CryptoStore;
use serde::{Deserialize, Serialize};

use crate::matrix::error::MatrixError;
use crate::matrix::store::{SeaOrmCryptoStore, StoreError};

/// Clé de la valeur personnalisée qui porte la session (`src/matrix/session.sdd`).
pub(crate) const SESSION_KEY: &str = "miryad.matrix.session";

/// Session persistée : forme « Matrix » à jeton importé, ou forme OAuth 2.0 (device code).
///
/// `OAuthSession` de `matrix-sdk` 0.18 n'est pas sérialisable ; sa forme persistée reprend ses
/// deux champs, dont `UserSession` (sérialisable) porte les jetons.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub(crate) enum StoredSession {
    /// Session à jeton importé (repli de compatibilité).
    Matrix(MatrixSession),
    /// Session OAuth 2.0 : identifiant de client et session utilisateur.
    OAuth {
        /// Identifiant de client OAuth obtenu à l'enregistrement.
        client_id: String,
        /// Session utilisateur (identité d'appareil et jetons).
        user: UserSession,
    },
}

impl fmt::Debug for StoredSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Matrix(session) => f
                .debug_struct("StoredSession::Matrix")
                .field("user_id", &session.meta.user_id)
                .field("device_id", &session.meta.device_id)
                .finish_non_exhaustive(),
            Self::OAuth { client_id, user } => f
                .debug_struct("StoredSession::OAuth")
                .field("client_id", client_id)
                .field("user_id", &user.meta.user_id)
                .field("device_id", &user.meta.device_id)
                .finish_non_exhaustive(),
        }
    }
}

impl From<&OAuthSession> for StoredSession {
    fn from(session: &OAuthSession) -> Self {
        Self::OAuth {
            client_id: session.client_id.as_str().to_owned(),
            user: session.user.clone(),
        }
    }
}

impl TryFrom<StoredSession> for OAuthSession {
    /// La session stockée, rendue intacte, quand elle n'est pas de la forme OAuth.
    type Error = StoredSession;

    fn try_from(stored: StoredSession) -> Result<Self, Self::Error> {
        match stored {
            StoredSession::OAuth { client_id, user } => Ok(Self {
                client_id: ClientId::new(client_id),
                user,
            }),
            other @ StoredSession::Matrix(_) => Err(other),
        }
    }
}

/// Lit la session du compte. `Ok(None)` seulement si aucune session n'a été enregistrée ; une
/// valeur illisible est `MatrixError::Store`, jamais une absence.
pub(crate) async fn load(store: &SeaOrmCryptoStore) -> Result<Option<StoredSession>, MatrixError> {
    let Some(bytes) = CryptoStore::get_custom_value(store, SESSION_KEY).await? else {
        return Ok(None);
    };
    // Seule la position de l'erreur de décodage est remontée, jamais le contenu.
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|err| MatrixError::Store(StoreError::Corrupt(format!("session illisible : {err}"))))
}

/// Enregistre la session en remplaçant l'existante.
pub(crate) async fn save(store: &SeaOrmCryptoStore, session: &StoredSession) -> Result<(), MatrixError> {
    let bytes = serde_json::to_vec(session).map_err(|err| StoreError::Serialization(err.to_string()))?;
    CryptoStore::set_custom_value(store, SESSION_KEY, bytes).await?;
    Ok(())
}

/// Efface la session du compte.
pub(crate) async fn clear(store: &SeaOrmCryptoStore) -> Result<(), MatrixError> {
    CryptoStore::remove_custom_value(store, SESSION_KEY).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{SESSION_KEY, StoredSession, clear, load, save};
    use crate::matrix::error::MatrixError;
    use crate::matrix::store::SeaOrmCryptoStore;
    use assert_matches::assert_matches;
    use matrix_sdk::SessionMeta;
    use matrix_sdk::SessionTokens;
    use matrix_sdk::authentication::matrix::MatrixSession;
    use matrix_sdk::authentication::oauth::{ClientId, OAuthSession, UserSession};
    use matrix_sdk::ruma::{OwnedDeviceId, UserId};
    use matrix_sdk_crypto::store::CryptoStore;
    use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
    use sea_orm_migration::MigratorTrait;

    const ACC: &str = "acc-session";
    const PASS: &str = "passphrase-1";

    struct StoreMigrator;

    impl MigratorTrait for StoreMigrator {
        fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
            crate::matrix::store::migrations()
        }
    }

    /// Base SQLite mémoire migrée. Une seule connexion : sinon chaque connexion du pool
    /// verrait sa propre base mémoire (même pattern que `store/crypto.rs`).
    async fn fresh_db() -> DatabaseConnection {
        let mut opts = ConnectOptions::new("sqlite::memory:");
        opts.max_connections(1).min_connections(1);
        let db = Database::connect(opts).await.expect("in-memory sqlite connects");
        StoreMigrator::up(&db, None)
            .await
            .expect("store migrations apply over an empty base");
        db
    }

    async fn fresh_store() -> (SeaOrmCryptoStore, DatabaseConnection) {
        let db = fresh_db().await;
        let store = SeaOrmCryptoStore::open(db.clone(), ACC, PASS)
            .await
            .expect("store opens");
        (store, db)
    }

    fn session_meta(device: &str) -> SessionMeta {
        SessionMeta {
            user_id: UserId::parse("@bot:x").expect("valid user id"),
            device_id: OwnedDeviceId::from(device),
        }
    }

    fn tokens(access: &str) -> SessionTokens {
        SessionTokens {
            access_token: access.to_owned(),
            refresh_token: Some("ref-456".to_owned()),
        }
    }

    fn matrix_session(access: &str) -> MatrixSession {
        MatrixSession {
            meta: session_meta("DEVMATRIX"),
            tokens: tokens(access),
        }
    }

    fn oauth_session(access: &str) -> OAuthSession {
        OAuthSession {
            client_id: ClientId::new("client-789".to_owned()),
            user: UserSession {
                meta: session_meta("DEVOAUTH"),
                tokens: tokens(access),
            },
        }
    }

    /// Forme JSON d'une session : comparaison des sessions OAuth, qui ne sont pas `PartialEq`.
    fn json(session: &StoredSession) -> String {
        serde_json::to_string(session).expect("stored session serializes")
    }

    // Scenario: une session OAuth fait l'aller-retour
    #[tokio::test]
    async fn oauth_session_round_trips() {
        let (store, _db) = fresh_store().await;
        let original = StoredSession::from(&oauth_session("tok-123"));
        save(&store, &original).await.expect("save succeeds");
        let loaded = load(&store)
            .await
            .expect("load succeeds")
            .expect("a session was saved");
        assert_eq!(json(&loaded), json(&original));
        let restored = OAuthSession::try_from(loaded).expect("the OAuth variant comes back");
        assert_eq!(restored.client_id.as_str(), "client-789");
        assert_eq!(restored.user.meta.device_id.as_str(), "DEVOAUTH");
    }

    // Scenario: une session Matrix fait l'aller-retour
    #[tokio::test]
    async fn matrix_session_round_trips() {
        let (store, _db) = fresh_store().await;
        save(&store, &StoredSession::Matrix(matrix_session("tok-123")))
            .await
            .expect("save succeeds");
        let loaded = load(&store).await.expect("load succeeds");
        assert_matches!(loaded, Some(StoredSession::Matrix(session)) => {
            assert_eq!(session, matrix_session("tok-123"));
        });
    }

    // Scenario: save remplace, clear efface
    #[tokio::test]
    async fn save_replaces_and_clear_removes() {
        let (store, _db) = fresh_store().await;
        save(&store, &StoredSession::Matrix(matrix_session("tok-1")))
            .await
            .expect("first save");
        let second = StoredSession::from(&oauth_session("tok-2"));
        save(&store, &second)
            .await
            .expect("second save replaces the first");
        let loaded = load(&store)
            .await
            .expect("load succeeds")
            .expect("second session present");
        assert_eq!(json(&loaded), json(&second));
        clear(&store).await.expect("clear succeeds");
        let after_clear = load(&store).await.expect("load after clear succeeds");
        assert!(after_clear.is_none(), "obtenu après clear : {after_clear:?}");
    }

    // Scenario: une valeur illisible est une erreur
    #[tokio::test]
    async fn unreadable_value_is_an_error_not_an_absence() {
        let (store, _db) = fresh_store().await;
        CryptoStore::set_custom_value(&store, SESSION_KEY, b"pas du json".to_vec())
            .await
            .expect("raw value stored");
        let result = load(&store).await;
        assert!(
            matches!(result, Err(MatrixError::Store(_))),
            "attendu MatrixError::Store, obtenu : {result:?}"
        );
    }

    // Scenario: la session n'est pas en clair en base
    #[tokio::test]
    async fn session_is_not_stored_in_clear() {
        let (store, db) = fresh_store().await;
        save(&store, &StoredSession::Matrix(matrix_session("tok-123")))
            .await
            .expect("save succeeds");
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT value FROM miryad_matrix_kv WHERE account_key = ? AND key = ?",
                [ACC.into(), format!("custom:{SESSION_KEY}").into()],
            ))
            .await
            .expect("row query succeeds")
            .expect("the session row exists in kv");
        let raw: Vec<u8> = row.try_get("", "value").expect("value column is readable");
        assert!(
            !String::from_utf8_lossy(&raw).contains("tok-123"),
            "le jeton apparaît en clair dans la ligne kv"
        );
    }

    // Garde hors scénario : `Debug` manuel ne montre aucun jeton, ni d'accès ni de rafraîchissement.
    #[test]
    fn debug_never_shows_tokens() {
        let sessions = [
            StoredSession::Matrix(matrix_session("tok-123")),
            StoredSession::from(&oauth_session("tok-123")),
        ];
        for session in sessions {
            let shown = format!("{session:?}");
            assert!(!shown.contains("tok-123"), "jeton d'accès affiché : {shown}");
            assert!(
                !shown.contains("ref-456"),
                "jeton de rafraîchissement affiché : {shown}"
            );
        }
    }
}
