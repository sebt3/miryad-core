//! Ressources de définition de l'hôte `lymaille` (feature `lymaille`) — contrat porté par
//! `./mod.sdd`.
//!
//! Les quatre entités `MiryadResource` que l'admin administre à la main au lot 1 (DSD `DEC-14`),
//! par le CRUD REST et MCP générique de la crate — le montage applicatif (`rest::resource_router`)
//! n'est pas de ce module :
//!
//! - [`agent::Entity`] — `lymaille_catalog_agents` : définitions d'agent du catalogue, lecture
//!   `Public`, écriture `AdminOnly`, filtre et libellé sur les colonnes dérivées ;
//! - [`toolset::Entity`] — `lymaille_catalog_toolsets` : toolsets du catalogue, version active et
//!   brouillon sur la même ligne (DSD `DEC-18`), mêmes politiques ;
//! - [`profile::Entity`] — `lymaille_profiles` : profils de modèle, mêmes politiques ;
//! - [`provider::Entity`] — `lymaille_providers` : moteurs d'inférence, `AdminOnly` en lecture
//!   comme en écriture (DSD `DEC-16`).
//!
//! Toutes sont du catalogue : écriture `AdminOnly`, `owner_column() = None`, et — agents et
//! toolsets — identifiant préfixé `G_` (DSD `DEC-9`). Les lignes privées (`agents`, `toolsets`,
//! `owner_id` non nul) vivent dans des tables distinctes et auront leur propre paire de ressources
//! `OwnerOnly` au lot 2 (DSD `DEC-19`). Les tables internes (sessions, mémoires, types de
//! contexte, contextes) n'ont pas d'entité ici (DSD `DEC-14`).
//!
//! Aucun ré-export à plat : les noms `Entity`, `Model` et `ActiveModel` collisionnent ; chaque
//! entité s'atteint par son chemin, par exemple
//! `rest::resource_router::<definitions::agent::Entity>()` (même motif que
//! `workflow::definition::Entity`). Les secrets n'y figurent jamais : un `provider` ne porte que
//! des références, validées comme telles à l'écriture.

/// Définitions d'agent du catalogue (`src/lymaille/definitions/agent.sdd`).
pub mod agent;

/// Toolsets du catalogue, version active et brouillon (`src/lymaille/definitions/toolset.sdd`).
pub mod toolset;

/// Profils de modèle (`src/lymaille/definitions/profile.sdd`).
pub mod profile;

/// Moteurs d'inférence, références seules (`src/lymaille/definitions/provider.sdd`).
pub mod provider;

/// Types de contexte du catalogue (`src/lymaille/definitions/context_type.sdd`).
pub mod context_type;

#[cfg(test)]
mod tests {
    use super::agent;
    use super::profile;
    use super::provider;
    use super::toolset;
    use crate::resource::MiryadResource as _;

    /// Scenario « les quatre entités sont atteignables » : les quatre chemins résolvent et leurs
    /// `resource_name()` sont distincts deux à deux (le montage du moteur refuserait un doublon).
    #[test]
    fn les_quatre_ressources_sont_atteignables_et_leurs_noms_distincts() {
        let noms = [
            agent::Entity::resource_name(),
            toolset::Entity::resource_name(),
            profile::Entity::resource_name(),
            provider::Entity::resource_name(),
        ];
        assert_eq!(
            noms,
            [
                "lymaille_catalog_agents",
                "lymaille_catalog_toolsets",
                "lymaille_profiles",
                "lymaille_providers",
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
