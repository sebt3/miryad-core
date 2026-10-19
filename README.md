# miryad-core

[![docs.rs](https://img.shields.io/docsrs/miryad-core)](https://docs.rs/miryad-core)
[![Crates.io](https://img.shields.io/crates/v/miryad-core)](https://crates.io/crates/miryad-core)

Moteur générique derrière **miryad**, template d'application Rust/Vue « à la Debian » :
vous décrivez votre modèle de données via le trait `MiryadResource`, miryad-core fournit
tout le reste — auth OIDC, RBAC/ownership, REST/MCP, OpenAPI, frontend admin —
**sans code par entité à écrire ni à maintenir**. Entités, RBAC, REST et MCP relèvent de la
feature `db` (activée par défaut) ; un profil sans base est décrit plus bas.

> Une seule implémentation de trait par entité, lue telle quelle par REST et MCP.

## Installation

```toml
[dependencies]
miryad-core = { version = "0.1", features = ["mcp"] }
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

* REST (`db`): `GET/POST /api/v1/recipes`, `GET/PUT/DELETE /api/v1/recipes/{id}` — paginé, filtré, RBAC auto.
* MCP (`mcp`, implique `db`): `POST /mcp` — 5 tools CRUD par entité.
* OpenAPI : `rest::openapi`. Le document des entités (`resource_openapi`) relève de `db` ; `openapi_router`
  sert n'importe quel document fourni par l'application. Swagger UI derrière `swagger-ui`.
* IR frontend pour le générateur TypeScript (`db`) : `ir::resource_ir`.

## Feature flags

| Feature | Effet | Dépendances |
|---------|-------|-------------|
| `static-frontend` *(default)* | Service SPA `frontend::static_frontend_router` | `tower-http` |
| `swagger-ui` | Swagger UI sur `/api/swagger-ui` | `utoipa-swagger-ui` |
| `db` *(default)* | Entités `MiryadResource`, comptes locaux, RBAC, migrations, tokens API — active `sea-orm` | `sea-orm`, `sea-orm-migration`, `hmac`, `sha2`, `rand`, `chrono`, `hex` |
| `mcp` | Serveur MCP JSON-RPC (implique `db`) | `vynil-core` |
| `workflow` | Moteur de workflow (DAG, Restate, steps Rhai, templating Handlebars central) ; implique `db` | `restate-sdk`, `vynil-core`, `tokio`, `uuid` |
| `systemone` | Client System One (`POST /v1/systemone`) ; kind de workflow `systemone` avec `workflow` | aucune |
| `matrix` | Client Matrix de bot (E2EE, store SeaORM), hors défaut ; implique `db` | `matrix-sdk`, `matrix-sdk-crypto`, `matrix-sdk-store-encryption`, `zeroize`, `tokio` |

```bash
cargo doc --open --features mcp,swagger-ui
```

## Sans base de données

Sans la feature `db` (`default-features = false`), la crate ne tire ni `sea-orm` ni `sea-orm-migration`.
Ce profil sert une application qui écrit ses propres routes et n'a pas besoin de tables :

```toml
[dependencies]
miryad-core = { version = "0.1", default-features = false, features = ["swagger-ui"] }
```

Ajouter `static-frontend` à `features` si l'application sert son frontend compilé
(`default-features = false` le désactive).

Ce qui reste disponible :

* authentification OIDC navigateur (`auth::auth_router` : `/auth/login`, `/auth/callback`, `/auth/logout`)
  et cookie de session ;
* extracteurs `AuthUser` et `AuthPrincipal`, avec la validation d'un JWT OIDC en `Authorization: Bearer`
  (signature contrôlée contre le JWKS de l'émetteur, sans base) ;
* `rest::openapi::openapi_router` (et `swagger_ui_router` avec `swagger-ui`) pour un document OpenAPI écrit à la main ;
* le routeur frontend `frontend::static_frontend_router` (`static-frontend`) ;
* `query::Pagination`.

Ce qui disparaît :

* les entités `MiryadResource`, les routes REST générées (`resource_router`) et les outils MCP ;
* RBAC, comptes locaux, groupes et tokens API ;
* les migrations internes, le workflow et le client Matrix.

## Documentation

* Crate : <https://docs.rs/miryad-core>
* Architecture : [`docs/architecture.md`](docs/architecture.md)
* Roadmap : [`docs/roadmap.md`](docs/roadmap.md)

## Licence

BSD 3-Clause. Les fichiers du store de chiffrement Matrix (`src/matrix/store/`, feature `matrix`) portent des portions dérivées de `matrix-sdk-sqlite` sous Apache-2.0 : voir `NOTICE` et `LICENSE-APACHE`.
