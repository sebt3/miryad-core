#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(html_root_url = "https://docs.rs/miryad-core")]
//! # miryad-core
//!
//! Moteur générique derrière le template d'application **miryad**.
//! Vous décrivez votre modèle de données via le trait [`MiryadResource`](resource::MiryadResource),
//! `miryad-core` fournit tout le reste : auth OIDC, RBAC/ownership, API REST,
//! GraphQL, MCP, OpenAPI, et scaffolding frontend.
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
//! * [GraphQL](graphql) (feature `graphql`): `POST /api/graphql` + GraphiQL.
//! * [MCP](mcp) (feature `mcp`): `POST /mcp` — 5 tools par entité.
//! * OpenAPI toujours disponible via [`rest::openapi`], Swagger UI derrière `swagger-ui`.
//! * IR frontend pour le générateur TypeScript : [`ir::resource_ir`] / [`ir::IrRegistry`].
//!
//! ## Feature flags
//!
//! | Feature | Effet | Dépendances lourdes |
//! |---------|-------|---------------------|
//! | `static-frontend` *(default)* | [`frontend::static_frontend_router`] — service SPA | `tower-http` |
//! | `swagger-ui` | Swagger UI sur `/api/swagger-ui` | `utoipa-swagger-ui` |
//! | `graphql` | GraphQL dynamique (Seaography) | `seaography`, `async-graphql` |
//! | `graphiql` | IDE GraphiQL sur `/api/graphiql` (implique `graphql`) | `async-graphql/graphiql` |
//! | `mcp` | Serveur MCP JSON-RPC sur `/mcp` | `vynil-core` (Handlebars) |
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
#![cfg_attr(docsrs, warn(missing_docs))]

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
