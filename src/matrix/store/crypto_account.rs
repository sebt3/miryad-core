// Portions de ce fichier (schéma des tables, formats de sérialisation et sémantique des opérations) suivent
// `matrix-sdk-sqlite` 0.18.0, réimplémentés sur SeaORM.
// Copyright 2022, 2026 The Matrix.org Foundation C.I.C.
// Licensed under the Apache License, Version 2.0 (http://www.apache.org/licenses/LICENSE-2.0).
// Modifié pour miryad-core : réécriture sur SeaORM (SQLite et PostgreSQL), autre découpage en fichiers.
// Le reste du fichier est sous la licence du dépôt (BSD-3-Clause), voir LICENSE et NOTICE.

//! Partie « compte local » du store de chiffrement : compte Olm, identité de cross-signing,
//! clés de sauvegarde, valeurs personnalisées, jeton `next_batch` et verrou à bail
//! (`src/matrix/store/crypto_account.sdd`). Fonctions inhérentes de `SeaOrmCryptoStore` ;
//! l'`impl CryptoStore` les délègue (`src/matrix/store/crypto.sdd`).

use matrix_sdk_crypto::Account;
use matrix_sdk_crypto::olm::{PickledAccount, PickledCrossSigningIdentity, PrivateCrossSigningIdentity};
use matrix_sdk_crypto::store::types::{
    BackupDecryptionKey, BackupKeys, Changes, DehydratedDeviceKey, PendingChanges,
};
use sea_orm::sea_query::{Alias, Expr, ExprTrait, OnConflict, Query};
use sea_orm::{ConnectionTrait, DatabaseTransaction, DbErr, StatementBuilder, TransactionTrait};
use serde::{Serialize, de::DeserializeOwned};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::crypto::SeaOrmCryptoStore;
use super::error::StoreError;

/// Clés du stockage `kv` du compte (`src/matrix/store/crypto_account.sdd`, `Must`).
const KEY_ACCOUNT: &str = "account";
const KEY_IDENTITY: &str = "identity";
const KEY_BACKUP_VERSION: &str = "backup_version";
const KEY_BACKUP_DECRYPTION_KEY: &str = "backup_decryption_key";
const KEY_DEHYDRATED_PICKLE_KEY: &str = "dehydrated_pickle_key";
const KEY_NEXT_BATCH: &str = "next_batch";
/// Préfixe des valeurs personnalisées : `custom:{key}`.
const CUSTOM_PREFIX: &str = "custom:";
/// Table des baux, préfixée par `TABLE_PREFIX` (`src/matrix/store/migration.sdd`).
const LEASES_TABLE: &str = "miryad_matrix_leases";

/// Horloge du processus en millisecondes depuis l'époque ; repli 0 avant l'époque.
fn now_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

