//! `SeaOrmCryptoStore` : type public du store de chiffrement (`src/matrix/store/crypto.sdd`).
//!
//! Squelette provisoire : struct et `open` seulement, posés en avance par `crypto_account.sdd`.
//! L'`impl CryptoStore` et `store_config` viennent avec `crypto.sdd`.

use sea_orm::DatabaseConnection;

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
}

#[cfg(test)]
mod tests {
    use super::SeaOrmCryptoStore;
    use sea_orm::{ConnectOptions, Database};
    use sea_orm_migration::MigratorTrait;

    struct StoreMigrator;

    impl MigratorTrait for StoreMigrator {
        fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
            super::super::migration::migrations()
        }
    }

    // Scenario local : deux open sur la même base avec la même passphrase réussissent
    #[tokio::test]
    async fn two_opens_with_same_passphrase_succeed() {
        let mut opts = ConnectOptions::new("sqlite::memory:");
        opts.max_connections(1).min_connections(1);
        let db = Database::connect(opts).await.expect("in-memory sqlite connects");
        StoreMigrator::up(&db, None)
            .await
            .expect("store migrations apply");
        SeaOrmCryptoStore::open(db.clone(), "acc", "pass")
            .await
            .expect("first open");
        SeaOrmCryptoStore::open(db, "acc", "pass")
            .await
            .expect("second open with the same passphrase");
    }
}
