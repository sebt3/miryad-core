//! Agrégation de `src/lymaille/chat/` (`src/lymaille/chat/mod.sdd`).
//!
//! Surface Matrix des sessions interactives (DSD `FR-13`, `DEC-23`, `DEC-26`) : la glue des
//! features `lymaille` et `matrix` — la déclaration du module dans `../mod.rs` est sous
//! `#[cfg(all(feature = "lymaille", feature = "matrix"))]`, et le module n'existe pas sans
//! `matrix`.
//!
//! Déploiement : l'application monte [`link_code_router`] dans son routeur (le lien mxid →
//! compte se fait depuis la surface REST authentifiée), puis construit un [`ChatBridge`] branché
//! sur son @SessionManager et son @MatrixClient, et le donne à `run_sync` comme
//! @MatrixEventHandler. Le contexte des salles ouvertes à la demande vient de l'hôte :
//! `ChatConfig::rooms` porte un @RoomContextResolver (`bridge::RoomContextResolver`).
//!
//! Identité : un mxid ne vaut un droit que par la liaison créée avec un code à usage unique
//! ([`issue_code`] → `!link` dans la salle → [`consume_code`]) ; la relecture à chaque message
//! suit la suppression du compte (DSD `AC-17`). [`unlink`] délie, révoquant la surface.
//!
//! Limites du lot 2 : le nommage des salles est à la charge de l'opérateur ; une salle directe
//! accueille un seul utilisateur lié ; pas de streaming par édition — la réponse arrive en un ou
//! plusieurs messages complets (./render.sdd).

/// Annuaire mxid → principal et codes d'appairage (`src/lymaille/chat/link.sdd`).
pub mod link;

/// Pont salles ↔ sessions interactives (`src/lymaille/chat/bridge.sdd`).
pub mod bridge;

/// Découpage et mise en forme des sorties pour Matrix (`src/lymaille/chat/render.sdd`).
mod render;

/// Chemins plats promis par `./mod.sdd` : le pont et sa configuration, et la surface d'annuaire
/// que l'application et les tests montent sans chemin complet.
pub use bridge::{ChatBridge, ChatConfig};
pub use link::IssuedCode;
pub use link::consume_code;
pub use link::issue_code;
pub use link::link_code_router;
pub use link::resolve;
pub use link::unlink;

#[cfg(test)]
mod tests {
    /// Les chemins plats promis par `./mod.sdd` compilent et désignent les éléments des modules.
    #[test]
    fn chemins_plats_attendus() {
        let montage: fn(
            sea_orm::DatabaseConnection,
            crate::lymaille::interactive::manager::SessionManager,
            std::sync::Arc<crate::matrix::MatrixClient>,
            super::ChatConfig,
        ) -> super::ChatBridge = super::ChatBridge::new;
        assert_eq!(std::mem::size_of_val(&montage), std::mem::size_of::<usize>());
        let annuaire = (
            super::issue_code,
            super::consume_code,
            super::resolve,
            super::unlink,
            super::link_code_router::<crate::auth::MiryadAuthState>,
        );
        assert_eq!(
            std::mem::size_of_val(&annuaire),
            0,
            "des items de fonction sont ZST"
        );
        let issu = super::IssuedCode {
            code: "AB23".to_owned(),
            expires_at: chrono::Utc::now(),
        };
        assert_eq!(issu.code, "AB23");
        let salles: std::sync::Arc<dyn super::bridge::RoomContextResolver> =
            super::ChatConfig::default().rooms;
        assert_eq!(
            std::mem::size_of_val(&salles),
            2 * std::mem::size_of::<usize>(),
            "la defaut monte un resolveur de contexte"
        );
    }
}
