//! Hôte miryad-core de la bibliothèque `lymaille` (`src/lymaille/mod.sdd`, Solution
//! `dsd/miryad-core/lymaille.md`). La feature `lymaille` implique `db` : tout build qui compile
//! ce module compile aussi `sea-orm`, `resource`, `rbac`, `users` et `migration`. Le module
//! fournit à `lymaille` ce qu'elle laisse à l'hôte — stockage SeaORM de ses objets, identité et
//! droits de l'appelant, installation du provider TLS, résolution des références de secret — et
//! expose les `MiryadResource` de l'application aux agents par le module Rhai `resources`.
//!
//! Déploiement requis : base présente, migrations `lymaille::migrations()` appliquées via
//! `miryad_core::migration::Migrator` (elles y sont assemblées sous la feature), provider TLS
//! installé par le montage de l'état — appel de `install_crypto_provider` (idempotent) avant tout
//! appel d'inférence ou MCP.
//!
//! Surfaces du lot 2 : sessions interactives par REST+SSE (`http::conversations_router`) et par
//! Matrix (`chat`, features `lymaille` **et** `matrix`), toutes deux adossées au même
//! `SessionManager` — un seul gestionnaire partagé, jamais un par surface. La rétention est
//! déléguée à l'application : `purge_sessions` est planifié par elle (aucune tâche interne). Le
//! montage des routeurs (`conversations_router`, et le pont Matrix) est fait par l'application,
//! jamais par ce module.
//!
//! Hôte applicatif (amendement DEC-27/DEC-30) : l'application fournit un `SharedHost` (optionnel)
//! à `LymailleStores::with_host`, à `SubagentDeps::host` et au gestionnaire de sessions
//! (`ManagerDeps`) pour ses serveurs MCP et ses contextes `H_…`. L'export TypeScript des
//! événements de session se fait **côté application** par
//! `interactive::event::export_typescript(dir)` : aucun fichier `.ts` n'est livré avec la crate.
//! Le pourquoi complet (FR, DEC, Open Issues) est dans `dsd/miryad-core/lymaille.md`.

/// Ressources de définition administrées par l'admin au lot 1 : agents et toolsets du catalogue,
/// profils de modèle, providers (`src/lymaille/definitions/mod.sdd`).
pub mod definitions;

/// Erreurs de l'hôte `lymaille` : un code `MRD-LYMAILLE-NNN` par variante et la décision de
/// retryabilité consommée par le step `subagent` (`src/lymaille/error.sdd`).
pub mod error;

/// Point d'extension de l'application hôte : serveurs MCP et contextes `H_…` fournis par elle
/// (`src/lymaille/host.sdd`). Ré-exports à plat : `LymailleHost`, `McpRequest`, `McpDecision`,
/// `McpGrant`, `HostError`, `SharedHost` (amendement DEC-27/DEC-30 de ./mod.sdd).
pub mod host;

/// TEMPORAIRE (tâche ./mod.sdd distincte) : la déclaration et les ré-exports de `host_module`
/// seront posés par la tâche de montage ; déclarés ici pour faire tourner les tests inline de
/// `src/lymaille/host_module.sdd`.
pub mod host_module;

/// Surface REST+SSE des sessions interactives (`src/lymaille/http/mod.sdd`) : routeur
/// auto-monté `conversations_router`, hors parité des ressources (acté 2026-11-03).
pub mod http;

/// Surface Matrix des sessions interactives (`src/lymaille/chat/mod.sdd`) : annuaire
/// mxid → principal, mise en forme des sorties et pont `MatrixEventHandler` — glue des
/// features `lymaille` et `matrix` (DSD `FR-13`, `DEC-26`).
#[cfg(all(feature = "lymaille", feature = "matrix"))]
pub mod chat;

/// Socle des sessions interactives (`src/lymaille/interactive/mod.sdd`) : cycle de vie des
/// sessions (`SessionManager`, `ManagerDeps`), diffusion des événements (`EventHub`) et leur
/// miroir exportable (`SessionEvent`, `SessionEventKind`, `SessionOutcome`). Ré-exports à plat de
/// l'amendement DEC-27/DEC-30 de ./mod.sdd ; `interactive::event::export_typescript` est promis
/// par son chemin, l'export TypeScript se fait côté application.
pub mod interactive;

