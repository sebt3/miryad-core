//! Partie « annuaire et coordination » du store de chiffrement : appareils, identités, utilisateurs
//! suivis, empreintes de messages, requêtes et boîte de secrets, paramètres de salon, bundles de
//! clés (`src/matrix/store/crypto_devices.sdd`). Fonctions inhérentes de `SeaOrmCryptoStore` ;
//! l'`impl CryptoStore` les délègue (`src/matrix/store/crypto.sdd`).

use std::collections::HashMap;

use matrix_sdk::ruma::events::secret::request::SecretName;
use matrix_sdk::ruma::{DeviceId, OwnedDeviceId, RoomId, TransactionId, UserId};
use matrix_sdk_crypto::olm::OlmMessageHash;
use matrix_sdk_crypto::store::types::{
    Changes, RoomPendingKeyBundleDetails, RoomSettings, StoredRoomKeyBundleData, TrackedUser,
};
use matrix_sdk_crypto::{DeviceData, GossipRequest, SecretInfo, UserIdentityData};
use sea_orm::sea_query::{Alias, Expr, ExprTrait, Func, OnConflict, Order, Query, SelectStatement};
use sea_orm::{ConnectionTrait, DatabaseTransaction, DbErr, QueryResult, StatementBuilder, TransactionTrait};
use serde::de::DeserializeOwned;
use zeroize::Zeroizing;

use super::crypto::SeaOrmCryptoStore;
use super::error::StoreError;

/// Tables de l'annuaire, nom complet (`TABLE_PREFIX` + suffixe, `src/matrix/store/migration.sdd`).
const DEVICES_TABLE: &str = "miryad_matrix_devices";
const IDENTITIES_TABLE: &str = "miryad_matrix_identities";
const TRACKED_TABLE: &str = "miryad_matrix_tracked_users";
const MESSAGE_HASHES_TABLE: &str = "miryad_matrix_message_hashes";
const SECRET_REQUESTS_TABLE: &str = "miryad_matrix_secret_requests";
const SECRETS_INBOX_TABLE: &str = "miryad_matrix_secrets_inbox";
const ROOM_SETTINGS_TABLE: &str = "miryad_matrix_room_settings";
const ROOM_KEY_BUNDLES_TABLE: &str = "miryad_matrix_room_key_bundles";
const PENDING_BUNDLES_TABLE: &str = "miryad_matrix_rooms_pending_key_bundle";
const ROOM_KEYS_DOWNLOADED_TABLE: &str = "miryad_matrix_room_keys_downloaded";

/// Lit les lignes d'une requête (sur le store ou une transaction).
async fn query_rows(
    conn: &impl ConnectionTrait,
    select: &SelectStatement,
) -> Result<Vec<QueryResult>, StoreError> {
    let backend = conn.get_database_backend();
    Ok(conn
        .query_all_raw(StatementBuilder::build(select, &backend))
        .await?)
}

/// Exécute une écriture sur `conn`.
async fn execute(conn: &impl ConnectionTrait, stmt: &impl StatementBuilder) -> Result<(), StoreError> {
    let backend = conn.get_database_backend();
    conn.execute_raw(StatementBuilder::build(stmt, &backend)).await?;
    Ok(())
}

/// Insère ou remplace une ligne : conflit sur `primary_key`, les colonnes hors clé sont mises à jour.
async fn upsert(
    conn: &impl ConnectionTrait,
    table: &str,
    columns: &[&str],
    values: Vec<Expr>,
    primary_key: &[&str],
) -> Result<(), StoreError> {
    let updated = columns
        .iter()
        .filter(|column| !primary_key.contains(column))
        .map(|column| Alias::new(*column));
    let stmt = Query::insert()
        .into_table(Alias::new(table))
        .columns(columns.iter().map(|column| Alias::new(*column)))
        .values(values)
        .map_err(|err| StoreError::Db(DbErr::Custom(err.to_string())))?
        .on_conflict(
            OnConflict::columns(primary_key.iter().map(|column| Alias::new(*column)))
                .update_columns(updated)
                .to_owned(),
        )
        .to_owned();
    execute(conn, &stmt).await
}

