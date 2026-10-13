//! Client HTTP mince vers l'API System One (`POST /v1/systemone`, modèles de décision au format
//! TypeSafe), derrière la feature `systemone`. Le module ne monte aucune route HTTP et n'ouvre
//! aucun état : le client ne garde que son pool `reqwest`. Voir `docs/architecture.md` pour le
//! contexte et les décisions.

pub mod error;

pub use error::SystemOneError;
