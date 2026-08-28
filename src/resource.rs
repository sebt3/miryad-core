//! Contrat central [`MiryadResource`](crate::resource::MiryadResource) — une implémentation par entité SeaORM,
//! lue telle quelle par REST, GraphQL et MCP.
//!
//! Voir la doc crate pour un exemple complet et `docs/architecture.md` pour les détails.

use sea_orm::EntityTrait;
use serde::Serialize;

use crate::auth::AuthPrincipal;

/// Erreur métier retournée par un hook applicatif (`MiryadResource::before_create`) — jamais une
/// erreur *de* miryad-core, donc jamais de code `MRD-XXX-NNN` (cette convention identifie un
/// problème dans le framework, pas une règle métier qui rejette une requête). Le code est libre,
/// à la charge de l'app ; `None` si elle n'en a pas.
#[derive(Debug, Clone)]
pub struct HookError {
    pub code: Option<String>,
    pub message: String,
}

impl HookError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            code: None,
            message: message.into(),
        }
    }

    pub fn with_code(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: Some(code.into()),
            message: message.into(),
        }
    }
}

/// Politique d'accès à une entité exposée par miryad-core.
/// Read et write sont évalués séparément — une entité peut être publique en
/// lecture et restreinte en écriture (cas "recettes partagées, modifiables
/// par leur auteur uniquement").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum AccessPolicy {
    /// Tout utilisateur authentifié (JWT ou token API valide)
    Public,
    /// Uniquement l'utilisateur référencé par `owner_column` (+ les membres
    /// du groupe admin)
    OwnerOnly,
    /// Membres du groupe nommé (+ admin)
    Group(&'static str),
    /// Membres du groupe admin uniquement
    AdminOnly,
}

/// Contrat qu'implémente toute entité SeaORM exposée par miryad-core.
/// Une seule implémentation par entité — REST, GraphQL et MCP la lisent
/// telle quelle, aucune n'a sa propre déclaration de politique.
pub trait MiryadResource: EntityTrait {
    /// Nom exposé côté API (ex: "recipes") — utilisé pour les chemins REST,
    /// le type GraphQL, et le nom des tools MCP.
    fn resource_name() -> &'static str;

    fn read_policy() -> AccessPolicy;
    fn write_policy() -> AccessPolicy;

    /// Colonne portant l'identifiant du propriétaire. `None` si l'entité
    /// n'a pas de notion de propriétaire (ex: référentiel partagé comme la
    /// liste des ingrédients dans l'exemple recette).
    /// Doit être `Some` si `read_policy()` ou `write_policy()` retourne
    /// `AccessPolicy::OwnerOnly` — comportement non défini sinon (vérifié
    /// par test, pas par le compilateur à ce stade).
    fn owner_column() -> Option<<Self as EntityTrait>::Column>;

    /// Colonne texte sur laquelle la liste REST/GraphQL/MCP peut être filtrée
    /// (`?filter=valeur`, égalité exacte) — feature 4. `None` par défaut : pas
    /// de filtre pour cette entité. Une entité qui veut un filtre de liste
    /// (ex. "recettes par catégorie") le déclare explicitement.
    fn filter_column() -> Option<<Self as EntityTrait>::Column> {
        None
    }

    /// Colonne à afficher comme libellé humain de l'entité (liste, select) — feature 8, IR
    /// frontend. `None` par défaut : le générateur retombe sur la clé primaire.
    fn label_column() -> Option<<Self as EntityTrait>::Column> {
        None
    }

    /// Hook métier exécuté après RBAC (`can_create`), avant l'insertion — peut muter
    /// l'`ActiveModel` (champ dérivé, valeur calculée) ou rejeter l'opération avec une erreur
    /// métier. Miroir direct de `before_active_model_save` (Seaography, feature 5) : create only,
    /// car Seaography ne déclenche ce hook que sur un insert pour l'instant — un hook qui ne se
    /// comporterait pas à l'identique sur les 3 surfaces (REST/GraphQL/MCP) n'a pas sa place ici.
    /// Défaut : no-op.
    fn before_create(
        active: Self::ActiveModel,
        principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        let _ = principal;
        Ok(active)
    }
}
