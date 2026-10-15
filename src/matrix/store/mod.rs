//! Store de chiffrement `matrix-sdk` sur `SeaORM` — squelette provisoire, finalisé en dernier du lot
//! (`src/matrix/store/mod.sdd`).

pub mod error;
// Provisoire : rien n'appelle encore `kv` tant que les autres enfants du store ne sont pas posés ; retiré à la finalisation (src/matrix/store/mod.sdd).
#[allow(dead_code)]
mod kv;
// Provisoire : `crypto` (squelette struct + open) n'est pas encore exporté ; retiré à la finalisation (src/matrix/store/mod.sdd).
#[allow(dead_code)]
mod crypto;
// Provisoire : rien n'appelle encore `crypto_account` tant que `crypto` n'implémente pas le trait ; retiré à la finalisation (src/matrix/store/mod.sdd).
#[allow(dead_code)]
mod crypto_account;
pub mod migration;
