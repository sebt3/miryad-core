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
//! Limites du lot 1 : le kind de workflow `subagent` n'exécute que des batches, le module des
//! ressources est en lecture seule, aucune session interactive. Le pourquoi complet (FR, DEC,
//! Open Issues) est dans `dsd/miryad-core/lymaille.md`.

/// Ressources de définition administrées par l'admin au lot 1 : agents et toolsets du catalogue,
/// profils de modèle, providers (`src/lymaille/definitions/mod.sdd`).
pub mod definitions;

/// Erreurs de l'hôte `lymaille` : un code `MRD-LYMAILLE-NNN` par variante et la décision de
/// retryabilité consommée par le step `subagent` (`src/lymaille/error.sdd`).
pub mod error;

/// Point d'extension de l'application hôte : serveurs MCP et contextes `H_…` fournis par elle
/// (`src/lymaille/host.sdd`). Déclaration seule — la re-déclaration et les ré-exports du
/// `mod.sdd` sont la tâche R7 du lot 2.
pub mod host;

/// Identité de l'appelant : principal reconstruit par appel, préfixe réservé `G_`, droits
/// d'accord de modules (`src/lymaille/identity.sdd`).
pub mod identity;

/// Droit d'usage des serveurs MCP et en-têtes d'authentification (`src/lymaille/mcp_access.sdd`).
/// Déclaration seule — la re-déclaration et les ré-exports du `mod.sdd` sont la tâche R7 du
/// lot 2.
pub mod mcp_access;

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
// exactement la liste du `Must` de ./mod.sdd. `store` et `definitions` restent aussi atteignables
// par leur chemin : leurs entités et stores se nomment par sous-module (collisions de noms).
pub use error::LymailleError;
pub use identity::{CallerPrincipal, Identity};
pub use migration::migrations;
pub use secret::{EnvSecretResolver, SecretResolver};
pub use store::LymailleStores;
pub use tls::install_crypto_provider;

#[cfg(test)]
mod tests {
    /// Scenario « les chemins plats existent » : les huit chemins plats du `Exposes` de `mod.sdd`
    /// résolvent depuis `crate::lymaille::` sans nommer les sous-modules. Compilation seule —
    /// témoins de type en position de paramètre, rien n'est instancié ni appelé (forme des
    /// témoins `Option<>` de `../workflow/mod.rs`, qui évitent `no_effect_underscore_binding` et
    /// `type_complexity` sous harnais pedantic). Les deux fonctions sont référencées en valeur,
    /// `black_box`ée sans appel.
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
        witness(None, None, None, None, None, None);
        std::hint::black_box(crate::lymaille::migrations);
        std::hint::black_box(crate::lymaille::install_crypto_provider);
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
