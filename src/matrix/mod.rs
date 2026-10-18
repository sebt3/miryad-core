//! Client Matrix de bot (feature `matrix`) — squelette provisoire, finalisé en dernier du lot
//! (`src/matrix/mod.sdd`).

pub mod config;
pub mod error;
pub mod store;

// allow: `session` est consommé par client.rs, non encore écrit (src/matrix/session.sdd) ; à retirer à son arrivée.
#[allow(dead_code)]
mod session;

// allow: `sync` est consommé par client.rs, non encore écrit (src/matrix/sync.sdd) ; à retirer à son arrivée.
#[allow(dead_code)]
mod sync;

// allow: `client` est ré-exporté par `mod.rs` en fin de lot (src/matrix/mod.sdd) ; à retirer à son arrivée.
#[allow(dead_code)]
mod client;
