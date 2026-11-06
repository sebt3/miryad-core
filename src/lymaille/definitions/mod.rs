//! Ressources de définition de l'hôte `lymaille` (feature `lymaille`) — contrat porté par
//! `./mod.sdd`, amendé lot 2 (DSD `DEC-19`, `DEC-20`).
//!
//! Dix entités `MiryadResource`, deux familles, administrées par le CRUD REST et MCP générique de
//! la crate — le montage applicatif (`rest::resource_router`) n'est pas de ce module, et aucune
//! ressource n'est enregistrée par défaut :
//!
//! Famille catalogue (tables `catalog_*`, écriture `AdminOnly`, `owner_column() = None`) :
//!
//! - [`agent::Entity`] — `lymaille_catalog_agents` : définitions d'agent, lecture `Public`, filtre
//!   et libellé sur les colonnes dérivées ;
//! - [`toolset::Entity`] — `lymaille_catalog_toolsets` : toolsets, version active et brouillon sur
//!   la même ligne (DSD `DEC-18`), mêmes politiques ;
//! - [`profile::Entity`] — `lymaille_profiles` : profils de modèle, mêmes politiques ;
//! - [`provider::Entity`] — `lymaille_providers` : moteurs d'inférence, `AdminOnly` en lecture
//!   comme en écriture (DSD `DEC-16`) ;
//! - [`context_type::Entity`] — `lymaille_catalog_context_types` : types de contexte, lecture
//!   `Public`, écriture `AdminOnly` ;
//! - [`catalog_context::Entity`] — `lymaille_catalog_contexts` : contextes de toolset du
//!   catalogue, mêmes politiques ;
//! - [`mcp_server::Entity`] — `lymaille_mcp_servers` : serveurs MCP déclarés par l'admin,
//!   références seules, `AdminOnly` en lecture comme en écriture.
//!
//! Famille privée (tables distinctes, `owner_id` non nul, `OwnerOnly` en lecture comme en
//! écriture) :
//!
//! - [`private_agent::Entity`] — `lymaille_agents` : définitions d'agent privées de leur
//!   propriétaire, écrites par les stores ;
//! - [`private_toolset::Entity`] — `lymaille_toolsets` : toolsets privés, version active et
//!   brouillon sur la même ligne ;
//! - [`user_context::Entity`] — `lymaille_contexts` : contextes de toolset personnels.
//!
//! Agents et toolsets du catalogue portent un identifiant préfixé `G_` (DSD `DEC-9`), refusé sur
//! les lignes privées (DSD `DEC-19`). Les tables internes (sessions, mémoires) n'ont pas d'entité
//! ici (DSD `DEC-14`).
//!
//! Aucun ré-export à plat : les noms `Entity`, `Model` et `ActiveModel` collisionnent ; chaque
//! entité s'atteint par son chemin, par exemple
//! `rest::resource_router::<definitions::agent::Entity>()` (même motif que
//! `workflow::definition::Entity`). Les secrets n'y figurent jamais : un `provider` ni un
//! `mcp_server` ne portent que des références, validées comme telles à l'écriture.

/// Définitions d'agent du catalogue (`src/lymaille/definitions/agent.sdd`).
pub mod agent;

/// Toolsets du catalogue, version active et brouillon (`src/lymaille/definitions/toolset.sdd`).
pub mod toolset;

/// Définitions d'agent privées de leur propriétaire, `OwnerOnly`
/// (`src/lymaille/definitions/private_agent.sdd`).
pub mod private_agent;

/// Toolsets privés de leur propriétaire, version active et brouillon sur la même ligne,
/// `OwnerOnly` (`src/lymaille/definitions/private_toolset.sdd`).
pub mod private_toolset;

/// Profils de modèle (`src/lymaille/definitions/profile.sdd`).
pub mod profile;

/// Moteurs d'inférence, références seules (`src/lymaille/definitions/provider.sdd`).
pub mod provider;

/// Types de contexte du catalogue (`src/lymaille/definitions/context_type.sdd`).
pub mod context_type;

/// Contextes de toolset du catalogue (`src/lymaille/definitions/catalog_context.sdd`).
pub mod catalog_context;

/// Contextes de toolset personnels, `OwnerOnly` (`src/lymaille/definitions/user_context.sdd`).
pub mod user_context;

/// Serveurs MCP déclarés par l'admin, références seules, `AdminOnly` en lecture comme en écriture
/// (`src/lymaille/definitions/mcp_server.sdd`).
pub mod mcp_server;

#[cfg(test)]
mod tests {
    use super::agent;
    use super::catalog_context;
    use super::context_type;
    use super::mcp_server;
    use super::private_agent;
    use super::private_toolset;
    use super::profile;
    use super::provider;
    use super::toolset;
    use super::user_context;
    use crate::resource::MiryadResource as _;

    /// Scenario « les quatre entités sont atteignables », étendu par l'amendement lot 2 : les dix
    /// chemins (`definitions::<module>::Entity`) résolvent et leurs `resource_name()` sont
    /// distincts deux à deux (le montage du moteur refuserait un doublon).
    #[test]
    fn les_dix_ressources_sont_atteignables_et_leurs_noms_distincts() {
        let noms = [
            agent::Entity::resource_name(),
            toolset::Entity::resource_name(),
            profile::Entity::resource_name(),
            provider::Entity::resource_name(),
            private_agent::Entity::resource_name(),
            private_toolset::Entity::resource_name(),
            context_type::Entity::resource_name(),
            catalog_context::Entity::resource_name(),
            user_context::Entity::resource_name(),
            mcp_server::Entity::resource_name(),
        ];
        assert_eq!(
            noms,
            [
                "lymaille_catalog_agents",
                "lymaille_catalog_toolsets",
                "lymaille_profiles",
                "lymaille_providers",
                "lymaille_agents",
                "lymaille_toolsets",
                "lymaille_catalog_context_types",
                "lymaille_catalog_contexts",
                "lymaille_contexts",
                "lymaille_mcp_servers",
            ],
            "les noms documentés dans le bloc `//!` sont ceux des ressources"
        );
        for (premiere, premiere_nom) in noms.iter().enumerate() {
            for seconde_nom in noms.iter().skip(premiere + 1) {
                assert_ne!(
                    premiere_nom, seconde_nom,
                    "deux ressources ne peuvent partager un nom"
                );
            }
        }
    }
}