/// Identité de l'appelant : principal reconstruit par appel, préfixe réservé `G_`, droits
/// d'accord de modules (`src/lymaille/identity.sdd`).
pub mod identity;

/// Droit d'usage des serveurs MCP et en-têtes d'authentification (`src/lymaille/mcp_access.sdd`).
/// Ré-exports à plat : `server_for`, `SeaMcpAuth` (amendement lot 2) et `resolve_role`,
/// `RoleBinding`, `SeaHostMcpAuth` (amendement DEC-27/DEC-30) de ./mod.sdd.
pub mod mcp_access;

/// Montage d'un client MCP par rôle, commun aux sessions interactives et au step batch
/// (`src/lymaille/mcp_mount.sdd`), `pub(crate)` : aucun ré-export à plat ; le step batch
/// (`crate::workflow::subagent_step`) y accède par ce chemin.
pub(crate) mod mcp_mount;

/// Migrations SeaORM internes du module (`src/lymaille/migration.sdd`), privées : l'API est le
/// ré-export `migrations` ci-dessous, assemblé par `crate::migration::Migrator` sous `lymaille`.
mod migration;

/// Module Rhai `resources` : lecture seule des `MiryadResource` par les agents, délégation
/// intégrale à `rest::core` (`src/lymaille/resource_module.sdd`).
pub mod resource_module;

/// Résolution des références d'authentification des providers (`src/lymaille/secret.sdd`).
pub mod secret;

/// Stores SeaORM des traits hôte de `lymaille` : huit stores liés à un `subject`, fabriqués par
/// `LymailleStores` (`src/lymaille/store/mod.sdd`).
pub mod store;

/// Installation idempotente du provider TLS `rustls` (`src/lymaille/tls.sdd`), privée : l'API est
/// le ré-export `install_crypto_provider` ci-dessous.
mod tls;

// Ré-exports à plat (ordre alphabétique des sous-modules, comme les déclarations ci-dessus) —
// exactement la liste fermée du `Must` de ./mod.sdd et de ses deux amendements : lot 1
// (`error`, `identity`, `migration`, `secret`, `store`, `tls`), amendement lot 2 (`host`, `http`,
// `interactive`, `mcp_access`, `store`) et amendement DEC-27/DEC-30 (`host`, `interactive` par son
// chemin `event::export_typescript`, `mcp_access`). `store` et `definitions` restent aussi
// atteignables par leur chemin : leurs entités et stores se nomment par sous-module (collisions
// de noms).
pub use error::LymailleError;
pub use host::{HostError, LymailleHost, McpDecision, McpGrant, McpRequest, SharedHost};
pub use http::conversations_router;
pub use identity::{CallerPrincipal, Identity};
pub use interactive::{
    EventHub, ManagerDeps, SessionEvent, SessionEventKind, SessionManager, SessionOutcome,
};
pub use mcp_access::{RoleBinding, SeaHostMcpAuth, SeaMcpAuth, resolve_role, server_for};
pub use migration::migrations;
pub use secret::{EnvSecretResolver, SecretResolver};
pub use store::{AuditWriter, LymailleStores, PurgeReport, purge_sessions};
pub use tls::install_crypto_provider;

