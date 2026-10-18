//! Client Matrix de bot (feature `matrix`) — squelette provisoire, finalisé en dernier du lot
//! (`src/matrix/mod.sdd`) : il manque `device_code` et son ré-export `DeviceCodePrompt`.

pub mod client;
pub mod config;
pub mod error;
pub mod store;
pub mod sync;

// allow: `session::clear` n'a pas encore d'appelant de production (src/matrix/session.sdd) ; à retirer à son arrivée.
#[allow(dead_code)]
mod session;

pub use client::{MatrixClient, MessageKind, SessionImport};
pub use config::MatrixConfig;
pub use error::MatrixError;
pub use sync::{MatrixEventHandler, MatrixMessage};
