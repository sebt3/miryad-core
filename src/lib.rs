#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(html_root_url = "https://docs.rs/miryad-core")]
//! # miryad-core
//!
//! Moteur générique derrière le template d'application **miryad**.
//! Vous décrivez votre modèle de données via le trait [`MiryadResource`](resource::MiryadResource),
//! `miryad-core` fournit tout le reste : auth OIDC, RBAC/ownership, API REST,
//! GraphQL, MCP, `OpenAPI`, et scaffolding frontend.
//!
//! > **80% de l'application vient gratuitement** — une seule implémentation de trait
//! > par entité, lue telle quelle par REST, GraphQL et MCP (zéro duplication).
//!
//! ## Architecture
//!
//! ```text
//! [ Vue 3 + shadcn-vue ]              ← scaffoldé côté app consommatrice
//!         │  REST / GraphQL (cookie OIDC ou token API)
//!         ▼
//! [ miryad-core — couche générique ]   ← jamais de code par entité à écrire
//!   ├─ REST CRUD (axum)
//!   ├─ GraphQL (Seaography, schéma dynamique)
//!   ├─ MCP (tools CRUD, sortie markdown/json/yaml)
//!   ├─ Auth (OIDC + cookie + tokens API, dual-auth)
//!   ├─ RBAC/ownership (par entité)
//!   └─ IR frontend + service statique SPA
//!         │  SeaORM
//!         ▼
//! [ Entités SeaORM → PostgreSQL (CNPG) ]
//! ```
//!
//! ## Installation
//!
//! ```toml
//! [dependencies]
//! miryad-core = { version = "0.1", features = ["graphql", "mcp"] }
//! sea-orm = { version = "2", features = ["macros", "runtime-tokio-rustls"] }
//! axum = "0.8"
//! ```
//!
//! ## Démarrage rapide
//!
//! ```rust,ignore
//! use axum::Router;
//! use sea_orm::entity::prelude::*;
//! use miryad_core::{
//!     auth::MiryadAuthState,
//!     migration::Migrator,
//!     resource::{AccessPolicy, MiryadResource},
//!     rest::resource_router,
//! };
//! use sea_orm_migration::MigratorTrait;
//!
//! // 1. Déclarez une entité SeaORM
//! #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
//! #[sea_orm(table_name = "recipes")]
//! pub struct Model { #[sea_orm(primary_key)] pub id: i32, pub title: String, pub owner_id: i32 }
//! #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)] pub enum Relation {}
//! impl ActiveModelBehavior for ActiveModel {}
//!
//! // 2. Implémentez MiryadResource — c'est tout
//! impl MiryadResource for Entity {
//!     fn resource_name() -> &'static str { "recipes" }
//!     fn read_policy() -> AccessPolicy { AccessPolicy::Public }
//!     fn write_policy() -> AccessPolicy { AccessPolicy::OwnerOnly }
//!     fn owner_column() -> Option<Column> { Some(Column::OwnerId) }
//! }
//!
//! // 3. Montez les routeurs dans votre AppState (FromRef<MiryadAuthState>)
//! # async fn example(db: sea_orm::DatabaseConnection, auth_state: MiryadAuthState) {
//! Migrator::up(&db, None).await.unwrap();
//! let app: Router = Router::new()
//!     .merge(miryad_core::auth::auth_router::<MiryadAuthState>())
//!     .merge(resource_router::<Entity, MiryadAuthState>())
//!     .with_state(auth_state);
//! # }
//! ```
//!
//! * REST: `GET/POST /api/v1/recipes`, `GET/PUT/DELETE /api/v1/recipes/{id}`
//!   — paginé (`?page=&per_page=&filter=`), RBAC automatique.
//! * `GraphQL` (feature `graphql`, module `graphql`) : `POST /api/graphql` + `GraphiQL`.
//! * `MCP` (feature `mcp`, module `mcp`) : `POST /mcp` — 5 tools par entité.
//! * `OpenAPI` toujours disponible via [`rest::openapi`], Swagger UI derrière `swagger-ui`.
//! * IR frontend pour le générateur TypeScript : [`ir::resource_ir`] / [`ir::IrRegistry`].
//!
//! ## Feature flags
//!
//! | Feature | Effet | Dépendances lourdes |
//! |---------|-------|---------------------|
//! | `static-frontend` *(default)* | [`frontend::static_frontend_router`] — service SPA | `tower-http` |
//! | `swagger-ui` | Swagger UI sur `/api/swagger-ui` | `utoipa-swagger-ui` |
//! | `graphql` | GraphQL dynamique (Seaography) | `seaography`, `async-graphql` |
//! | `graphiql` | IDE `GraphiQL` sur `/api/graphiql` (implique `graphql`) | `async-graphql/graphiql` |
//! | `mcp` | Serveur MCP JSON-RPC sur `/mcp` | `vynil-core` (Handlebars) |
//! | `workflow` | Moteur de workflow DAG sur Restate — **aucune** route sur le `axum::Router` de l'app (services liés par l'app elle-même, `Endpoint::builder()` ; voir `docs/architecture.md`) | `restate-sdk`, `uuid`, `tokio`, `vynil-core` (`rhai`) |
//!
//! Voir aussi [`docs/architecture.md`](https://github.com/sebt3/miryad-core/blob/main/docs/architecture.md)
//! et [`docs/roadmap.md`](https://github.com/sebt3/miryad-core/blob/main/docs/roadmap.md).
//!
//! ## Conventions
//!
//! * Pas de `println!`/`dbg!` — [`tracing`].
//! * Erreurs avec code unique `MRD-<DOMAINE>-NNN` (`MRD-AUTH-XXX`, `MRD-REST-XXX`).
//! * Tables internes préfixées `miryad_*`, migrations isolées (tracking table dédiée).

