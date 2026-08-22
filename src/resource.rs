use sea_orm::EntityTrait;

/// Politique d'accès à une entité exposée par miryad-core.
/// Read et write sont évalués séparément — une entité peut être publique en
/// lecture et restreinte en écriture (cas "recettes partagées, modifiables
/// par leur auteur uniquement").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}
