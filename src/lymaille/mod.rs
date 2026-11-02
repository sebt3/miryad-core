//! Hôte miryad-core de la bibliothèque `lymaille` — squelette du lot 1 (`src/lymaille/mod.sdd`,
//! Solution `dsd/miryad-core/lymaille.md`). La feature `lymaille` implique `db`. Les enfants du
//! module (stores SeaORM, identité, secrets, module des ressources) sont déclarés au fil du lot.

/// Erreurs de l'hôte `lymaille` : un code `MRD-LYMAILLE-NNN` par variante et la décision de
/// retryabilité consommée par le step `subagent` (`src/lymaille/error.sdd`).
pub mod error;
