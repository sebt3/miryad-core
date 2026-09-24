//! Moteur de workflow à DAG (feature `workflow`) : des workflows persistés en base, édités par
//! l'admin via le CRUD générique de `WorkflowDefinition` comme toute autre entité, et exécutés
//! par un cluster Restate self-hosté — un par cluster Kubernetes, jamais géré par cette crate.
//!
//! `workflow` ne monte **aucune** route sur le `axum::Router` de l'app : ses services parlent le
//! protocole `restate-sdk`, servis par un `HttpServer`/`Endpoint` que l'app construit et lie
//! elle-même dans son propre `main()`. Schéma de déploiement (StatefulSet Restate, vynil box)
//! dans `docs/architecture.md`.

pub mod client;
pub mod error;
pub mod step;
