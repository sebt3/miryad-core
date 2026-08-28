# miryad-core

[![docs.rs](https://img.shields.io/docsrs/miryad-core)](https://docs.rs/miryad-core)
[![Crates.io](https://img.shields.io/crates/v/miryad-core)](https://crates.io/crates/miryad-core)

Moteur générique derrière **miryad**, template d'application Rust/Vue « à la Debian » :
vous décrivez votre modèle de données via le trait `MiryadResource`, miryad-core fournit
tout le reste — auth OIDC, RBAC/ownership, REST/GraphQL/MCP, OpenAPI, frontend admin —
**sans code par entité à écrire ni à maintenir**.

> Une seule implémentation de trait par entité, lue telle quelle par REST, GraphQL et MCP.

## Installation

```toml
[dependencies]
miryad-core = { version = "0.1", features = ["graphql", "mcp"] }
sea-orm = { version = "2", features = ["macros", "runtime-tokio-rustls"] }
axum = "0.8"
```

## Démarrage rapide

```rust
use sea_orm::entity::prelude::*;
use miryad_core::{
    auth::MiryadAuthState,
    migration::Migrator,
    resource::{AccessPolicy, MiryadResource},
    rest::resource_router,
};
use sea_orm_migration::MigratorTrait;

// 1. Entité SeaORM
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "recipes")]
pub struct Model { #[sea_orm(primary_key)] pub id: i32, pub title: String, pub owner_id: i32 }
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)] pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}

// 2. Contrat unique — tout le reste est fourni
impl MiryadResource for Entity {
    fn resource_name() -> &'static str { "recipes" }
    fn read_policy() -> AccessPolicy { AccessPolicy::Public }
    fn write_policy() -> AccessPolicy { AccessPolicy::OwnerOnly }
    fn owner_column() -> Option<Column> { Some(Column::OwnerId) }
}

// 3. Montage axum (FromRef<MiryadAuthState>)
# async fn example(db: sea_orm::DatabaseConnection, auth_state: MiryadAuthState) {
Migrator::up(&db, None).await.unwrap();
let app = axum::Router::new()
    .merge(miryad_core::auth::auth_router::<MiryadAuthState>())
    .merge(resource_router::<Entity, MiryadAuthState>())
    .with_state(auth_state);
# }
```

* REST: `GET/POST /api/v1/recipes`, `GET/PUT/DELETE /api/v1/recipes/{id}` — paginé, filtré, RBAC auto.
* GraphQL (`graphql`): `POST /api/graphql` + GraphiQL (`graphiql`).
* MCP (`mcp`): `POST /mcp` — 5 tools CRUD par entité.
* OpenAPI toujours disponible (`rest::openapi`), Swagger UI derrière `swagger-ui`.
* IR frontend pour le générateur TypeScript : `ir::resource_ir`.

## Feature flags

| Feature | Effet | Dépendances |
|---------|-------|-------------|
| `static-frontend` *(default)* | Service SPA `frontend::static_frontend_router` | `tower-http` |
| `swagger-ui` | Swagger UI sur `/api/swagger-ui` | `utoipa-swagger-ui` |
| `graphql` | GraphQL dynamique (Seaography) | `seaography`, `async-graphql` |
| `graphiql` | IDE GraphiQL (implique `graphql`) | `async-graphql/graphiql` |
| `mcp` | Serveur MCP JSON-RPC | `vynil-core` |

```bash
cargo doc --open --features graphql,mcp,swagger-ui
```

## Documentation

* Crate : <https://docs.rs/miryad-core>
* Architecture : [`docs/architecture.md`](docs/architecture.md)
* Roadmap : [`docs/roadmap.md`](docs/roadmap.md)

## Licence

BSD 3-Clause.