#[cfg(test)]
mod tests {
    /// Scenario « les chemins plats existent » : la liste fermée du `Exposes` de `mod.sdd` et de
    /// ses deux amendements — lot 1 : `LymailleError`, `SecretResolver`, `EnvSecretResolver`,
    /// `Identity`, `CallerPrincipal`, `LymailleStores`, `migrations`, `install_crypto_provider` ;
    /// amendement lot 2 : `mcp_access::server_for`, `mcp_access::SeaMcpAuth`,
    /// `interactive::{SessionManager, ManagerDeps, EventHub}`, `http::conversations_router`,
    /// `store::{purge_sessions, PurgeReport, AuditWriter}` ; amendement DEC-27/DEC-30 :
    /// `host::{LymailleHost, McpRequest, McpDecision, McpGrant, HostError, SharedHost}`,
    /// `interactive::{SessionEvent, SessionEventKind, SessionOutcome}`,
    /// `interactive::event::export_typescript` (promis par son chemin, non à plat),
    /// `mcp_access::{resolve_role, RoleBinding, SeaHostMcpAuth}` — chaque item résout depuis
    /// `crate::lymaille::` sans nommer les sous-modules (sauf le chemin `event::export_typescript`
    /// promis tel quel par l'amendement). Compilation seule — témoins de type en position de
    /// paramètre, rien n'est instancié ni appelé (forme des témoins `Option<>` de
    /// `../workflow/mod.rs`, qui évitent `no_effect_underscore_binding` et `type_complexity` sous
    /// harnais pedantic). Les fonctions sont référencées en valeur, `black_box`ées sans appel.
    #[test]
    fn chemins_plats_resolvent() {
        fn witness(
            _: Option<crate::lymaille::LymailleError>,
            _: Option<&dyn crate::lymaille::SecretResolver>,
            _: Option<crate::lymaille::EnvSecretResolver>,
            _: Option<crate::lymaille::Identity>,
            _: Option<crate::lymaille::CallerPrincipal>,
            _: Option<crate::lymaille::LymailleStores>,
        ) {
        }
        // Amendement lot 2 : surfaces REST+SSE, sessions interactives, rétention, MCP serveur.
        fn witness_lotte2(
            _: Option<crate::lymaille::SeaMcpAuth>,
            _: Option<crate::lymaille::SessionManager>,
            _: Option<crate::lymaille::ManagerDeps>,
            _: Option<crate::lymaille::EventHub>,
            _: Option<crate::lymaille::PurgeReport>,
            _: Option<crate::lymaille::AuditWriter>,
        ) {
        }
        // Amendement DEC-27/DEC-30 : point d'extension de l'application hôte.
        fn witness_hote(
            _: Option<&dyn crate::lymaille::LymailleHost>,
            _: Option<crate::lymaille::McpRequest<'_>>,
            _: Option<crate::lymaille::McpDecision>,
            _: Option<crate::lymaille::McpGrant>,
            _: Option<crate::lymaille::HostError>,
            _: Option<crate::lymaille::SharedHost>,
        ) {
        }
        // Amendement DEC-27/DEC-30 : événements de session et résolution de rôle par l'hôte.
        fn witness_evenements(
            _: Option<crate::lymaille::SessionEvent>,
            _: Option<crate::lymaille::SessionEventKind>,
            _: Option<crate::lymaille::SessionOutcome>,
            _: Option<crate::lymaille::RoleBinding>,
            _: Option<crate::lymaille::SeaHostMcpAuth>,
        ) {
        }
        witness(None, None, None, None, None, None);
        witness_lotte2(None, None, None, None, None, None);
        witness_hote(None, None, None, None, None, None);
        witness_evenements(None, None, None, None, None);
        std::hint::black_box(crate::lymaille::migrations);
        std::hint::black_box(crate::lymaille::install_crypto_provider);
        std::hint::black_box(crate::lymaille::server_for::<sea_orm::DatabaseConnection>);
        // `S = MiryadAuthState` : le `FromRef` recouvrement d'`axum` suffit — référence en valeur
        // sans montage réel (le test de montage complet est dans `http/mod.rs`).
        std::hint::black_box(crate::lymaille::conversations_router::<crate::auth::MiryadAuthState>);
        std::hint::black_box(crate::lymaille::purge_sessions);
        std::hint::black_box(crate::lymaille::resolve_role);
        std::hint::black_box(crate::lymaille::interactive::event::export_typescript);
    }

    /// Scenario « la feature implique db » : sous `--no-default-features --features lymaille`,
    /// `crate::resource::MiryadResource` (accessible seulement sous `db`, que `lymaille`
    /// implique) et `crate::lymaille::LymailleStores` coexistent dans le même graphe — la
    /// compilation de ce test est la preuve. `MiryadResource` n'est pas objet-safe (fonctions
    /// statiques, types associés issus de `EntityTrait`) : il est référencé en position de
    /// contrainte générique, non en `dyn`.
    #[test]
    fn la_feature_implique_db() {
        fn witness<R: crate::resource::MiryadResource>(_: Option<crate::lymaille::LymailleStores>) {}
        witness::<crate::lymaille::definitions::agent::Entity>(None);
    }
}
