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

/// Installation idempotente du provider TLS `rustls` (`src/lymaille/tls.sdd`).
/// `mod.sdd` veut ce module privé : temporairement `pub` en attendant le ré-export à plat et
/// l'appel par le montage de l'état (fin de lot), sans quoi `dead_code` frapperait
/// `install_crypto_provider`. À replonger privé avec les ré-exports.
pub mod tls;
