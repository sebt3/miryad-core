//! Client HTTP mince vers l'API System One (`POST /v1/systemone`, modèles de décision au format
//! TypeSafe), derrière la feature `systemone`. Le module ne monte aucune route HTTP et n'ouvre
//! aucun état : le client ne garde que son pool `reqwest`. Voir `docs/architecture.md` pour le
//! contexte et les décisions.

/// Client HTTP `POST /v1/systemone` : configuration, requête et réponse typées, client.
pub mod client;
/// Taxonomie d'erreur `MRD-SYSTEMONE-NNN`.
pub mod error;

pub use client::{SystemOneClient, SystemOneConfig, SystemOneRequest, SystemOneResponse};
pub use error::SystemOneError;

#[cfg(test)]
mod tests {
    use crate::systemone::{
        SystemOneClient, SystemOneConfig, SystemOneError, SystemOneRequest, SystemOneResponse,
    };

    /// Scenario « les chemins plats existent » (`mod.sdd`) : les cinq types s'importent depuis
    /// `crate::systemone`, sans passer par `client::` ni `error::`.
    #[test]
    fn les_chemins_plats_existent() {
        let names = [
            std::any::type_name::<SystemOneClient>(),
            std::any::type_name::<SystemOneConfig>(),
            std::any::type_name::<SystemOneRequest>(),
            std::any::type_name::<SystemOneResponse>(),
            std::any::type_name::<SystemOneError>(),
        ];
        assert_eq!(names.len(), 5);
    }
}
