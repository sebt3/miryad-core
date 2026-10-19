//! Store de chiffrement `matrix-sdk` sur `SeaORM`, conçu pour être extrait (`src/matrix/store/mod.sdd`).
//!
//! # Promesse
//!
//! Porter l'état de chiffrement de bout en bout (E2EE) d'un client `matrix-sdk` dans la base de
//! données de l'application, au lieu du store SQLite livré avec `matrix-sdk`. Seul le store de
//! chiffrement est persistant : clés Olm et Megolm, identités, appareils et secrets. Les stores
//! d'état de salon, de cache d'événements et de médias restent en mémoire (`MemoryStore` de
//! `matrix-sdk`) ; un bot refait donc un sync initial au démarrage.
//!
//! Le module ne connaît ni la configuration, ni le client, ni les entités de l'application : il
//! reçoit une connexion (`DatabaseConnection`), une clé de compte (`account_key`) et une
//! passphrase, et rend un [`SeaOrmCryptoStore`] dont [`SeaOrmCryptoStore::store_config`] assemble
//! la `StoreConfig` d'un `Client`.
//!
//! Contraintes qui gardent le module extractible, toutes testées : aucun chemin `crate::` hors de
//! `crate::matrix::store` (`tests/matrix_store_isolation.rs`), erreurs propres ([`StoreError`],
//! codes `MRD-MXSTORE-*`), migrations livrées par le module et assemblées par l'hôte, préfixe de
//! table unique ([`TABLE_PREFIX`]), clé de compte dans chaque table.
//!
//! # Extraction
//!
//! Le module est écrit pour devenir une crate à part (`matrix-sdk-crypto-sea-orm`, nom provisoire)
//! par simple déplacement du répertoire. À l'extraction : renommer la crate, changer le préfixe
//! ([`TABLE_PREFIX`]) et les codes d'erreur `MRD-MXSTORE-*`, puis ajouter son propre CI.
//!
//! # Déploiement
//!
//! - La base de données de l'application est déjà présente (Postgres en production, SQLite en
//!   test) ; le store n'ouvre aucune connexion par lui-même.
//! - L'hôte applique les migrations de [`migrations()`] (via `miryad_core::migration::Migrator`,
//!   sous la feature `matrix`) avant toute ouverture du store.
//! - `matrix-sdk` est épinglé en `0.18` : la version `0.19` et suivantes imposent
//!   `libsqlite3-sys` 0.38, en conflit avec le `sqlx-sqlite` de `SeaORM` (`links = "sqlite3"`).
//!   La feature `sqlite` de `matrix-sdk` n'est jamais activée (`cargo tree -i matrix-sdk-sqlite`
//!   ne trouve rien). Détail et plan d'extraction : `docs/architecture.md`.

pub mod error;
pub mod migration;

mod crypto;
mod crypto_account;
mod crypto_devices;
mod crypto_sessions;
mod kv;

#[cfg(test)]
mod test_support;

pub use crypto::SeaOrmCryptoStore;
pub use error::StoreError;
pub use migration::migrations;

/// Préfixe unique des tables du store (`src/matrix/store/mod.sdd`, `TABLE_PREFIX`).
pub const TABLE_PREFIX: &str = "miryad_matrix_";
