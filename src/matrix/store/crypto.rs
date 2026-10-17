//! `SeaOrmCryptoStore` : type public du store de chiffrement (`src/matrix/store/crypto.sdd`).
//!
//! Squelette provisoire : struct et `open` seulement, posés en avance par `crypto_account.sdd`.
//! L'`impl CryptoStore` et `store_config` viennent avec `crypto.sdd`.

use std::collections::HashMap;

use async_trait::async_trait;
use matrix_sdk::config::StoreConfig;
use matrix_sdk::cross_process_lock::{CrossProcessLockConfig, CrossProcessLockGeneration};
use matrix_sdk::ruma::events::secret::request::SecretName;
use matrix_sdk::ruma::{DeviceId, OwnedDeviceId, RoomId, TransactionId, UserId};
use matrix_sdk_crypto::olm::{
    Curve25519PublicKey, InboundGroupSession, OlmMessageHash, OutboundGroupSession,
    PrivateCrossSigningIdentity, SenderDataType, Session,
};
use matrix_sdk_crypto::store::CryptoStore;
use matrix_sdk_crypto::store::types::{
    BackupKeys, Changes, DehydratedDeviceKey, PendingChanges, RoomKeyCounts, RoomKeyWithheldEntry,
    RoomPendingKeyBundleDetails, RoomSettings, StoredRoomKeyBundleData, TrackedUser,
};
use matrix_sdk_crypto::{Account, DeviceData, GossipRequest, SecretInfo, UserIdentityData};
use sea_orm::{DatabaseConnection, DatabaseTransaction, TransactionTrait};
use zeroize::Zeroizing;

use super::error::StoreError;
use super::kv::Kv;

/// Store de chiffrement d'un compte `matrix-sdk` adossé à la base `SeaORM` de l'hôte.
#[derive(Clone)]
pub struct SeaOrmCryptoStore {
    pub(super) kv: Kv,
    pub(super) db: DatabaseConnection,
}

impl std::fmt::Debug for SeaOrmCryptoStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeaOrmCryptoStore").finish_non_exhaustive()
    }
}

impl SeaOrmCryptoStore {
    /// Ouvre le store du compte `account_key` ; le cipher est créé au premier appel.
    ///
    /// # Errors
    ///
    /// `StoreError::Cipher` si la passphrase ne déchiffre pas le cipher existant,
    /// `StoreError::Db` si une table manque (migrations non appliquées par l'hôte).
    pub async fn open(
        db: DatabaseConnection,
        account_key: &str,
        passphrase: &str,
    ) -> Result<Self, StoreError> {
        let kv = Kv::open(db.clone(), account_key, passphrase).await?;
        Ok(Self { kv, db })
    }

    /// Configuration des stores d'un `Client` : ce store pour le chiffrement, les stores
    /// mémoire de `matrix-sdk` pour l'état, les événements et les médias (`src/matrix/store/mod.sdd`).
    #[must_use]
    pub fn store_config(&self, lock: CrossProcessLockConfig) -> StoreConfig {
        StoreConfig::new(lock).crypto_store(self.clone())
    }
}

/// Délégation pure vers les fonctions inhérentes des fichiers de domaine (`crypto.sdd`, tableau
/// de délégation). Une seule implémentation par méthode : celle du fichier qui la porte.
#[async_trait]
impl CryptoStore for SeaOrmCryptoStore {
    type Error = StoreError;

    async fn load_account(&self) -> Result<Option<Account>, StoreError> {
        self.load_account().await
    }

    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>, StoreError> {
        self.load_identity().await
    }

    /// Une seule `DatabaseTransaction` : compte, puis sessions, puis appareils, puis `commit`.
    /// Une erreur abandonne la transaction (rollback implicite de `SeaORM`) : aucun effet partiel.
    async fn save_changes(&self, changes: Changes) -> Result<(), StoreError> {
        if changes.is_empty() {
            return Ok(());
        }
        let txn: DatabaseTransaction = self.db.begin().await?;
        self.apply_account(&txn, &changes).await?;
        self.apply_sessions(&txn, &changes).await?;
        self.apply_devices(&txn, &changes).await?;
        txn.commit().await?;
        Ok(())
    }

    async fn save_pending_changes(&self, changes: PendingChanges) -> Result<(), StoreError> {
        self.save_pending_changes(changes).await
    }