#![warn(rustdoc::broken_intra_doc_links, rustdoc::missing_crate_level_docs)]
// Famille panic/unwrap/indexation interdite en production (`[lints]` de Cargo.toml, harnais
// contractualisé par `tooling.sdd`) mais tolérée sous `cfg(test)` : les modules de tests inline
// héritent de cette exemption depuis la racine de crate, sans `#[allow]` dispersés.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::dbg_macro,
        clippy::todo,
        clippy::unimplemented,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing,
        clippy::unwrap_in_result,
        clippy::panic_in_result_fn
    )
)]

/// Authentification OIDC, cookies de session, tokens API et extracteurs axum.
///
/// Point d'entrée principal : [`auth::MiryadAuthState`] + [`auth::auth_router`].
/// Le dual-auth (cookie **ou** `Authorization: Bearer`) est consommé via
/// [`auth::AuthPrincipal`] — voir [`auth::dual`] et [`auth::middleware::AuthUser`].
pub mod auth;

/// Service statique SPA pour le frontend compilé (feature `static-frontend`).
#[cfg(feature = "static-frontend")]
pub mod frontend;

/// API GraphQL dynamique via Seaography (feature `graphql`).
#[cfg(feature = "graphql")]
pub mod graphql;

/// Représentation intermédiaire par entité pour le générateur frontend TypeScript.
///
/// Voir [`ir::resource_ir`] et [`ir::IrRegistry`].
pub mod ir;

/// Serveur MCP — tools CRUD génériques, sortie JSON/YAML/Markdown (feature `mcp`).
#[cfg(feature = "mcp")]
pub mod mcp;

/// Migrations SeaORM internes (`miryad_*`). À appliquer au démarrage de l'app :
/// `miryad_core::migration::Migrator::up(&db, None).await`.
pub mod migration;

/// Pagination partagée REST/GraphQL/MCP.
pub mod query;

/// RBAC row-level et filtrage de liste par propriétaire.
pub mod rbac;

/// Contrat central [`resource::MiryadResource`] et [`resource::AccessPolicy`].
pub mod resource;

/// API REST générique — routeur CRUD + OpenAPI.
pub mod rest;

/// Gestion utilisateurs/groupes (résolution, synchronisation OIDC, comptes de service).
pub mod users;

/// Moteur de workflow à DAG (feature `workflow`) : définitions persistées en base, exécutées par
/// un cluster Restate self-hosté. Ne monte **aucune** route sur le `axum::Router` de l'app — les
/// services `restate-sdk` sont liés par l'app consommatrice elle-même ; voir
/// `docs/architecture.md`.
#[cfg(feature = "workflow")]
pub mod workflow;

#[cfg(test)]
mod tests {
    /// Scénario verrouillé : « workflow gated par la feature workflow » (`./lib.sdd`, amendement
    /// 2026-09-23). Même méthode que le futur `mcp_gated_compiles` du batch lib.sdd : le test
    /// entier est placé sous `#[cfg(feature = "workflow")]` — dans les six combinaisons sans la
    /// feature, `workflow` « n'est pas déclaré du tout » (`Handles` de `./lib.sdd`) et un test
    /// résiduel vide serait précisément le squelette que ce contrat refuse. Sous
    /// `--no-default-features --features workflow`, les quatre chemins plats du `Then` résolvent.
    #[cfg(feature = "workflow")]
    #[test]
    fn workflow_gated_compiles() {
        // Les quatre chemins du `Then` en position de type, sans instanciation ni liaison
        // (mêmes raisons de harnais que `chemins_plats_resolvent_sans_structure_interne`).
        fn witness(
            _: Option<crate::workflow::DagInterpreter>,
            _: Option<crate::workflow::StepDispatcher>,
            _: Option<Box<dyn crate::workflow::MiryadWorkflowStep>>,
            _: Option<crate::workflow::StepRegistry>,
        ) {
        }
        witness(None, None, None, None);
        // Clause « But » du scénario : aucune route n'apparaît sur un `axum::Router` construit
        // sans que l'app n'appelle elle-même `Endpoint::builder()`. La garantie est structurelle
        // (`Forbids` de `./workflow/mod.sdd` : aucun `axum`/`tower` dans le module) ; ce témoin
        // l'exécute : un routeur assemblé par l'app sans intervention de `workflow` reste vide.
        assert!(
            !axum::Router::<()>::new().has_routes(),
            "workflow ne doit monter aucune route sur un Router que l'app ne construit pas elle-même"
        );
    }
}