/// Insère une ligne si la clé primaire est libre ; sans effet sinon.
async fn insert_ignore(
    conn: &impl ConnectionTrait,
    table: &str,
    columns: &[&str],
    values: Vec<Expr>,
    primary_key: &[&str],
) -> Result<(), StoreError> {
    let stmt = Query::insert()
        .into_table(Alias::new(table))
        .columns(columns.iter().map(|column| Alias::new(*column)))
        .values(values)
        .map_err(|err| StoreError::Db(DbErr::Custom(err.to_string())))?
        .on_conflict(
            OnConflict::columns(primary_key.iter().map(|column| Alias::new(*column)))
                .do_nothing()
                .to_owned(),
        )
        .to_owned();
    execute(conn, &stmt).await
}

impl SeaOrmCryptoStore {
    /// Hachage de recherche d'une valeur pour `table` (`Kv::hash`).
    fn dev_hashed(&self, table: &str, value: &str) -> String {
        self.kv.hash(table, value)
    }

    /// Requête sur `table` restreinte au compte, projetant `columns`.
    fn dev_scoped(&self, table: &str, columns: &[&str]) -> SelectStatement {
        let mut select = Query::select();
        for column in columns {
            select.column(Alias::new(*column));
        }
        select
            .from(Alias::new(table))
            .and_where(Expr::col(Alias::new("account_key")).eq(self.kv.account_key()))
            .to_owned()
    }

    /// Premier `payload` de la requête, déchiffré ; `None` si aucune ligne.
    async fn one_payload<T: DeserializeOwned>(
        &self,
        select: &SelectStatement,
    ) -> Result<Option<T>, StoreError> {
        let rows = query_rows(&self.db, select).await?;
        match rows.first() {
            Some(row) => {
                let sealed: Vec<u8> = row.try_get("", "payload")?;
                Ok(Some(self.kv.open_sealed(&sealed)?))
            }
            None => Ok(None),
        }
    }

    /// Tous les `payload` de la requête, déchiffrés dans l'ordre de la requête.
    async fn all_payloads<T: DeserializeOwned>(
        &self,
        select: &SelectStatement,
    ) -> Result<Vec<T>, StoreError> {
        let rows = query_rows(&self.db, select).await?;
        rows.iter()
            .map(|row| -> Result<T, StoreError> {
                let sealed: Vec<u8> = row.try_get("", "payload")?;
                self.kv.open_sealed(&sealed)
            })
            .collect()
    }

    /// Utilisateurs suivis et leur indicateur `dirty`.
    pub(super) async fn load_tracked_users(&self) -> Result<Vec<TrackedUser>, StoreError> {
        let select = self.dev_scoped(TRACKED_TABLE, &["payload"]);
        self.all_payloads(&select).await
    }

    /// Remplace l'entrée de chaque utilisateur donné.
    pub(super) async fn save_tracked_users(&self, users: &[(&UserId, bool)]) -> Result<(), StoreError> {
        let txn = self.db.begin().await?;
        for &(user, dirty) in users {
            let sealed = self.kv.seal(&TrackedUser {
                user_id: user.to_owned(),
                dirty,
            })?;
            upsert(
                &txn,
                TRACKED_TABLE,
                &["account_key", "user_id", "dirty", "payload"],
                vec![
                    Expr::val(self.kv.account_key()),
                    Expr::val(self.dev_hashed(TRACKED_TABLE, user.as_str())),
                    Expr::val(dirty),
                    Expr::val(sealed),
                ],
                &["account_key", "user_id"],
            )
            .await?;
        }
        txn.commit().await?;
        Ok(())
    }

    /// Appareil `device_id` de `user_id`, s'il existe.
    pub(super) async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<DeviceData>, StoreError> {
        let mut select = self.dev_scoped(DEVICES_TABLE, &["payload"]);
        select
            .and_where(Expr::col(Alias::new("user_id")).eq(self.dev_hashed(DEVICES_TABLE, user_id.as_str())))
            .and_where(
                Expr::col(Alias::new("device_id")).eq(self.dev_hashed(DEVICES_TABLE, device_id.as_str())),
            );
        self.one_payload(&select).await
    }

    /// Tous les appareils de `user_id`, indexés par identifiant d'appareil.
    pub(super) async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, DeviceData>, StoreError> {
        let mut select = self.dev_scoped(DEVICES_TABLE, &["payload"]);
        select
            .and_where(Expr::col(Alias::new("user_id")).eq(self.dev_hashed(DEVICES_TABLE, user_id.as_str())));
        let devices: Vec<DeviceData> = self.all_payloads(&select).await?;
        Ok(devices
            .into_iter()
            .map(|device| (device.device_id().to_owned(), device))
            .collect())
    }

