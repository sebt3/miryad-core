//! API GraphQL dynamique via Seaography — RBAC via `LifecycleHooksInterface`.
//!
//! Nécessite la feature `graphql`. Voir [`graphql_router`](crate::graphql::graphql_router) et [`PolicyRegistry`](crate::graphql::PolicyRegistry).

mod handler;
pub mod hooks;
pub mod principal;
pub mod registry;

pub use handler::graphql_router;
pub use hooks::MiryadHooks;
pub use principal::{GraphQlPrincipal, load_principal};
pub use registry::{EntityPolicy, PolicyRegistry};