impl SeaOrmCryptoStore {
    /// Lit une valeur scellée de `kv` et la désérialise.
    async fn get_sealed<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, StoreError> {
        match self.kv.get(&self.db, key).await? {
            Some(bytes) => Ok(Some(self.kv.open_sealed(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Sérialise, scelle et écrit `value` sous `key` dans `conn`.
    async fn put_sealed<T: Serialize>(
        &self,
        conn: &impl ConnectionTrait,
        key: &str,
        value: &T,
    ) -> Result<(), StoreError> {
        let sealed = self.kv.seal(value)?;
        self.kv.put(conn, key, sealed).await
    }

    /// Lit le compte (`None` si le store est neuf).
    pub(super) async fn load_account(&self) -> Result<Option<Account>, StoreError> {
        match self.get_sealed::<PickledAccount>(KEY_ACCOUNT).await? {
            Some(pickle) => Account::from_pickle(pickle)
                .map(Some)
                .map_err(|err| StoreError::Corrupt(err.to_string())),
            None => Ok(None),
        }
    }

    /// Lit l'identité de cross-signing privée (`None` si le store est neuf).
    pub(super) async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>, StoreError> {
        match self
            .get_sealed::<PickledCrossSigningIdentity>(KEY_IDENTITY)
            .await?
        {
            Some(pickle) => PrivateCrossSigningIdentity::from_pickle(pickle)
                .map(Some)
                .map_err(|err| StoreError::Corrupt(err.to_string())),
            None => Ok(None),
        }
    }

    /// Enregistre les changements en attente (compte seul) dans une transaction.
    pub(super) async fn save_pending_changes(&self, changes: PendingChanges) -> Result<(), StoreError> {
        let Some(account) = changes.account else {
            return Ok(());
        };
        let txn = self.db.begin().await?;
        self.save_account(&txn, &account).await?;
        txn.commit().await?;
        Ok(())
    }

    /// Écrit le compte scellé dans la transaction reçue.
    pub(super) async fn save_account(
        &self,
        txn: &DatabaseTransaction,
        account: &Account,
    ) -> Result<(), StoreError> {
        self.put_sealed(txn, KEY_ACCOUNT, &account.pickle()).await
    }

    /// Applique les champs de `Changes` relevant de ce fichier, dans la transaction reçue.
    pub(super) async fn apply_account(
        &self,
        txn: &DatabaseTransaction,
        changes: &Changes,
    ) -> Result<(), StoreError> {
        if let Some(identity) = &changes.private_identity {
            self.put_sealed(txn, KEY_IDENTITY, &identity.pickle().await)
                .await?;
        }
        if let Some(version) = &changes.backup_version {
            self.put_sealed(txn, KEY_BACKUP_VERSION, version).await?;
        }
        if let Some(key) = &changes.backup_decryption_key {
            self.put_sealed(txn, KEY_BACKUP_DECRYPTION_KEY, key).await?;
        }
        if let Some(key) = &changes.dehydrated_device_pickle_key {
            self.put_sealed(txn, KEY_DEHYDRATED_PICKLE_KEY, key).await?;
        }
        if let Some(token) = &changes.next_batch_token {
            self.put_sealed(txn, KEY_NEXT_BATCH, token).await?;
        }
        Ok(())
    }

    /// Clés de sauvegarde (version et clé de déchiffrement), vides sur un store neuf.
    pub(super) async fn load_backup_keys(&self) -> Result<BackupKeys, StoreError> {
        Ok(BackupKeys {
            decryption_key: self
                .get_sealed::<BackupDecryptionKey>(KEY_BACKUP_DECRYPTION_KEY)
                .await?,
            backup_version: self.get_sealed::<String>(KEY_BACKUP_VERSION).await?,
        })
    }

    /// Clé de pickle de l'appareil déshydraté, si présente.
    pub(super) async fn load_dehydrated_device_pickle_key(
        &self,
    ) -> Result<Option<DehydratedDeviceKey>, StoreError> {
        self.get_sealed(KEY_DEHYDRATED_PICKLE_KEY).await
    }

    /// Supprime la clé de pickle de l'appareil déshydraté.
    pub(super) async fn delete_dehydrated_device_pickle_key(&self) -> Result<(), StoreError> {
        self.kv.delete(&self.db, KEY_DEHYDRATED_PICKLE_KEY).await
    }

    /// Lit une valeur personnalisée.
    pub(super) async fn get_custom_value(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        self.get_sealed(&format!("{CUSTOM_PREFIX}{key}")).await
    }

    /// Écrit une valeur personnalisée (opaque, scellée).
    pub(super) async fn set_custom_value(&self, key: &str, value: Vec<u8>) -> Result<(), StoreError> {
        self.put_sealed(&self.db, &format!("{CUSTOM_PREFIX}{key}"), &value)
            .await
    }

    /// Supprime une valeur personnalisée.
    pub(super) async fn remove_custom_value(&self, key: &str) -> Result<(), StoreError> {
        self.kv.delete(&self.db, &format!("{CUSTOM_PREFIX}{key}")).await
    }

    /// Prend ou prolonge le bail `key` pour `holder` ; `Some(génération)` si obtenu.
    ///
    /// Obtenu si aucun bail n'existe (génération 1), si le bail est expiré (génération + 1 si
    /// le détenteur change), ou si `holder` le détient déjà (prolongation, génération inchangée).
    /// Atomicité entre processus : l'insertion conditionnelle puis l'`UPDATE ... RETURNING`
    /// décident sur le nombre de lignes, sans lecture préalable.
    ///
    /// Hypothèse : l'horloge de chaque réplica (`SystemTime` du processus) est synchronisée à
    /// ±1 s, très en deçà d'un bail de plusieurs dizaines de secondes. Une horloge antérieure à
    /// l'époque donne `now = 0` (les baux existants ne expirent pas tant que l'horloge n'est pas
    /// corrigée).
    pub(super) async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<Option<u64>, StoreError> {
        let expiration = now_ms().saturating_add(i64::from(lease_duration_ms));
        let account_key = self.kv.account_key();
        let backend = self.db.get_database_backend();

        // 1. Pas de bail : insertion conditionnelle, génération initiale 1.
        let insert = Query::insert()
            .into_table(Alias::new(LEASES_TABLE))
            .columns([
                Alias::new("account_key"),
                Alias::new("key"),
                Alias::new("holder"),
                Alias::new("expiration_ms"),
                Alias::new("generation"),
            ])
            .values([
                Expr::val(account_key),
                Expr::val(key),
                Expr::val(holder),
                Expr::val(expiration),
                Expr::val(1_i64),
            ])
            .map_err(|err| StoreError::Db(DbErr::Custom(err.to_string())))?
            .on_conflict(
                OnConflict::columns([Alias::new("account_key"), Alias::new("key")])
                    .do_nothing()
                    .to_owned(),
            )
            .to_owned();
        let inserted = self
            .db
            .execute_raw(StatementBuilder::build(&insert, &backend))
            .await?;
        if inserted.rows_affected() == 1 {
            return Ok(Some(1));
        }

        // 2. Bail existant : prise (expiré ou détenteur identique) ou refus. Les expressions de
        // SET lisent l'ancienne ligne : `holder` dans le CASE est le détenteur précédent.
        let update = Query::update()
            .table(Alias::new(LEASES_TABLE))
            .value(Alias::new("holder"), holder)
            .value(Alias::new("expiration_ms"), expiration)
            .value(
                Alias::new("generation"),
                Expr::col(Alias::new("generation"))
                    .add(Expr::case(Expr::col(Alias::new("holder")).eq(holder), 0_i64).finally(1_i64)),
            )
            .and_where(Expr::col(Alias::new("account_key")).eq(account_key))
            .and_where(Expr::col(Alias::new("key")).eq(key))
            .and_where(
                Expr::col(Alias::new("holder"))
                    .eq(holder)
                    .or(Expr::col(Alias::new("expiration_ms")).lte(now_ms())),
            )
            .returning_col(Alias::new("generation"))
            .to_owned();
        match self
            .db
            .query_one_raw(StatementBuilder::build(&update, &backend))
            .await?
        {
            Some(row) => {
                let generation: i64 = row.try_get("", "generation")?;
                u64::try_from(generation)
                    .map(Some)
                    .map_err(|_| StoreError::Corrupt("génération de bail négative".into()))
            }
            None => Ok(None),
        }
    }

    /// Jeton `next_batch` des requêtes to-device, s'il existe.
    pub(super) async fn next_batch_token(&self) -> Result<Option<String>, StoreError> {
        self.get_sealed(KEY_NEXT_BATCH).await
    }

    /// Sans effet : la connexion appartient à l'hôte (`src/matrix/store/mod.sdd`).
    // async imposé par la délégation du trait CryptoStore (src/matrix/store/crypto.sdd, Must).
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub(super) async fn close(&self) -> Result<(), StoreError> {
        Ok(())
    }

    /// Sans effet : la connexion appartient à l'hôte (`src/matrix/store/mod.sdd`).
    // async imposé par la délégation du trait CryptoStore (src/matrix/store/crypto.sdd, Must).
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub(super) async fn reopen(&self) -> Result<(), StoreError> {
        Ok(())
    }

    /// Taille du store : non supportée, `None` (autorisé par le trait).
    // async imposé par la délégation du trait CryptoStore (src/matrix/store/crypto.sdd, Must).
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub(super) async fn get_size(&self) -> Result<Option<usize>, StoreError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::super::crypto::SeaOrmCryptoStore;
    use matrix_sdk_crypto::store::types::Changes;
    use sea_orm::TransactionTrait;
    use sea_orm_migration::MigratorTrait;
    use std::time::Duration;

    const PASS: &str = "passphrase-1";

    struct StoreMigrator;

    impl MigratorTrait for StoreMigrator {
        fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
            super::super::migration::migrations()
        }
    }

    /// Store neuf sur SQLite mémoire migrée. Une seule connexion : sans elle, chaque
    /// connexion du pool verrait sa propre base mémoire.
    async fn fresh_store(name: &str) -> SeaOrmCryptoStore {
        let db = super::super::test_support::test_db().await;
        StoreMigrator::up(&db, None)
            .await
            .expect("store migrations apply over an empty base");
        SeaOrmCryptoStore::open(db, name, PASS)
            .await
            .expect("store opens")
    }

    // Scenario: un bail concurrent n'est accordé qu'à un détenteur
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_lease_is_granted_to_exactly_one_holder() {
        let store = fresh_store("acc").await;
        let (sa, sb) = (store.clone(), store.clone());
        let ta = tokio::spawn(async move { sa.try_take_leased_lock(30_000, "k", "a").await });
        let tb = tokio::spawn(async move { sb.try_take_leased_lock(30_000, "k", "b").await });
        let ra = ta.await.expect("task a joins").expect("lease a is a valid call");
        let rb = tb.await.expect("task b joins").expect("lease b is a valid call");
        let granted = [ra, rb].iter().filter(|r| r.is_some()).count();
        assert_eq!(granted, 1, "exactement un détenteur obtient le bail");
    }

    // Scenario: un bail expiré est repris, avec une génération supérieure
    #[tokio::test]
    async fn expired_lease_is_taken_over_with_higher_generation() {
        let store = fresh_store("acc").await;
        let ga = store
            .try_take_leased_lock(1, "k", "a")
            .await
            .expect("a takes the lease")
            .expect("a obtains a 1 ms lease on an empty key");
        tokio::time::sleep(Duration::from_millis(5)).await;
        let gb = store
            .try_take_leased_lock(30_000, "k", "b")
            .await
            .expect("b tries the lease")
            .expect("b obtains the expired lease");
        assert!(gb > ga, "génération de b ({gb}) > celle de a ({ga})");
    }

    // Scenario: le détenteur prolonge sans changer de génération
    #[tokio::test]
    async fn holder_extends_lease_without_changing_generation() {
        let store = fresh_store("acc").await;
        let g1 = store
            .try_take_leased_lock(30_000, "k", "a")
            .await
            .expect("first take")
            .expect("first take obtains the lease");
        let g2 = store
            .try_take_leased_lock(30_000, "k", "a")
            .await
            .expect("extension")
            .expect("the holder keeps the lease");
        assert_eq!(g1, g2, "même détenteur, même génération");
    }

    // Scenario: les valeurs personnalisées font l'aller-retour
    #[tokio::test]
    async fn custom_values_round_trip() {
        let store = fresh_store("acc").await;
        store
            .set_custom_value("x", vec![1, 2, 3])
            .await
            .expect("set custom value");
        assert_eq!(
            store.get_custom_value("x").await.expect("get custom value"),
            Some(vec![1, 2, 3])
        );
        store.remove_custom_value("x").await.expect("remove custom value");
        assert_eq!(store.get_custom_value("x").await.expect("get after remove"), None);
    }

    // Scenario: un store neuf n'a ni compte ni identité
    // Handles: les autres load_* d'un store neuf rendent les valeurs par défaut, sans erreur
    #[tokio::test]
    async fn new_store_has_no_account_nor_identity_and_default_loads() {
        let store = fresh_store("acc").await;
        assert!(store.load_account().await.expect("load_account").is_none());
        assert!(store.load_identity().await.expect("load_identity").is_none());
        let keys = store.load_backup_keys().await.expect("load_backup_keys");
        assert!(keys.decryption_key.is_none(), "pas de clé de sauvegarde");
        assert!(keys.backup_version.is_none(), "pas de version de sauvegarde");
        assert!(
            store
                .load_dehydrated_device_pickle_key()
                .await
                .expect("load_dehydrated_device_pickle_key")
                .is_none()
        );
        assert!(
            store
                .next_batch_token()
                .await
                .expect("next_batch_token")
                .is_none()
        );
    }

    // Handles : `apply_account` n'écrit que dans la transaction reçue ; sans commit, rien n'est visible
    #[tokio::test]
    async fn apply_writes_only_inside_the_given_transaction() {
        let store = fresh_store("acc").await;
        let changes = Changes {
            next_batch_token: Some("t1".to_owned()),
            ..Default::default()
        };
        let txn = store.db.begin().await.expect("begin");
        store
            .apply_account(&txn, &changes)
            .await
            .expect("apply inside the transaction");
        txn.rollback().await.expect("rollback");
        assert!(
            store
                .next_batch_token()
                .await
                .expect("read after rollback")
                .is_none(),
            "rollback : rien d'écrit"
        );
        let txn = store.db.begin().await.expect("begin");
        store.apply_account(&txn, &changes).await.expect("apply");
        txn.commit().await.expect("commit");
        assert_eq!(
            store.next_batch_token().await.expect("read after commit"),
            Some("t1".to_owned())
        );
    }

    // Handles : supprimer une clé absente n'est pas une erreur
    #[tokio::test]
    async fn removing_an_absent_custom_value_is_not_an_error() {
        let store = fresh_store("acc").await;
        store
            .remove_custom_value("absent")
            .await
            .expect("delete of an absent key succeeds");
        assert!(store.get_custom_value("absent").await.expect("read").is_none());
    }
}