    async fn save_inbound_group_sessions(
        &self,
        sessions: Vec<InboundGroupSession>,
        backed_up_to_version: Option<&str>,
    ) -> Result<(), StoreError> {
        self.save_inbound_group_sessions(sessions, backed_up_to_version)
            .await
    }

    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Vec<Session>>, StoreError> {
        self.get_sessions(sender_key).await
    }

    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>, StoreError> {
        self.get_inbound_group_session(room_id, session_id).await
    }

    async fn get_withheld_info(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<RoomKeyWithheldEntry>, StoreError> {
        self.get_withheld_info(room_id, session_id).await
    }

    async fn get_withheld_sessions_by_room_id(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<RoomKeyWithheldEntry>, StoreError> {
        self.get_withheld_sessions_by_room_id(room_id).await
    }

    async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>, StoreError> {
        self.get_inbound_group_sessions().await
    }

    async fn inbound_group_session_counts(
        &self,
        backup_version: Option<&str>,
    ) -> Result<RoomKeyCounts, StoreError> {
        self.inbound_group_session_counts(backup_version).await
    }

    async fn get_inbound_group_sessions_by_room_id(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<InboundGroupSession>, StoreError> {
        self.get_inbound_group_sessions_by_room_id(room_id).await
    }

    async fn get_inbound_group_sessions_for_device_batch(
        &self,
        curve_key: Curve25519PublicKey,
        sender_data_type: SenderDataType,
        after_session_id: Option<String>,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>, StoreError> {
        self.get_inbound_group_sessions_for_device_batch(curve_key, sender_data_type, after_session_id, limit)
            .await
    }

    async fn inbound_group_sessions_for_backup(
        &self,
        backup_version: &str,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>, StoreError> {
        self.inbound_group_sessions_for_backup(backup_version, limit)
            .await
    }

    async fn mark_inbound_group_sessions_as_backed_up(
        &self,
        backup_version: &str,
        room_and_session_ids: &[(&RoomId, &str)],
    ) -> Result<(), StoreError> {
        self.mark_inbound_group_sessions_as_backed_up(backup_version, room_and_session_ids)
            .await
    }

    async fn reset_backup_state(&self) -> Result<(), StoreError> {
        self.reset_backup_state().await
    }

    async fn load_backup_keys(&self) -> Result<BackupKeys, StoreError> {
        self.load_backup_keys().await
    }

    async fn load_dehydrated_device_pickle_key(&self) -> Result<Option<DehydratedDeviceKey>, StoreError> {
        self.load_dehydrated_device_pickle_key().await
    }

    async fn delete_dehydrated_device_pickle_key(&self) -> Result<(), StoreError> {
        self.delete_dehydrated_device_pickle_key().await
    }

    async fn get_outbound_group_session(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OutboundGroupSession>, StoreError> {
        self.get_outbound_group_session(room_id).await
    }

    async fn load_tracked_users(&self) -> Result<Vec<TrackedUser>, StoreError> {
        self.load_tracked_users().await
    }

    async fn save_tracked_users(&self, users: &[(&UserId, bool)]) -> Result<(), StoreError> {
        self.save_tracked_users(users).await
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<DeviceData>, StoreError> {
        self.get_device(user_id, device_id).await
    }

    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, DeviceData>, StoreError> {
        self.get_user_devices(user_id).await
    }

    /// Composition : compte chargé, puis identité de l'appareil. Compte absent : erreur explicite.
    async fn get_own_device(&self) -> Result<DeviceData, StoreError> {
        let account = self
            .load_account()
            .await?
            .ok_or_else(|| StoreError::Corrupt("pas de compte".to_owned()))?;
        Ok(DeviceData::from_account(&account))
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<UserIdentityData>, StoreError> {
        self.get_user_identity(user_id).await
    }

    async fn is_message_known(&self, message_hash: &OlmMessageHash) -> Result<bool, StoreError> {
        self.is_message_known(message_hash).await
    }

    async fn get_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> Result<Option<GossipRequest>, StoreError> {
        self.get_outgoing_secret_requests(request_id).await
    }

    async fn get_secret_request_by_info(
        &self,
        secret_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>, StoreError> {
        self.get_secret_request_by_info(secret_info).await
    }

    async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>, StoreError> {
        self.get_unsent_secret_requests().await
    }

    async fn delete_outgoing_secret_requests(&self, request_id: &TransactionId) -> Result<(), StoreError> {
        self.delete_outgoing_secret_requests(request_id).await
    }

    async fn get_secrets_from_inbox(
        &self,
        secret_name: &SecretName,
    ) -> Result<Vec<Zeroizing<String>>, StoreError> {
        self.get_secrets_from_inbox(secret_name).await
    }

    async fn delete_secrets_from_inbox(&self, secret_name: &SecretName) -> Result<(), StoreError> {
        self.delete_secrets_from_inbox(secret_name).await
    }

    async fn get_room_settings(&self, room_id: &RoomId) -> Result<Option<RoomSettings>, StoreError> {
        self.get_room_settings(room_id).await
    }

    async fn get_received_room_key_bundle_data(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<StoredRoomKeyBundleData>, StoreError> {
        self.get_received_room_key_bundle_data(room_id, user_id).await
    }

    async fn get_pending_key_bundle_details_for_room(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<RoomPendingKeyBundleDetails>, StoreError> {
        self.get_pending_key_bundle_details_for_room(room_id).await
    }

    async fn get_all_rooms_pending_key_bundles(
        &self,
    ) -> Result<Vec<RoomPendingKeyBundleDetails>, StoreError> {
        self.get_all_rooms_pending_key_bundles().await
    }

    async fn has_downloaded_all_room_keys(&self, room_id: &RoomId) -> Result<bool, StoreError> {
        self.has_downloaded_all_room_keys(room_id).await
    }

    async fn get_custom_value(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        self.get_custom_value(key).await
    }

    async fn set_custom_value(&self, key: &str, value: Vec<u8>) -> Result<(), StoreError> {
        self.set_custom_value(key, value).await
    }

    async fn remove_custom_value(&self, key: &str) -> Result<(), StoreError> {
        self.remove_custom_value(key).await
    }

    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<Option<CrossProcessLockGeneration>, StoreError> {
        self.try_take_leased_lock(lease_duration_ms, key, holder).await
    }

    async fn next_batch_token(&self) -> Result<Option<String>, StoreError> {
        self.next_batch_token().await
    }

    async fn close(&self) -> Result<(), StoreError> {
        self.close().await
    }

    async fn reopen(&self) -> Result<(), StoreError> {
        self.reopen().await
    }

    async fn get_size(&self) -> Result<Option<usize>, StoreError> {
        self.get_size().await
    }
}

#[cfg(test)]
mod tests {
    use super::super::error::StoreError;
    use super::SeaOrmCryptoStore;
    use matrix_sdk::ruma::UserId;
    use matrix_sdk_crypto::olm::Session;
    use matrix_sdk_crypto::store::CryptoStore;
    use matrix_sdk_crypto::store::types::{Changes, DeviceChanges, PendingChanges};
    use matrix_sdk_crypto::{Account, DeviceData};
    use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;

    const PASS: &str = "passphrase-1";

    struct StoreMigrator;

    impl MigratorTrait for StoreMigrator {
        fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
            super::super::migration::migrations()
        }
    }

    /// Base SQLite mémoire migrée. Une seule connexion : sans elle, chaque connexion
    /// du pool verrait sa propre base mémoire.
    async fn fresh_db() -> DatabaseConnection {
        let mut opts = ConnectOptions::new("sqlite::memory:");
        opts.max_connections(1).min_connections(1);
        let db = Database::connect(opts).await.expect("in-memory sqlite connects");
        StoreMigrator::up(&db, None)
            .await
            .expect("store migrations apply over an empty base");
        db
    }

    async fn fresh_store(name: &str) -> SeaOrmCryptoStore {
        SeaOrmCryptoStore::open(fresh_db().await, name, PASS)
            .await
            .expect("store opens")
    }

    fn user(local: &str) -> matrix_sdk::ruma::OwnedUserId {
        UserId::parse(format!("@{local}:x")).expect("valid user id")
    }

    /// Session Olm sortante de `ours` vers `theirs` (un one-time key frais par session).
    fn outbound_olm_session(ours: &Account, theirs: &mut Account) -> Session {
        theirs.generate_one_time_keys(1);
        let one_time_keys = theirs.signed_one_time_keys();
        let device = DeviceData::from_account(theirs);
        ours.create_outbound_session(&device, &one_time_keys, ours.device_keys())
            .expect("olm session towards the other account")
    }

    // Scenario local : deux open sur la même base avec la même passphrase réussissent
    #[tokio::test]
    async fn two_opens_with_same_passphrase_succeed() {
        let db = fresh_db().await;
        SeaOrmCryptoStore::open(db.clone(), "acc", "pass")
            .await
            .expect("first open");
        SeaOrmCryptoStore::open(db, "acc", "pass")
            .await
            .expect("second open with the same passphrase");
    }

    // Scenario: save_changes est atomique
    // Given un Changes portant une session Olm et un appareil, et la table `devices` supprimée avant l'appel
    // When save_changes est appelé
    // Then le résultat est Err et get_sessions ne retrouve pas la session Olm (rollback)
    #[tokio::test]
    async fn save_changes_is_atomic() {
        let store = fresh_store("acc").await;
        let ours = Account::new(&user("alice"));
        let mut bob = Account::new(&user("bob"));
        let session = outbound_olm_session(&ours, &mut bob);
        let bob_key = bob.identity_keys().curve25519.to_base64();
        let new_device = DeviceData::from_account(&bob);
        store
            .save_pending_changes(PendingChanges { account: Some(ours) })
            .await
            .expect("save our account");
        // Table `devices` : `miryad_matrix_devices` (./migration.sdd, préfixe TABLE_PREFIX).
        store
            .db
            .execute_unprepared("DROP TABLE miryad_matrix_devices")
            .await
            .expect("drop the devices table");
        let changes = Changes {
            sessions: vec![session],
            devices: DeviceChanges {
                new: vec![new_device],
                changed: vec![],
                deleted: vec![],
            },
            ..Default::default()
        };
        let result = CryptoStore::save_changes(&store, changes).await;
        assert!(result.is_err(), "save_changes doit échouer sans la table devices");
        assert!(
            store
                .get_sessions(&bob_key)
                .await
                .expect("read sessions after rollback")
                .is_none(),
            "rollback : la session Olm n'est pas persistée"
        );
    }

    // Must : Changes vide = succès sans écriture
    #[tokio::test]
    async fn empty_changes_succeed_without_writing() {
        let store = fresh_store("acc").await;
        CryptoStore::save_changes(&store, Changes::default())
            .await
            .expect("empty changes are accepted");
        assert!(
            CryptoStore::load_account(&store)
                .await
                .expect("load account")
                .is_none(),
            "aucun compte écrit"
        );
    }

    // Scenario: deux comptes cohabitent dans une base
    #[tokio::test]
    async fn two_accounts_share_a_database_without_mixing() {
        let db = fresh_db().await;
        let store_a = SeaOrmCryptoStore::open(db.clone(), "a", PASS)
            .await
            .expect("open a");
        let store_b = SeaOrmCryptoStore::open(db, "b", PASS).await.expect("open b");
        let acc_a = Account::new(&user("alice"));
        let acc_b = Account::new(&user("bob"));
        let key_a = acc_a.identity_keys().curve25519;
        let key_b = acc_b.identity_keys().curve25519;
        store_a
            .save_pending_changes(PendingChanges { account: Some(acc_a) })
            .await
            .expect("save a");
        store_b
            .save_pending_changes(PendingChanges { account: Some(acc_b) })
            .await
            .expect("save b");
        let loaded_a = CryptoStore::load_account(&store_a)
            .await
            .expect("load a")
            .expect("account a present");
        let loaded_b = CryptoStore::load_account(&store_b)
            .await
            .expect("load b")
            .expect("account b present");
        assert_eq!(loaded_a.identity_keys().curve25519, key_a);
        assert_eq!(loaded_b.identity_keys().curve25519, key_b);
        assert_ne!(loaded_a.identity_keys().curve25519, key_b);
    }

    // Scenario: la passphrase protège les données
    #[tokio::test]
    async fn wrong_passphrase_is_a_cipher_error() {
        let db = fresh_db().await;
        let store = SeaOrmCryptoStore::open(db.clone(), "acc", "p1")
            .await
            .expect("open with p1");
        store
            .save_pending_changes(PendingChanges {
                account: Some(Account::new(&user("alice"))),
            })
            .await
            .expect("save account with p1");
        let err = SeaOrmCryptoStore::open(db, "acc", "p2")
            .await
            .expect_err("second open with p2 must fail");
        assert!(
            matches!(err, StoreError::Cipher(_)),
            "variante inattendue : {err:?}"
        );
    }
}
