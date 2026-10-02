//! Serveur MCP — 5 tools CRUD par entité, sortie JSON/YAML/Markdown.
//!
//! Nécessite la feature `mcp`. Voir [`McpToolRegistry`](crate::mcp::McpToolRegistry) et [`mcp_router`](crate::mcp::mcp_router).
//!
//! La surface publique du module est exactement ces deux chemins courts (plus `McpError` et
//! `OutputFormat`) : les cinq sous-modules enfants — `error`, `format`, `handler`, `protocol`,
//! `registry` — sont déclarés `mod` privés et ne sont jamais joignables de l'extérieur. Un
//! chemin long vers un enfant échoue à la compilation (`E0603`, preuve `compile_fail`) :
//!
//! ```compile_fail
//! use miryad_core::mcp::protocol;
//! ```

mod error;
mod format;
mod handler;
mod protocol;
mod registry;

pub use error::McpError;
pub use format::OutputFormat;
pub use handler::mcp_router;
pub use registry::McpToolRegistry;

#[cfg(test)]
mod tests {
    /// Entité fixture du pattern `recipe` de ./registry.rs, dupliquée ici — l'originale vit
    /// dans le `mod tests` privé de ./registry.rs et n'est pas atteignable d'ici. Borne de
    /// `McpToolRegistry::register::<E>` (blanket `RestEntity` sur `MiryadResource`) : seule
    /// la déclaration du trait compte, aucune base n'est touchée par la chaîne de montage.
    /// `filter_column` de l'originale omis : il exerce un `Scenario` propre à ./registry.rs,
    /// hors du contrat d'agrégation prouvé ici.
    mod recipe {
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};

        use crate::resource::{AccessPolicy, MiryadResource};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "recipes")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
            pub owner_id: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "recipes"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn owner_column() -> Option<Column> {
                Some(Column::OwnerId)
            }
        }
    }

    /// `Scenario` : « quatre ré-exports et la chaîne de montage résolvent sous mcp » — les
    /// quatre items par leurs seuls chemins courts `crate::mcp::*` (aucun chemin d'enfant),
    /// puis la chaîne de montage d'une app `OutputFormat::Json` → `McpToolRegistry::new` →
    /// `register::<E>` (fixture `recipe`) → `mcp_router::<MiryadAuthState>` se construit à
    /// travers ./mod.rs seul, sans instance d'état (la construction du `Router` générique
    /// n'en requiert pas). Le module de test entier n'existe que sous `mcp` — lui-même
    /// preuve d'atomicité de l'unique `#[cfg(feature = "mcp")]` de ../lib.rs.
    #[cfg(feature = "mcp")]
    #[test]
    fn mcp_surface_four_reexports_resolve() {
        use crate::auth::MiryadAuthState;
        use crate::mcp::{McpError, McpToolRegistry, OutputFormat, mcp_router};

        // `McpError`, quatrième item, n'est pas un maillon de la chaîne de montage : témoin
        // de type, sa résolution en chemin court est la preuve attendue.
        let _: Option<McpError> = None;

        let mut registry = McpToolRegistry::new(OutputFormat::Json);
        registry.register::<recipe::Entity>();
        let router = mcp_router::<MiryadAuthState>(registry);
        assert!(
            router.has_routes(),
            "la chaîne montée à travers ./mod.rs seul produit un routeur portant POST /mcp"
        );
    }

    /// `Scenario` : « mcp et graphql coexistent sans interférence » — `mcp_router` en chemin
    /// court `crate::mcp::*` et `crate::graphql::graphql_router` en chemin court graphql
    /// adressés dans une même unité de compilation (`--all-features`). La coercition des deux
    /// items en pointeurs de fonction typés force la validation de leurs bornes
    /// `FromRef<MiryadAuthState>` respectives ; ./mod.rs ne change aucune ligne entre `mcp`
    /// seul et `mcp` + `graphql` (aucun cfg croisé), et la surface publique de `mcp` reste
    /// exactement les quatre items, ni amputée ni étendue par `graphql`.
    #[cfg(all(feature = "mcp", feature = "graphql"))]
    #[test]
    fn mcp_graphql_coexist() {
        use crate::auth::MiryadAuthState;
        use crate::mcp::{McpToolRegistry, OutputFormat, mcp_router};

        fn witness(
            _mcp: fn(McpToolRegistry) -> axum::Router<MiryadAuthState>,
            _graphql: fn(async_graphql::dynamic::Schema) -> axum::Router<MiryadAuthState>,
        ) {
        }
        witness(
            mcp_router::<MiryadAuthState>,
            crate::graphql::graphql_router::<MiryadAuthState>,
        );

        // La chaîne de montage mcp reste entièrement constructible sous --all-features,
        // à l'identique de `mcp` seul : même registre, même routeur, surface inchangée.
        let router = mcp_router::<MiryadAuthState>(McpToolRegistry::new(OutputFormat::Json));
        assert!(
            router.has_routes(),
            "sous mcp + graphql, mcp_router construit toujours POST /mcp sans interférence"
        );
    }
}
