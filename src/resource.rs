//! Contrat central [`MiryadResource`](crate::resource::MiryadResource) — une implémentation par entité SeaORM,
//! lue telle quelle par REST, GraphQL et MCP.
//!
//! Voir la doc crate pour un exemple complet et `docs/architecture.md` pour les détails.

use sea_orm::EntityTrait;
use serde::Serialize;

use crate::auth::AuthPrincipal;

/// Erreur métier retournée par un hook applicatif (`before_create`, `before_update`,
/// `before_delete`) — jamais une
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
    /// du groupe admin). Sans `owner_column` déclarée, c'est une déclaration
    /// invalide — voir [`MiryadResource::owner_column`].
    OwnerOnly,
    /// Membres du groupe nommé (+ admin)
    Group(&'static str),
    /// Membres du groupe admin uniquement
    AdminOnly,
}

/// Contrat qu'implémente toute entité `SeaORM` exposée par miryad-core.
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
    /// Être `None` alors que `read_policy()` ou `write_policy()` retourne `AccessPolicy::OwnerOnly`
    /// est une **déclaration invalide** (arbitré 2026-09-27), jamais un choix runtime à arbitrer
    /// par requête : `rest::mod` refuse de monter le routeur d'une telle entité (panic au montage,
    /// même mécanisme que la collision de `resource_name`) et `rbac::can_create` refuse par
    /// cohérence défensive avec `can_read`/`can_write`. Ce fichier ne vérifie rien lui-même à la
    /// compilation — l'invalidité se prouve en aval, le comportement est documenté ici.
    fn owner_column() -> Option<<Self as EntityTrait>::Column>;

    /// Colonne texte sur laquelle la liste peut être filtrée côté REST et MCP seulement
    /// (`?filter=valeur`, égalité exacte) — feature 4. GraphQL ne lit jamais cette colonne :
    /// le pont GraphQL ne construit que la clause propriétaire, le filtrage y passe par les
    /// inputs natifs `seaography` (arbitré 2026-09-27). `None` par défaut : pas
    /// de filtre pour cette entité. Une entité qui veut un filtre de liste
    /// (ex. "recettes par catégorie") le déclare explicitement.
    #[must_use]
    fn filter_column() -> Option<<Self as EntityTrait>::Column> {
        None
    }

    /// Colonne à afficher comme libellé humain de l'entité (liste, select) — feature 8, IR
    /// frontend. `None` par défaut : le générateur retombe sur la clé primaire.
    #[must_use]
    fn label_column() -> Option<<Self as EntityTrait>::Column> {
        None
    }

    /// Hook métier exécuté après RBAC (`can_create`), avant l'insertion — peut muter
    /// l'`ActiveModel` (champ dérivé, valeur calculée) ou rejeter l'opération avec une erreur
    /// métier. Miroir direct de `before_active_model_save` (Seaography, feature 5) : create only,
    /// car Seaography ne déclenche ce hook que sur un insert pour l'instant — un hook qui ne se
    /// comporterait pas à l'identique sur les 3 surfaces (REST/GraphQL/MCP) n'a pas sa place ici.
    /// Défaut : no-op.
    ///
    /// # Errors
    ///
    /// Un `Err(HookError)` remonté par l'override de l'application consommatrice (code applicatif
    /// libre, jamais un code `MRD-*` de la crate). Le défaut ne rejette jamais.
    fn before_create(
        active: Self::ActiveModel,
        principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        let _ = principal;
        Ok(active)
    }

    /// Hook métier de la mise à jour — position symétrique de [`before_create`](Self::before_create)
    /// sur la création : après RBAC (`can_write`, évalué sur la ligne relue) et après
    /// `rest::core::mark_all_set`, avant les deux invariants de `rest::core::update` (forçage de la
    /// clé primaire à l'id du chemin, reconduction de `owner_column` depuis `existing`) — un hook
    /// hostile est défait par le même ordre que sur la création. `existing` (le `Model` relu avant
    /// mise à jour, la même ligne qu'a évaluée `can_write`) est prêté en lecture seule : comparer
    /// avant/après (ex. valider une transition d'état) ne demande pas de relire la base soi-même.
    /// Déclenché sur REST et MCP seulement, **jamais sur GraphQL** — Seaography `2.0.0-rc.9` ne
    /// pilote son hook équivalent (`before_active_model_save`) qu'à l'insertion ; asymétrie actée
    /// par l'amendement `resource.sdd` du 2026-09-23 à la règle de parité. Défaut : identité —
    /// `Ok(active)`, `existing` et `principal` ignorés.
    ///
    /// # Errors
    ///
    /// Un `Err(HookError)` remonté par l'override de l'application consommatrice (code applicatif
    /// libre, jamais un code `MRD-*` de la crate). Le défaut ne rejette jamais.
    fn before_update(
        active: Self::ActiveModel,
        existing: &Self::Model,
        principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        let _ = existing;
        let _ = principal;
        Ok(active)
    }

    /// Hook métier de la suppression : exécuté après RBAC (`can_write`, évalué sur la ligne
    /// relue), avant l'exécution du `DELETE` — un `Err` interrompt avant toute requête. Aucun
    /// `ActiveModel` n'existe pour une suppression, seul `existing` (le `Model` relu) est prêté,
    /// en lecture seule. Déclenché sur REST et MCP seulement, **jamais sur GraphQL** (même
    /// asymétrie actée que [`before_update`](Self::before_update), amendement `resource.sdd` du
    /// 2026-09-23). Défaut : `Ok(())`, `existing` et `principal` ignorés.
    ///
    /// # Errors
    ///
    /// Un `Err(HookError)` remonté par l'override de l'application consommatrice (code applicatif
    /// libre, jamais un code `MRD-*` de la crate) — il interrompt avant toute requête `DELETE`.
    /// Le défaut ne rejette jamais.
    fn before_delete(existing: &Self::Model, principal: &AuthPrincipal) -> Result<(), HookError> {
        let _ = existing;
        let _ = principal;
        Ok(())
    }
}
