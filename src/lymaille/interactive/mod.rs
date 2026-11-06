//! Socle commun des sessions interactives, canal-agnostique (`src/lymaille/interactive/mod.sdd`),
//! consommé par la surface REST+SSE (`../http/mod.sdd`) et par la surface Matrix
//! (`../chat/mod.sdd`) — DSD `FR-11`, `DEC-24`. Aucune logique propre : déclarations, ré-exports,
//! documentation.
//!
//! Modèle : un tour actif par session et par processus ; le tour est une tâche détachée du client —
//! il survit à la déconnexion ; le principal est relu à chaque appel ; le jeton `forward` n'existe
//! qu'en mémoire pour un seul tour.
//!
//! Assemblage attendu de l'application : un `SessionManager::new(ManagerDeps { … })` partagé,
//! monté sous les deux surfaces (jamais un gestionnaire par surface).
//!
//! Limites du lot 2 : pas de rejeu d'événements (le flux n'est pas persisté) ; pas de coordination
//! entre processus (le hub et le verrou de tour sont en mémoire, application mono-réplique).

/// Miroir typé, exportable en TypeScript, des événements de session (`src/lymaille/interactive/event.sdd`).
pub mod event;

/// Diffusion des événements de session vers leurs abonnés (`src/lymaille/interactive/hub.sdd`).
pub mod hub;

/// Cycle de vie des sessions interactives — socle commun REST+SSE et Matrix (`src/lymaille/interactive/manager.sdd`).
pub mod manager;

/// Activation self-service des définitions du propriétaire (`src/lymaille/interactive/activation.sdd`).
pub mod activation;

/// Assemblage du moteur d'une session interactive (`src/lymaille/interactive/assemble.sdd`).
pub mod assemble;

// Ré-exports à plat — exactement la liste fermée du `Must` de ./mod.sdd (ordre des sous-modules
// tels que déclarés ci-dessus). `event::export_typescript` est promis par ../mod.sdd (amendement
// DEC-30) par son chemin, non à plat.
pub use activation::{ActivationReport, activate_toolset, discard_draft};
pub use event::{SessionEvent, SessionEventKind, SessionOutcome};
pub use hub::EventHub;
pub use manager::{ManagerDeps, SessionHandle, SessionManager};

#[cfg(test)]
mod tests {
    /// Scenario « les chemins plats existent » : la liste fermée du `Must` de `./mod.sdd` —
    /// `event::{SessionEvent, SessionEventKind, SessionOutcome}`, `hub::EventHub`,
    /// `manager::{SessionManager, ManagerDeps, SessionHandle}`,
    /// `activation::{activate_toolset, discard_draft, ActivationReport}` — résout depuis
    /// `crate::lymaille::interactive::` sans nommer les sous-modules. Compilation seule — témoins
    /// de type en position de paramètre, rien n'est instancié ni appelé (forme des témoins
    /// `Option<>` de `../../mod.rs`, qui évitent `no_effect_underscore_binding` et
    /// `type_complexity` sous harnais pedantic). Les fonctions sont référencées en valeur,
    /// `black_box`ées sans appel. Le chemin `event::export_typescript`, promis par `../mod.sdd`
    /// (amendement DEC-30), est verrouillé ici car c'est ce module qui le rend atteignable par sa
    /// déclaration `pub mod event;`.
    #[test]
    fn chemins_plats_resolvent() {
        use crate::lymaille::interactive::{
            ActivationReport, EventHub, ManagerDeps, activate_toolset, discard_draft,
        };
        use crate::lymaille::interactive::{SessionEvent, SessionEventKind, SessionHandle};
        use crate::lymaille::interactive::{SessionManager, SessionOutcome};
        fn witness_types(
            _: Option<SessionEvent>,
            _: Option<SessionEventKind>,
            _: Option<SessionOutcome>,
            _: Option<EventHub>,
        ) {
        }
        fn witness_manager(
            _: Option<SessionManager>,
            _: Option<ManagerDeps>,
            _: Option<SessionHandle>,
            _: Option<ActivationReport>,
        ) {
        }
        witness_types(None, None, None, None);
        witness_manager(None, None, None, None);
        std::hint::black_box(activate_toolset);
        std::hint::black_box(discard_draft);
        std::hint::black_box(crate::lymaille::interactive::event::export_typescript);
    }
}
