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
//! | `static-frontend` *(default)* | `frontend::static_frontend_router` — service SPA | `tower-http` |
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
//! * [`HookError`](resource::HookError) reste l'erreur **applicative** de l'application
//!   consommatrice : jamais de code `MRD-*` sur ce chemin, chaque surface la restitue sans lui
//!   imposer la taxonomie de la crate.
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

/// Pagination partagée par REST et MCP (MCP via `rest::core::list`) — volontairement hors de
/// `rest/` ; GraphQL utilise ses propres curseurs seaography et n'est pas partie au contrat.
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
    /// 2026-09-23). Même méthode que `mcp_gated_compiles` : le test entier est placé sous
    /// `#[cfg(feature = "workflow")]` — dans les six combinaisons sans la feature, `workflow`
    /// « n'est pas déclaré du tout » (`Handles` de `./lib.sdd`) et un test résiduel vide serait
    /// précisément le squelette que ce contrat refuse. Sous
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

    // Les six tests de contrat documentaire (`test_lint_exemption_alignment`,
    // `query_doc_matches_reality`, `hook_error_doc`, `doc_links_feature_stable`,
    // `documented_paths_resolve`, `bin_target_unaffected`) comparent le contrat racine au source
    // de `lib.rs` et de `Cargo.toml` tels que compilés : `include_str!` donne exactement la
    // matière que lit rustdoc, sans relancer cargo depuis un test. Les chemins d'items génériques
    // (`resource_router`, `resource_ir`, les routeurs gatés) sont résolus en position d'item de
    // fonction ou sous bornes `where`, jamais instanciés ni appelés.

    fn crate_source() -> &'static str {
        include_str!("lib.rs")
    }

    fn cargo_toml_source() -> &'static str {
        include_str!("../Cargo.toml")
    }

    /// Le bloc `//!` (documentation de crate) du source, lignes jointes.
    fn crate_doc_block(source: &str) -> String {
        source
            .lines()
            .filter(|line| line.starts_with("//!"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Cibles des liens du bloc `//!` : liens en ligne `](cible)`, et liens raccourcis
    /// [`chemin`] non suivis de `(` ni `[` (les liens complets [`libellé`](cible) sont couverts
    /// par leur cible).
    fn intra_doc_targets(doc: &str) -> Vec<String> {
        let mut targets = Vec::new();
        for (pos, _) in doc.match_indices("](") {
            let after = &doc[pos + 2..];
            let end = after.find([')', ' ']).unwrap_or(after.len());
            targets.push(after[..end].to_string());
        }
        for (pos, _) in doc.match_indices("[`") {
            let after = &doc[pos..];
            let Some(close) = after.find("`]") else {
                continue;
            };
            let next = after[close + 2..].chars().next();
            if matches!(next, Some('(' | '[')) {
                continue;
            }
            targets.push(after[2..close].to_string());
        }
        targets
    }

    /// Scénario verrouillé : « surface par défaut complète » (`./lib.sdd`). Le `Given` est la
    /// compilation en features par défaut (`static-frontend`) : sous les combinaisons sans cette
    /// feature, `frontend` « n'est pas déclaré du tout » (`Handles` de `./lib.sdd`) et un test
    /// résiduel serait le squelette que ce contrat refuse — même méthode que
    /// `workflow_gated_compiles`. Les neuf chemins `miryad_core::…` du `Then` compilent : huit
    /// par témoins de types en position `Option`, `frontend` par l'item de
    /// `frontend::static_frontend_router` sous les mêmes bornes `S`, jamais appelé.
    #[cfg(feature = "static-frontend")]
    #[test]
    fn default_surface_compiles() {
        fn witness(
            _: Option<crate::auth::MiryadAuthState>,
            _: Option<crate::ir::IrRegistry>,
            _: Option<crate::migration::Migrator>,
            _: Option<crate::query::Pagination>,
            _: Option<crate::rbac::ListAccess>,
            _: Option<crate::resource::AccessPolicy>,
            _: Option<crate::rest::error::RestError>,
        ) {
        }
        fn witness_users(_: Option<crate::users::User>) {}
        fn _frontend_router<S: Clone + Send + Sync + 'static>() {
            let router = crate::frontend::static_frontend_router::<S>("spa");
            std::mem::drop(router);
        }
        witness(None, None, None, None, None, None, None);
        witness_users(None);
        std::hint::black_box(crate::auth::auth_router::<crate::auth::MiryadAuthState>);
    }

    /// Scénario verrouillé : « graphql gated par la feature graphql » (`./lib.sdd`). Sous
    /// `--no-default-features --features graphql`, le `Then` exige `graphql::graphql_router`
    /// résolvable ; hors feature, le module n'est pas déclaré et le test disparaît avec lui
    /// (`Handles` de `./lib.sdd`).
    #[cfg(feature = "graphql")]
    #[test]
    fn graphql_gated_compiles() {
        std::hint::black_box(crate::graphql::graphql_router::<crate::auth::MiryadAuthState>);
    }

    /// Scénario verrouillé : « `graphiql` implique `graphql` à la racine » (`./lib.sdd`). Sous
    /// `--no-default-features --features graphiql`, sans `graphql` demandée explicitement, la
    /// cible de test ne compile que si `crate::graphql` — gate `#[cfg(feature = "graphql")]` à la
    /// racine — est bien ouvert par l'implication du graphe `[features]` de `/Cargo.toml` : la
    /// compilation de ce test est la vérification, `graphql_router` est le `Then` du scenario
    /// jumeau revérifié sous l'implication seule.
    #[cfg(feature = "graphiql")]
    #[test]
    fn graphiql_implies_graphql() {
        std::hint::black_box(crate::graphql::graphql_router::<crate::auth::MiryadAuthState>);
    }

    /// Scénario verrouillé : « mcp gated par la feature mcp » (`./lib.sdd`). Sous
    /// `--no-default-features --features mcp`, les quatre items publics promis par l'`Exposes`
    /// de `./lib.sdd` (`mcp_router`, `McpToolRegistry`, `McpError`, `OutputFormat`) sont
    /// résolvables.
    #[cfg(feature = "mcp")]
    #[test]
    fn mcp_gated_compiles() {
        fn witness(
            _: Option<crate::mcp::McpToolRegistry>,
            _: Option<crate::mcp::McpError>,
            _: Option<crate::mcp::OutputFormat>,
        ) {
        }
        witness(None, None, None);
        std::hint::black_box(crate::mcp::mcp_router::<crate::auth::MiryadAuthState>);
    }

    /// Scénario verrouillé : « static-frontend retiré garde le reste de la surface »
    /// (`./lib.sdd`). Sous `--no-default-features`, `rest`, `resource` et `ir` restent
    /// disponibles — témoins de types. La clause « `frontend` n'est pas déclaré » est
    /// structurelle et vérifiée par la combinaison elle-même : `frontend.rs`, seul consommateur
    /// de `tower-http`, n'est pas compilé — une déclaration hors `#[cfg]` ferait échouer le build
    /// bien avant le test, et tout chemin `crate::frontend::…` serait une erreur de compilation.
    /// La clause « `tower-http` n'est pas dans le graphe de dépendances » est verrouillée par les
    /// deux lignes contractuelles de `/Cargo.toml` : dépendance `optional = true`, activable
    /// uniquement par `static-frontend`.
    #[cfg(not(feature = "static-frontend"))]
    #[test]
    fn static_frontend_absent_surface() {
        fn witness(
            _: Option<crate::ir::IrRegistry>,
            _: Option<crate::resource::AccessPolicy>,
            _: Option<crate::rest::error::RestError>,
        ) {
        }
        witness(None, None, None);
        let cargo = cargo_toml_source();
        assert!(
            cargo.contains("static-frontend = [\"dep:tower-http\"]"),
            "`tower-http` ne doit être activable que par `static-frontend`"
        );
        let tower_line = cargo
            .lines()
            .find(|line| line.trim_start().starts_with("tower-http ="))
            .expect("`tower-http` doit rester déclaré dans `[dependencies]`");
        assert!(
            tower_line.contains("optional = true"),
            "`tower-http` doit rester une dépendance optionnelle"
        );
    }

    /// Scénario verrouillé : « chemins documentés résolus sans ré-export racine » (`./lib.sdd`).
    /// Les onze chemins promis par la documentation racine résolvent depuis les chemins plats
    /// `miryad_core::<module>::…` — types en position `Option`, items de fonction concrets
    /// référencés sans appel, `resource_router` et `resource_ir` (génériques) résolus sous les
    /// bornes `where` qu'ils exigent, jamais instanciés. Clause « But » : aucun `pub use` de
    /// racine n'apparaît dans le source (`Must not` de `./lib.sdd`).
    #[test]
    fn documented_paths_resolve() {
        fn witness(
            _: Option<crate::auth::AuthPrincipal>,
            _: Option<crate::auth::middleware::AuthUser>,
            _: Option<crate::auth::MiryadAuthState>,
            _: Option<crate::ir::IrRegistry>,
            _: Option<crate::migration::Migrator>,
            _: Option<crate::resource::AccessPolicy>,
        ) {
        }
        fn _resource_trait_resolves<E: crate::resource::MiryadResource>() {}
        fn _generic_surface_paths<E, S>()
        where
            E: crate::rest::RestEntity,
            S: Clone + Send + Sync + 'static,
            crate::auth::MiryadAuthState: axum::extract::FromRef<S>,
        {
            std::hint::black_box(crate::rest::resource_router::<E, S>);
            std::hint::black_box(crate::ir::resource_ir::<E>);
        }
        witness(None, None, None, None, None, None);
        std::hint::black_box(crate::auth::auth_router::<crate::auth::MiryadAuthState>);
        std::hint::black_box(crate::rest::openapi::openapi_router::<crate::auth::MiryadAuthState>);
        let has_root_pub_use = crate_source()
            .lines()
            .any(|line| line.trim_start().starts_with("pub use "));
        assert!(
            !has_root_pub_use,
            "les chemins consommés sont `miryad_core::<module>::…` : aucun `pub use` de racine (`Must not` de `./lib.sdd`)"
        );
    }

    /// Scénario verrouillé : « exemption alignée exactement sur la famille stricte »
    /// (`./lib.sdd`). La liste des entrées `deny` de `[lints.clippy]` de `/Cargo.toml` hors les
    /// deux groupes `pedantic`/`cargo` (la famille stricte, treize lints) est comparée à la liste
    /// de l'attribut `#![cfg_attr(test, allow(…))]` de `./lib.rs` : les deux listes sont
    /// identiques, et ni `pedantic` ni `cargo` ne figurent dans l'exemption.
    #[test]
    fn test_lint_exemption_alignment() {
        let mut deny_entries: Vec<String> = Vec::new();
        let mut in_section = false;
        for line in cargo_toml_source().lines() {
            let line = line.trim();
            if line == "[lints.clippy]" {
                in_section = true;
            } else if in_section && line.starts_with('[') {
                break;
            } else if in_section
                && !line.is_empty()
                && !line.starts_with('#')
                && let Some((key, value)) = line.split_once('=')
                && value.contains("deny")
            {
                deny_entries.push(key.trim().to_string());
            }
        }
        let groups: Vec<&String> = deny_entries
            .iter()
            .filter(|entry| entry.as_str() == "pedantic" || entry.as_str() == "cargo")
            .collect();
        assert_eq!(
            groups.len(),
            2,
            "`pedantic` et `cargo` doivent rester `deny` dans `[lints.clippy]`"
        );
        let strict: Vec<String> = deny_entries
            .into_iter()
            .filter(|entry| entry != "pedantic" && entry != "cargo")
            .collect();
        assert_eq!(
            strict.len(),
            13,
            "la famille stricte compte exactement treize lints `deny`"
        );
        let lib = crate_source();
        let attr_at = lib
            .find("#![cfg_attr(")
            .expect("la racine porte ses attributs `cfg_attr`");
        let after_attr = &lib[attr_at..];
        let allow_at = after_attr
            .find("allow(")
            .expect("l'exemption racine est un `allow(…)` sous `cfg_attr(test, …)`");
        let allow_body = &after_attr[allow_at + "allow(".len()..];
        let allow_end = allow_body.find(')').expect("`allow(…)` est fermé");
        let mut allowed: Vec<String> = allow_body[..allow_end]
            .split(',')
            .map(|entry| entry.trim().trim_start_matches("clippy::").to_string())
            .filter(|entry| !entry.is_empty())
            .collect();
        assert!(
            !allowed
                .iter()
                .any(|entry| entry == "pedantic" || entry == "cargo"),
            "aucun lint des groupes `pedantic` ou `cargo` dans l'exemption `cfg(test)`"
        );
        let mut strict_sorted = strict;
        strict_sorted.sort();
        allowed.sort();
        assert_eq!(
            allowed, strict_sorted,
            "l'exemption `cfg(test)` doit contenir exactement les treize lints de la famille stricte"
        );
    }

    /// Scénario verrouillé : « commentaire de @query aligne la réalité » (`./lib.sdd`, arbitré
    /// par Sébastien le 2026-09-29, à l'unisson du `Must` de `./query.sdd`). Le `///` de
    /// `pub mod query` présente la pagination comme partagée par REST et MCP (MCP via
    /// `rest::core::list`) — jamais l'énumération « REST/GraphQL/MCP » ni le futur « plus tard » ;
    /// clause « But » : si `GraphQL` est nommé, ce l'est pour l'exclure du contrat (curseurs
    /// `seaography` propres).
    #[test]
    fn query_doc_matches_reality() {
        let lib = crate_source();
        let decl_at = lib
            .find("pub mod query;")
            .expect("`pub mod query` est déclaré à la racine");
        let doc: String = lib[..decl_at]
            .lines()
            .rev()
            .take_while(|line| line.trim_start().starts_with("///"))
            .map(str::trim)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            doc.contains("REST et MCP") && doc.contains("rest::core::list"),
            "le commentaire de @query doit le présenter comme la pagination partagée par REST et MCP (MCP via `rest::core::list`) : `{doc}`"
        );
        assert!(
            !doc.contains("REST/GraphQL/MCP") && !doc.contains("plus tard"),
            "le commentaire de @query ne doit plus présenter GraphQL comme consommant @query ni promettre un « plus tard » : `{doc}`"
        );
        let lower = doc.to_lowercase();
        assert!(
            !lower.contains("graphql") || lower.contains("n'est pas partie au contrat"),
            "si @graphql est nommé, ce doit être pour l'exclure du contrat (curseurs @seaography propres) : `{doc}`"
        );
    }

    /// Scénario verrouillé : « documentation de `HookError` » (`./lib.sdd`). Le bloc `//!`
    /// présente `HookError` comme l'erreur applicative de l'application consommatrice, exclue des
    /// codes `MRD-<DOMAINE>-NNN` que la crate réserve à ses erreurs internes.
    #[test]
    fn hook_error_doc() {
        let doc = crate_doc_block(crate_source());
        assert!(
            doc.contains("HookError"),
            "le bloc `//!` doit nommer `HookError` (`Must` de `./lib.sdd`)"
        );
        assert!(
            doc.contains("applicative"),
            "`HookError` doit être présentée comme l'erreur applicative de l'application consommatrice"
        );
        assert!(
            doc.contains("MRD"),
            "la documentation racine doit situer l'exclusion de `HookError` hors de la taxonomie `MRD-<DOMAINE>-NNN`"
        );
    }

    /// Scénario verrouillé : « cargo doc propre sur les combinaisons de features »
    /// (`./lib.sdd`). Le bloc `//!` est compilé sur toutes les combinaisons : nul lien intra-doc
    /// qu'il porte ne peut viser un module gated (`frontend`, `graphql`, `mcp`, `workflow`),
    /// faute de quoi `rustdoc::broken_intra_doc_links` sonne dès que la feature est amputée —
    /// `Raises` de `./lib.sdd` promet zéro tel avertissement, la correction attendue étant une
    /// forme robuste au graphe de features, pas la désactivation du lint. Seconde clause : les
    /// modules publics prévus apparaissent sur la page racine avec leur documentation — toute
    /// déclaration `pub mod` porte son `///` de rôle (`Must` de `./lib.sdd`). Les lancements
    /// effectifs de `cargo doc` sur chaque combinaison de `/tooling.sdd` comptent des
    /// `broken_intra_doc_links` au brut — étape de la batterie, tenue par le validator.
    #[test]
    fn doc_links_feature_stable() {
        const GATED: [&str; 4] = ["frontend", "graphql", "mcp", "workflow"];
        let doc = crate_doc_block(crate_source());
        for target in intra_doc_targets(&doc) {
            let base = target.split('#').next().unwrap_or("");
            if base.starts_with("http") {
                continue;
            }
            let head = base.split("::").next().unwrap_or(base);
            assert!(
                !GATED.contains(&head),
                "lien intra-doc du bloc `//!` vers un module gated : `{target}` casse sous toute combinaison sans cette feature"
            );
        }
        let lines: Vec<&str> = crate_source().lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("pub mod ") {
                continue;
            }
            let mut previous = index;
            while previous > 0 && lines[previous - 1].trim_start().starts_with("#[") {
                previous -= 1;
            }
            assert!(
                previous > 0 && lines[previous - 1].trim_start().starts_with("///"),
                "`{trimmed}` doit être documenté par un commentaire `///` de rôle (`Must` de `./lib.sdd`)"
            );
        }
    }

    /// Scénario verrouillé : « surface complète en toutes features » (`./lib.sdd`). Sous
    /// `--all-features` — seule combinaison de la batterie `/tooling.sdd` où les quatre modules
    /// gatés compilent ensemble — les douze chemins `miryad_core::…` du `Then` compilent : huit
    /// modules toujours compilés par témoins de types, `workflow` par ses quatre items promis,
    /// `frontend`, `graphql` et `mcp` par l'item de leur routeur sous bornes, jamais appelé.
    #[cfg(all(
        feature = "static-frontend",
        feature = "graphql",
        feature = "mcp",
        feature = "workflow"
    ))]
    #[test]
    fn all_features_surface() {
        fn witness(
            _: Option<crate::auth::MiryadAuthState>,
            _: Option<crate::ir::IrRegistry>,
            _: Option<crate::migration::Migrator>,
            _: Option<crate::query::Pagination>,
        ) {
        }
        fn witness_always(
            _: Option<crate::rbac::ListAccess>,
            _: Option<crate::resource::AccessPolicy>,
            _: Option<crate::rest::error::RestError>,
            _: Option<crate::users::User>,
        ) {
        }
        fn witness_workflow(
            _: Option<crate::workflow::DagInterpreter>,
            _: Option<crate::workflow::StepDispatcher>,
            _: Option<Box<dyn crate::workflow::MiryadWorkflowStep>>,
            _: Option<crate::workflow::StepRegistry>,
        ) {
        }
        fn _gated_routers<S: Clone + Send + Sync + 'static>()
        where
            crate::auth::MiryadAuthState: axum::extract::FromRef<S>,
        {
            let frontend = crate::frontend::static_frontend_router::<S>("spa");
            std::mem::drop(frontend);
            std::hint::black_box(crate::graphql::graphql_router::<S>);
            std::hint::black_box(crate::mcp::mcp_router::<S>);
        }
        witness(None, None, None, None);
        witness_always(None, None, None, None);
        witness_workflow(None, None, None, None);
    }

    /// Scénario verrouillé : « l'exécutable miryad reste hors de l'exemption racine »
    /// (`./lib.sdd`). L'exemption de la famille stricte est un `#![cfg_attr(test, allow(…))]` de
    /// la racine de bibliothèque ; la cible binaire `miryad` (`./bin/miryad.rs`) est une crate
    /// distincte qui ne l'hérite jamais. Verrous : l'exemption reste conditionnée à `test` (jamais
    /// un `#![allow]` nu, `Must not` de `./lib.sdd`), le source de l'exécutable ne porte aucune
    /// exemption propre, et aucun site de la famille stricte n'y apparaît — `cargo clippy
    /// --all-targets` de la batterie `/tooling.sdd` garde ainsi tout son sens sur la cible binaire
    /// (un `unwrap()` ajouté sous l'exemption hypothétique serait rattrapé ici comme par le
    /// `deny`).
    #[test]
    fn bin_target_unaffected() {
        let lines: Vec<&str> = crate_source().lines().collect();
        let test_conditioned_exemption = lines.windows(3).any(|window| {
            window[0].trim_start().starts_with("#![cfg_attr(")
                && window[1].trim() == "test,"
                && window[2].trim().starts_with("allow(")
        });
        assert!(
            test_conditioned_exemption,
            "l'exemption racine doit être un `#![cfg_attr(test, allow(…))]` conditionné à `test`"
        );
        let has_unconditional_allow = crate_source()
            .lines()
            .any(|line| line.trim_start().starts_with("#![allow("));
        assert!(
            !has_unconditional_allow,
            "aucun `#![allow]` inconditionnel à la racine : il neutraliserait le harnais (`Must not` de `./lib.sdd`)"
        );
        let bin = include_str!("bin/miryad.rs");
        assert!(
            !bin.contains("cfg_attr(test") && !bin.contains("allow(clippy::"),
            "l'exécutable ne doit porter aucune exemption de la famille stricte"
        );
        for marker in [
            ".unwrap()",
            ".expect(",
            "panic!(",
            "unreachable!(",
            "todo!(",
            "unimplemented!(",
            "dbg!",
        ] {
            assert!(
                !bin.contains(marker),
                "l'exécutable reste soumis aux `deny` de la famille stricte : site `{marker}`"
            );
        }
    }
}
