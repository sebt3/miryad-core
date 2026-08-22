# Architecture — miryad-core

Ce fichier accumule, feature après feature, les décisions et contrats qui structurent réellement
le code (par opposition à `AGENTS.md`, qui décrit la cible visée avant implémentation, et à
`docs/roadmap.md`, qui découpe le travail restant).

## Fondations (feature 1, 2026-08-22)

### Trait `MiryadResource`

Contrat central sur lequel REST, GraphQL, MCP et le frontend générique se greffent — une seule
implémentation par entité SeaORM, lue telle quelle par les trois couches API (pas de déclaration
de politique dupliquée entre elles).

```rust
pub trait MiryadResource: EntityTrait {
    fn resource_name() -> &'static str;
    fn read_policy() -> AccessPolicy;
    fn write_policy() -> AccessPolicy;
    fn owner_column() -> Option<<Self as EntityTrait>::Column>;
}
```

`AccessPolicy` (`Public` / `OwnerOnly` / `Group(&'static str)` / `AdminOnly`) est évaluée
séparément en lecture et en écriture — une entité peut être publique en lecture et restreinte en
écriture (cas "recettes partagées, modifiables par leur auteur uniquement"). `owner_column()`
retourne `None` pour les entités sans notion de propriétaire (référentiel partagé) ; ce contrat
n'est pas encore vérifié par le compilateur quand `OwnerOnly` est utilisé sans colonne — à
surveiller quand l'évaluation RBAC réelle arrivera (feature "Utilisateurs & Groupes").

Source : `src/resource.rs`. Non branché à quoi que ce soit encore (pas de RBAC réel, pas de
REST/GraphQL/MCP) — le trait est prouvé compilable et testable sur deux entités d'exemple
(`tests/resource.rs`), rien de plus.

### Découverte : `Column` généré par SeaORM 2.0 n'implémente pas `PartialEq`

`DeriveEntityModel` dérive `Column` avec `Copy, Clone, Debug, EnumIter, DeriveColumn` — pas
`PartialEq`/`Eq`. Comparer une valeur de `Column` (ex. dans un test, ou plus tard dans l'évaluation
RBAC pour vérifier qu'une colonne correspond à `owner_column()`) doit passer par `matches!(...)`,
pas par `==`. À garder en tête pour la feature "Utilisateurs & Groupes", qui devra comparer des
`Column` pour l'évaluation RBAC réelle.

### CI

`.github/workflows/ci.yml` (test/fmt/clippy) et `publish.yml` (publish crates.io sur tag `v*`)
repris de `vynil-core`, adaptés : pas de matrice de features (un seul jeu pour l'instant, à
réintroduire si des features Cargo apparaissent — ex. `postgres` vs `sqlite`).
