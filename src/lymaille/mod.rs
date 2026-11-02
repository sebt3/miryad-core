//! Hôte miryad-core de la bibliothèque `lymaille` — squelette du lot 1 (`src/lymaille/mod.sdd`,
//! Solution `dsd/miryad-core/lymaille.md`). La feature `lymaille` implique `db`. Les enfants du
//! module (stores SeaORM, identité, secrets, module des ressources) sont déclarés au fil du lot.

/// Erreurs de l'hôte `lymaille` : un code `MRD-LYMAILLE-NNN` par variante et la décision de
/// retryabilité consommée par le step `subagent` (`src/lymaille/error.sdd`).
pub mod error;

/// Résolution des références d'authentification des providers (`src/lymaille/secret.sdd`).
pub mod secret;

/// Identité de l'appelant : principal reconstruit par appel, préfixe réservé `G_`, droits
/// d'accord de modules (`src/lymaille/identity.sdd`).
pub mod identity;

/// Stores SeaORM des traits hôte de `lymaille` : huit stores liés à un `subject`, fabriqués par
/// `LymailleStores` (`src/lymaille/store/mod.sdd`).
pub mod store;

/// Module Rhai `resources` : lecture seule des `MiryadResource` par les agents, délégation
/// intégrale à `rest::core` (`src/lymaille/resource_module.sdd`).
pub mod resource_module;

/// Ressources de définition administrées par l'admin au lot 1 : agents et toolsets du catalogue,
/// profils de modèle, providers (`src/lymaille/definitions/mod.sdd`).
pub mod definitions;

/// Migrations SeaORM internes du module (`src/lymaille/migration.sdd`). `mod.sdd` veut ce module
/// privé et le ré-export `pub use migration::migrations` seulement en fin de lot : `pub(crate)`
/// en attendant, pour que `crate::migration::Migrator` assemble déjà les migrations sous
/// `lymaille` (`src/migration/mod.sdd`, amendement 2026-11-02). À passer à `pub use` avec les
/// autres ré-exports à plat.
mod migration;

/// Installation idempotente du provider TLS `rustls` (`src/lymaille/tls.sdd`).
/// `mod.sdd` veut ce module privé : temporairement `pub` en attendant le ré-export à plat et
/// l'appel par le montage de l'état (fin de lot), sans quoi `dead_code` frapperait
/// `install_crypto_provider`. À replonger privé avec les ré-exports.
pub mod tls;

pub(crate) use migration::migrations;
