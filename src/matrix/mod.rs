//! Client Matrix de **bot** (feature `matrix`, désactivée par défaut) : se connecter, envoyer, lire
//! (historique et flux) et rejoindre une salle, sans logique de conversation — la couche chatbot
//! consomme [`MatrixEventHandler`](crate::matrix::MatrixEventHandler).
//!
//! Construit sur `matrix-sdk`, **épinglé en `0.18`** : `matrix-sdk` ≥ 0.19 impose `libsqlite3-sys` 0.38
//! et casse le graphe avec le `sqlx-sqlite` de SeaORM (`links = "sqlite3"`). Aucune feature `sqlite` de
//! `matrix-sdk` n'est activée : l'état de chiffrement vit dans la base de l'application ([`store`](crate::matrix::store)).
//! L'API publique n'expose aucun type de `matrix-sdk` ni de `ruma`.
//!
//! # Déploiement requis
//!
//! Une base de données déjà présente, et les migrations de [`store::migrations`](crate::matrix::store::migrations) appliquées par
//! [`crate::migration::Migrator`]. Le homeserver visé est Palpo derrière MAS (authentification
//! déléguée) : connexion interactive par [`MatrixClient::login_with_device_code`](crate::matrix::MatrixClient::login_with_device_code), ou session
//! importée par [`MatrixClient::import_session`](crate::matrix::MatrixClient::import_session) (repli automatisable).
//!
//! # Limite
//!
//! Le sync démarre par un sync initial sans jeton : **les messages reçus pendant une coupure ne sont
//! pas rattrapés** (une trace `info` l'annonce au démarrage). Un message peut être remis deux fois
//! après une bascule de bail : le handler doit être idempotent. Détails et décisions :
//! `docs/architecture.md`.

pub mod client;
pub mod config;
mod device_code;
pub mod error;
// allow: `session::clear` n'a pas encore d'appelant de production (src/matrix/session.sdd) ; à retirer à son arrivée.
#[allow(dead_code)]
mod session;
pub mod store;
pub mod sync;

pub use client::{MatrixClient, MessageKind, SessionImport};
pub use config::MatrixConfig;
pub use device_code::DeviceCodePrompt;
pub use error::MatrixError;
pub use sync::{MatrixEventHandler, MatrixMessage};

#[cfg(test)]
mod tests {
    /// Scenario « les chemins plats existent » : les huit types ré-exportés à plat résolvent sans
    /// nommer les sous-modules. Compilation seule.
    #[test]
    fn chemins_plats_resolvent_sans_structure_interne() {
        fn witness_client(
            _: Option<crate::matrix::MatrixConfig>,
            _: Option<crate::matrix::MatrixClient>,
            _: Option<crate::matrix::MessageKind>,
            _: Option<crate::matrix::SessionImport>,
        ) {
        }
        fn witness_events(
            _: Option<crate::matrix::DeviceCodePrompt>,
            _: Option<crate::matrix::MatrixError>,
            _: Option<Box<dyn crate::matrix::MatrixEventHandler>>,
            _: Option<crate::matrix::MatrixMessage>,
        ) {
        }
        witness_client(None, None, None, None);
        witness_events(None, None, None, None);
    }
}