    /// Identité de `user_id`, si elle existe.
    pub(super) async fn get_user_identity(
        &self,
        user_id: &UserId,
    ) -> Result<Option<UserIdentityData>, StoreError> {
        let mut select = self.dev_scoped(IDENTITIES_TABLE, &["payload"]);
        select.and_where(
            Expr::col(Alias::new("user_id")).eq(self.dev_hashed(IDENTITIES_TABLE, user_id.as_str())),
        );
        self.one_payload(&select).await
    }

    /// Vrai si l'empreinte de ce message Olm a déjà été vue.
    pub(super) async fn is_message_known(&self, message_hash: &OlmMessageHash) -> Result<bool, StoreError> {
        let mut select = self.dev_scoped(MESSAGE_HASHES_TABLE, &["hash"]);
        select.and_where(
            Expr::col(Alias::new("hash")).eq(self.dev_hashed(MESSAGE_HASHES_TABLE, &message_hash.hash)),
        );
        Ok(!query_rows(&self.db, &select).await?.is_empty())
    }

    /// Requête sortante `request_id`, si elle existe.
    pub(super) async fn get_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> Result<Option<GossipRequest>, StoreError> {
        let mut select = self.dev_scoped(SECRET_REQUESTS_TABLE, &["payload"]);
        select.and_where(
            Expr::col(Alias::new("request_id"))
                .eq(self.dev_hashed(SECRET_REQUESTS_TABLE, request_id.as_str())),
        );
        self.one_payload(&select).await
    }

