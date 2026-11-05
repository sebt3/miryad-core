//! Socle commun des sessions interactives, canal-agnostique (`src/lymaille/interactive/mod.sdd`).
//!
//! **État intermédiaire (réalignement R10, DSD `DEC-24`, `DEC-28`)** : `event` (miroir typé des
//! événements de session, `./event.sdd`), `hub` (diffusion de ces événements, `./hub.sdd`),
//! `activation` (`./activation.sdd`), `assemble` (`./assemble.sdd`) et `manager` (`./manager.sdd`)
//! existent. Les ré-exports à plat et le test de chemins plats prévus par `./mod.sdd` restent la
//! tâche de finalisation de ce `mod.sdd` — ce fichier n'en est pas la version finale.

/// Miroir typé, exportable en TypeScript, des événements de session (`src/lymaille/interactive/event.sdd`).
pub mod event;

/// Diffusion des événements de session vers leurs abonnés (`src/lymaille/interactive/hub.sdd`).
pub mod hub;

/// Activation self-service des définitions du propriétaire (`src/lymaille/interactive/activation.sdd`).
pub mod activation;

/// Assemblage du moteur d'une session interactive (`src/lymaille/interactive/assemble.sdd`).
pub mod assemble;

/// Cycle de vie des sessions interactives — socle commun REST+SSE et Matrix (`src/lymaille/interactive/manager.sdd`).
pub mod manager;