    /// Requête sortante pour `secret_info`, si elle existe.
    pub(super) async fn get_secret_request_by_info(
        &self,
        secret_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>, StoreError> {
        let mut select = self.dev_scoped(SECRET_REQUESTS_TABLE, &["payload"]);
        select.and_where(
            Expr::col(Alias::new("info_key"))
                .eq(self.dev_hashed(SECRET_REQUESTS_TABLE, &secret_info.as_key())),
        );
        self.one_payload(&select).await
    }

    /// Requêtes sortantes jamais envoyées.
    pub(super) async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>, StoreError> {
        let mut select = self.dev_scoped(SECRET_REQUESTS_TABLE, &["payload"]);
        select.and_where(Expr::col(Alias::new("sent_out")).eq(false));
        self.all_payloads(&select).await
    }

    /// Supprime la requête sortante `request_id`.
    pub(super) async fn delete_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> Result<(), StoreError> {
        let stmt = Query::delete()
            .from_table(Alias::new(SECRET_REQUESTS_TABLE))
            .and_where(Expr::col(Alias::new("account_key")).eq(self.kv.account_key()))
            .and_where(
                Expr::col(Alias::new("request_id"))
                    .eq(self.dev_hashed(SECRET_REQUESTS_TABLE, request_id.as_str())),
            )
            .to_owned();
        execute(&self.db, &stmt).await
    }

    /// Secrets reçus pour `secret_name`, dans l'ordre d'arrivée.
    pub(super) async fn get_secrets_from_inbox(
        &self,
        secret_name: &SecretName,
    ) -> Result<Vec<Zeroizing<String>>, StoreError> {
        let mut select = self.dev_scoped(SECRETS_INBOX_TABLE, &["payload"]);
        select
            .and_where(
                Expr::col(Alias::new("secret_name"))
                    .eq(self.dev_hashed(SECRETS_INBOX_TABLE, secret_name.as_str())),
            )
            .order_by(Alias::new("seq"), Order::Asc);
        let rows = query_rows(&self.db, &select).await?;
        rows.iter()
            .map(|row| -> Result<Zeroizing<String>, StoreError> {
                let sealed: Vec<u8> = row.try_get("", "payload")?;
                let secret: String = self.kv.open_sealed(&sealed)?;
                Ok(Zeroizing::new(secret))
            })
            .collect()
    }

    /// Vide la boîte de secrets pour `secret_name`.
    pub(super) async fn delete_secrets_from_inbox(&self, secret_name: &SecretName) -> Result<(), StoreError> {
        let stmt = Query::delete()
            .from_table(Alias::new(SECRETS_INBOX_TABLE))
            .and_where(Expr::col(Alias::new("account_key")).eq(self.kv.account_key()))
            .and_where(
                Expr::col(Alias::new("secret_name"))
                    .eq(self.dev_hashed(SECRETS_INBOX_TABLE, secret_name.as_str())),
            )
            .to_owned();
        execute(&self.db, &stmt).await
    }

    /// Paramètres de chiffrement du salon, s'ils existent.
    pub(super) async fn get_room_settings(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<RoomSettings>, StoreError> {
        let mut select = self.dev_scoped(ROOM_SETTINGS_TABLE, &["payload"]);
        select.and_where(
            Expr::col(Alias::new("room_id")).eq(self.dev_hashed(ROOM_SETTINGS_TABLE, room_id.as_str())),
        );
        self.one_payload(&select).await
    }

    /// Bundle de clés reçu de `user_id` pour `room_id`, s'il existe.
    pub(super) async fn get_received_room_key_bundle_data(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<StoredRoomKeyBundleData>, StoreError> {
        let mut select = self.dev_scoped(ROOM_KEY_BUNDLES_TABLE, &["payload"]);
        select
            .and_where(
                Expr::col(Alias::new("room_id"))
                    .eq(self.dev_hashed(ROOM_KEY_BUNDLES_TABLE, room_id.as_str())),
            )
            .and_where(
                Expr::col(Alias::new("sender_user_id"))
                    .eq(self.dev_hashed(ROOM_KEY_BUNDLES_TABLE, user_id.as_str())),
            );
        self.one_payload(&select).await
    }

    /// Détails du bundle en attente pour `room_id`, s'il y en a un.
    pub(super) async fn get_pending_key_bundle_details_for_room(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<RoomPendingKeyBundleDetails>, StoreError> {
        let mut select = self.dev_scoped(PENDING_BUNDLES_TABLE, &["payload"]);
        select.and_where(
            Expr::col(Alias::new("room_id")).eq(self.dev_hashed(PENDING_BUNDLES_TABLE, room_id.as_str())),
        );
        self.one_payload(&select).await
    }

    /// Détails de tous les bundles en attente.
    pub(super) async fn get_all_rooms_pending_key_bundles(
        &self,
    ) -> Result<Vec<RoomPendingKeyBundleDetails>, StoreError> {
        let select = self.dev_scoped(PENDING_BUNDLES_TABLE, &["payload"]);
        self.all_payloads(&select).await
    }

    /// Vrai si toutes les clés de `room_id` ont été téléchargées depuis la sauvegarde.
    pub(super) async fn has_downloaded_all_room_keys(&self, room_id: &RoomId) -> Result<bool, StoreError> {
        let mut select = self.dev_scoped(ROOM_KEYS_DOWNLOADED_TABLE, &["room_id"]);
        select.and_where(
            Expr::col(Alias::new("room_id"))
                .eq(self.dev_hashed(ROOM_KEYS_DOWNLOADED_TABLE, room_id.as_str())),
        );
        Ok(!query_rows(&self.db, &select).await?.is_empty())
    }

    /// Prochain numéro de séquence de la boîte `secret_hash`, calculé dans `txn` : `max(seq) + 1`.
    async fn next_secret_seq(&self, txn: &DatabaseTransaction, secret_hash: &str) -> Result<i64, StoreError> {
        let backend = txn.get_database_backend();
        let stmt = Query::select()
            .expr_as(Func::max(Expr::col(Alias::new("seq"))), Alias::new("max_seq"))
            .from(Alias::new(SECRETS_INBOX_TABLE))
            .and_where(Expr::col(Alias::new("account_key")).eq(self.kv.account_key()))
            .and_where(Expr::col(Alias::new("secret_name")).eq(secret_hash))
            .to_owned();
        let row = txn
            .query_one_raw(StatementBuilder::build(&stmt, &backend))
            .await?;
        let max: Option<i64> = match row {
            Some(row) => row.try_get("", "max_seq")?,
            None => None,
        };
        match max {
            None => Ok(0),
            Some(max) => max
                .checked_add(1)
                .ok_or_else(|| StoreError::Corrupt("numéro de séquence saturé".into())),
        }
    }

    /// Écrit les appareils, identités, empreintes, requêtes, secrets, paramètres, bundles et
    /// indicateurs de `changes` dans `txn`.
    pub(super) async fn apply_devices(
        &self,
        txn: &DatabaseTransaction,
        changes: &Changes,
    ) -> Result<(), StoreError> {
        self.apply_hashes_and_requests(txn, changes).await?;
        self.apply_identities_and_devices(txn, changes).await?;
        self.apply_room_settings_and_secrets(txn, changes).await?;
        self.apply_room_key_bundles(txn, changes).await
    }
    /// Empreintes de messages et requêtes de secrets sortantes de `changes`, dans `txn`.
    async fn apply_hashes_and_requests(
        &self,
        txn: &DatabaseTransaction,
        changes: &Changes,
    ) -> Result<(), StoreError> {
        let account = self.kv.account_key();
        for hash in &changes.message_hashes {
            insert_ignore(
                txn,
                MESSAGE_HASHES_TABLE,
                &["account_key", "hash"],
                vec![
                    Expr::val(account),
                    Expr::val(self.dev_hashed(MESSAGE_HASHES_TABLE, &hash.hash)),
                ],
                &["account_key", "hash"],
            )
            .await?;
        }
        for request in &changes.key_requests {
            let sealed = self.kv.seal(request)?;
            upsert(
                txn,
                SECRET_REQUESTS_TABLE,
                &["account_key", "request_id", "info_key", "sent_out", "payload"],
                vec![
                    Expr::val(account),
                    Expr::val(self.dev_hashed(SECRET_REQUESTS_TABLE, request.request_id.as_str())),
                    Expr::val(self.dev_hashed(SECRET_REQUESTS_TABLE, &request.info.as_key())),
                    Expr::val(request.sent_out),
                    Expr::val(sealed),
                ],
                &["account_key", "request_id"],
            )
            .await?;
        }
        Ok(())
    }

    /// Identités, appareils nouveaux, modifiés et supprimés de `changes`, dans `txn`.
    async fn apply_identities_and_devices(
        &self,
        txn: &DatabaseTransaction,
        changes: &Changes,
    ) -> Result<(), StoreError> {
        let account = self.kv.account_key();
        let identities = changes
            .identities
            .new
            .iter()
            .chain(&changes.identities.changed)
            .chain(&changes.identities.unchanged);
        for identity in identities {
            let sealed = self.kv.seal(identity)?;
            upsert(
                txn,
                IDENTITIES_TABLE,
                &["account_key", "user_id", "payload"],
                vec![
                    Expr::val(account),
                    Expr::val(self.dev_hashed(IDENTITIES_TABLE, identity.user_id().as_str())),
                    Expr::val(sealed),
                ],
                &["account_key", "user_id"],
            )
            .await?;
        }
        for device in changes.devices.new.iter().chain(&changes.devices.changed) {
            let sealed = self.kv.seal(device)?;
            upsert(
                txn,
                DEVICES_TABLE,
                &["account_key", "user_id", "device_id", "payload"],
                vec![
                    Expr::val(account),
                    Expr::val(self.dev_hashed(DEVICES_TABLE, device.user_id().as_str())),
                    Expr::val(self.dev_hashed(DEVICES_TABLE, device.device_id().as_str())),
                    Expr::val(sealed),
                ],
                &["account_key", "user_id", "device_id"],
            )
            .await?;
        }
        for device in &changes.devices.deleted {
            let stmt = Query::delete()
                .from_table(Alias::new(DEVICES_TABLE))
                .and_where(Expr::col(Alias::new("account_key")).eq(account))
                .and_where(
                    Expr::col(Alias::new("user_id"))
                        .eq(self.dev_hashed(DEVICES_TABLE, device.user_id().as_str())),
                )
                .and_where(
                    Expr::col(Alias::new("device_id"))
                        .eq(self.dev_hashed(DEVICES_TABLE, device.device_id().as_str())),
                )
                .to_owned();
            execute(txn, &stmt).await?;
        }
        Ok(())
    }

    /// Paramètres de salon et secrets reçus de `changes`, dans `txn`.
    async fn apply_room_settings_and_secrets(
        &self,
        txn: &DatabaseTransaction,
        changes: &Changes,
    ) -> Result<(), StoreError> {
        let account = self.kv.account_key();
        for (room_id, settings) in &changes.room_settings {
            let sealed = self.kv.seal(settings)?;
            upsert(
                txn,
                ROOM_SETTINGS_TABLE,
                &["account_key", "room_id", "payload"],
                vec![
                    Expr::val(account),
                    Expr::val(self.dev_hashed(ROOM_SETTINGS_TABLE, room_id.as_str())),
                    Expr::val(sealed),
                ],
                &["account_key", "room_id"],
            )
            .await?;
        }
        for item in &changes.secrets {
            let secret_hash = self.dev_hashed(SECRETS_INBOX_TABLE, item.secret_name.as_str());
            let seq = self.next_secret_seq(txn, &secret_hash).await?;
            let sealed = self.kv.seal(&*item.secret)?;
            execute(
                txn,
                &Query::insert()
                    .into_table(Alias::new(SECRETS_INBOX_TABLE))
                    .columns([
                        Alias::new("account_key"),
                        Alias::new("secret_name"),
                        Alias::new("seq"),
                        Alias::new("payload"),
                    ])
                    .values([
                        Expr::val(account),
                        Expr::val(secret_hash),
                        Expr::val(seq),
                        Expr::val(sealed),
                    ])
                    .map_err(|err| StoreError::Db(DbErr::Custom(err.to_string())))?
                    .to_owned(),
            )
            .await?;
        }
        Ok(())
    }

    /// Bundles reçus, salons téléchargés et bundles en attente de `changes`, dans `txn`.
    async fn apply_room_key_bundles(
        &self,
        txn: &DatabaseTransaction,
        changes: &Changes,
    ) -> Result<(), StoreError> {
        let account = self.kv.account_key();
        for bundle in &changes.received_room_key_bundles {
            let sealed = self.kv.seal(bundle)?;
            upsert(
                txn,
                ROOM_KEY_BUNDLES_TABLE,
                &["account_key", "room_id", "sender_user_id", "payload"],
                vec![
                    Expr::val(account),
                    Expr::val(self.dev_hashed(ROOM_KEY_BUNDLES_TABLE, bundle.bundle_data.room_id.as_str())),
                    Expr::val(self.dev_hashed(ROOM_KEY_BUNDLES_TABLE, bundle.sender_user.as_str())),
                    Expr::val(sealed),
                ],
                &["account_key", "room_id", "sender_user_id"],
            )
            .await?;
        }
        for room_id in &changes.room_key_backups_fully_downloaded {
            insert_ignore(
                txn,
                ROOM_KEYS_DOWNLOADED_TABLE,
                &["account_key", "room_id"],
                vec![
                    Expr::val(account),
                    Expr::val(self.dev_hashed(ROOM_KEYS_DOWNLOADED_TABLE, room_id.as_str())),
                ],
                &["account_key", "room_id"],
            )
            .await?;
        }
        for (room_id, details) in &changes.rooms_pending_key_bundle {
            let room_hash = self.dev_hashed(PENDING_BUNDLES_TABLE, room_id.as_str());
            if let Some(details) = details {
                let sealed = self.kv.seal(details)?;
                upsert(
                    txn,
                    PENDING_BUNDLES_TABLE,
                    &["account_key", "room_id", "payload"],
                    vec![Expr::val(account), Expr::val(room_hash), Expr::val(sealed)],
                    &["account_key", "room_id"],
                )
                .await?;
            } else {
                let stmt = Query::delete()
                    .from_table(Alias::new(PENDING_BUNDLES_TABLE))
                    .and_where(Expr::col(Alias::new("account_key")).eq(account))
                    .and_where(Expr::col(Alias::new("room_id")).eq(room_hash))
                    .to_owned();
                execute(txn, &stmt).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::crypto::SeaOrmCryptoStore;
    use matrix_sdk::ruma::events::secret::request::SecretName;
    use matrix_sdk::ruma::{MilliSecondsSinceUnixEpoch, OwnedRoomId, RoomId, TransactionId, UserId};
    use matrix_sdk_crypto::olm::OlmMessageHash;
    use matrix_sdk_crypto::store::types::{
        Changes, DeviceChanges, RoomPendingKeyBundleDetails, SecretsInboxItem,
    };
    use matrix_sdk_crypto::{Account, DeviceData, GossipRequest, SecretInfo};
    use sea_orm::{ConnectOptions, ConnectionTrait, Database, DbBackend, Statement, TransactionTrait};
    use sea_orm_migration::MigratorTrait;
    use std::collections::HashMap;
    use zeroize::Zeroizing;

    const PASS: &str = "passphrase-1";
    const ROOM: &str = "!r:x";

    struct StoreMigrator;

    impl MigratorTrait for StoreMigrator {
        fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
            super::super::migration::migrations()
        }
    }

    /// Store neuf sur SQLite mémoire migrée. Une seule connexion : sans elle, chaque
    /// connexion du pool verrait sa propre base mémoire.
    async fn fresh_store() -> SeaOrmCryptoStore {
        let mut opts = ConnectOptions::new("sqlite::memory:");
        opts.max_connections(1).min_connections(1);
        let db = Database::connect(opts).await.expect("in-memory sqlite connects");
        StoreMigrator::up(&db, None)
            .await
            .expect("store migrations apply over an empty base");
        SeaOrmCryptoStore::open(db, "acc", PASS)
            .await
            .expect("store opens")
    }

    fn user(local: &str) -> matrix_sdk::ruma::OwnedUserId {
        UserId::parse(format!("@{local}:x")).expect("valid user id")
    }

    /// Appareil `device_id` de `owner`, créé avec l'API publique du compte.
    fn device(owner: &UserId, device_id: &str) -> DeviceData {
        let account = Account::with_device_id(owner, device_id.into());
        DeviceData::from_account(&account)
    }

    /// Applique `changes` dans une transaction committée.
    async fn apply(store: &SeaOrmCryptoStore, changes: Changes) {
        let txn = store.db.begin().await.expect("begin");
        store.apply_devices(&txn, &changes).await.expect("apply_devices");
        txn.commit().await.expect("commit");
    }

    fn only_new_devices(devices: Vec<DeviceData>) -> Changes {
        Changes {
            devices: DeviceChanges {
                new: devices,
                changed: vec![],
                deleted: vec![],
            },
            ..Default::default()
        }
    }

    // Scenario: un appareil enregistré est retrouvé, puis supprimé
    #[tokio::test]
    async fn device_is_found_then_removed() {
        let store = fresh_store().await;
        let owner = user("u");
        let d1 = device(&owner, "D1");
        apply(&store, only_new_devices(vec![d1.clone()])).await;
        let found = store
            .get_device(&owner, "D1".into())
            .await
            .expect("get_device after apply");
        assert!(found.is_some(), "l'appareil enregistré est retrouvé");
        let changes = Changes {
            devices: DeviceChanges {
                new: vec![],
                changed: vec![],
                deleted: vec![d1],
            },
            ..Default::default()
        };
        apply(&store, changes).await;
        let gone = store
            .get_device(&owner, "D1".into())
            .await
            .expect("get_device after delete");
        assert!(gone.is_none(), "l'appareil supprimé n'est plus rendu");
    }

    // Scenario: get_user_devices ne rend que les appareils de l'utilisateur
    #[tokio::test]
    async fn get_user_devices_returns_only_that_users_devices() {
        let store = fresh_store().await;
        let a = user("a");
        let b = user("b");
        apply(
            &store,
            only_new_devices(vec![device(&a, "A1"), device(&a, "A2"), device(&b, "B1")]),
        )
        .await;
        let devices: HashMap<_, _> = store.get_user_devices(&a).await.expect("get_user_devices");
        let mut ids: Vec<String> = devices.keys().map(ToString::to_string).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["A1".to_owned(), "A2".to_owned()],
            "exactement les deux de @a:x"
        );
    }

    // Scenario: les empreintes de messages se retrouvent
    #[tokio::test]
    async fn message_hashes_are_found_and_others_are_not() {
        let store = fresh_store().await;
        let known = OlmMessageHash {
            sender_key: "k".to_owned(),
            hash: "h1".to_owned(),
        };
        let changes = Changes {
            message_hashes: vec![known.clone()],
            ..Default::default()
        };
        apply(&store, changes).await;
        let other = OlmMessageHash {
            sender_key: "k".to_owned(),
            hash: "h2".to_owned(),
        };
        assert!(
            store.is_message_known(&known).await.expect("known probe"),
            "hash vu"
        );
        assert!(
            !store.is_message_known(&other).await.expect("other probe"),
            "hash inconnu"
        );
    }

    // Scenario: les requêtes de secrets non envoyées sont filtrées
    #[tokio::test]
    async fn only_unsent_secret_requests_are_returned() {
        let store = fresh_store().await;
        let recipient = user("a");
        let unsent = GossipRequest {
            request_recipient: recipient.clone(),
            request_id: TransactionId::new(),
            info: SecretInfo::SecretRequest("s1".into()),
            sent_out: false,
        };
        let sent = GossipRequest {
            request_recipient: recipient,
            request_id: TransactionId::new(),
            info: SecretInfo::SecretRequest("s2".into()),
            sent_out: true,
        };
        let changes = Changes {
            key_requests: vec![unsent.clone(), sent],
            ..Default::default()
        };
        apply(&store, changes).await;
        let rendered = store.get_unsent_secret_requests().await.expect("unsent");
        assert_eq!(rendered.len(), 1, "seule la requête non envoyée est rendue");
        assert_eq!(rendered[0].request_id, unsent.request_id);
    }

    // Scenario: la boîte de secrets rend dans l'ordre et se vide
    #[tokio::test]
    async fn secrets_inbox_returns_in_order_then_empties() {
        let store = fresh_store().await;
        let secret_name: SecretName = "n".into();
        let changes = Changes {
            secrets: vec![
                SecretsInboxItem {
                    secret_name: secret_name.clone(),
                    secret: Zeroizing::new("first".to_owned()),
                },
                SecretsInboxItem {
                    secret_name: secret_name.clone(),
                    secret: Zeroizing::new("second".to_owned()),
                },
            ],
            ..Default::default()
        };
        apply(&store, changes).await;
        let got: Vec<String> = store
            .get_secrets_from_inbox(&secret_name)
            .await
            .expect("first read")
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            got,
            vec!["first".to_owned(), "second".to_owned()],
            "ordre d'arrivée"
        );
        store
            .delete_secrets_from_inbox(&secret_name)
            .await
            .expect("delete");
        assert!(
            store
                .get_secrets_from_inbox(&secret_name)
                .await
                .expect("read after delete")
                .is_empty(),
            "la boîte est vide"
        );
    }

    // Scenario: les utilisateurs suivis font l'aller-retour
    #[tokio::test]
    async fn tracked_users_round_trip_with_hashed_user_id_in_database() {
        let store = fresh_store().await;
        let u = user("u");
        let v = user("v");
        store
            .save_tracked_users(&[(&u, true), (&v, false)])
            .await
            .expect("save_tracked_users");
        let loaded = store.load_tracked_users().await.expect("load_tracked_users");
        assert_eq!(loaded.len(), 2, "deux utilisateurs suivis");
        let found_u = loaded.iter().find(|t| t.user_id == u).expect("u is loaded");
        let found_v = loaded.iter().find(|t| t.user_id == v).expect("v is loaded");
        assert!(found_u.dirty, "dirty de @u:x");
        assert!(!found_v.dirty, "dirty de @v:x");
        let rows = store
            .db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT user_id FROM miryad_matrix_tracked_users WHERE account_key = ?",
                ["acc".into()],
            ))
            .await
            .expect("raw select");
        assert_eq!(rows.len(), 2, "deux lignes en base");
        for row in rows {
            let stored: String = row.try_get("", "user_id").expect("user_id column");
            assert_ne!(stored, u.as_str(), "user_id haché, jamais en clair");
            assert_ne!(stored, v.as_str(), "user_id haché, jamais en clair");
        }
    }

    // Scenario: un bundle en attente apparaît puis disparaît
    #[tokio::test]
    async fn pending_key_bundle_appears_then_disappears() {
        let store = fresh_store().await;
        let room: OwnedRoomId = RoomId::parse(ROOM).expect("valid room id");
        let details = RoomPendingKeyBundleDetails {
            room_id: room.clone(),
            invite_accepted_at: MilliSecondsSinceUnixEpoch::now(),
            inviter: user("inv"),
        };
        let mut pending = HashMap::new();
        pending.insert(room.clone(), Some(details));
        apply(
            &store,
            Changes {
                rooms_pending_key_bundle: pending,
                ..Default::default()
            },
        )
        .await;
        let first = store
            .get_pending_key_bundle_details_for_room(&room)
            .await
            .expect("first probe");
        assert!(first.is_some(), "le bundle en attente est rendu");
        let mut cleared = HashMap::new();
        cleared.insert(room.clone(), None);
        apply(
            &store,
            Changes {
                rooms_pending_key_bundle: cleared,
                ..Default::default()
            },
        )
        .await;
        let second = store
            .get_pending_key_bundle_details_for_room(&room)
            .await
            .expect("second probe");
        assert!(second.is_none(), "le bundle supprimé n'est plus rendu");
    }
}
