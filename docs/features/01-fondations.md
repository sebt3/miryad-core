# Feature 01 — Fondations

**Statut : brouillon, pas encore validé.** Avant dispatch à Cadence, le développeur principal et
Claude doivent chacun donner leur accord explicite ("c'est prêt") — cf. `.claude/config.md`,
Phase 1.

## Ce que ça fait

Met en place le squelette de la crate `miryad-core` : workspace Cargo, CI de base
(fmt/clippy/test), conventions de logging, et le trait central `MiryadResource` qui décrira, pour
toute entité SeaORM future, sa politique d'accès (lecture/écriture) et sa colonne de propriétaire.
C'est le contrat sur lequel REST, GraphQL, MCP et le frontend générique viendront se greffer dans
les features suivantes (cf. `docs/roadmap.md`, étapes 2-8).

## Ce que ça ne fait pas

- Pas de REST/GraphQL/MCP/front — uniquement le trait, prouvé par une entité d'exemple
- Pas d'évaluation RBAC réelle (le trait *déclare* une politique, rien ne l'*applique* encore —
  ça viendra avec l'étape "Utilisateurs & Groupes" de la roadmap)
- Pas de connexion PostgreSQL/CNPG réelle — tests sur SQLite in-memory (via SeaORM) pour prouver
  la compilation et le comportement, sans dépendance externe
- Pas de binaire de scaffolding fonctionnel — juste un stub `src/bin/miryad.rs` qui répond à
  `--version` et rien d'autre
- Pas de dépendance à Seaography/apalis/axum à ce stade — ces briques arrivent dans les features
  qui en ont besoin, pas avant

## Interfaces clés

```rust
// src/resource.rs

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
}
```

## Fichiers à modifier / créer

- `Cargo.toml` (nouveau) — crate `miryad-core`, edition 2021 ou 2024 (aligner sur vynil-core/
  vanyline), dépendances : `sea-orm` (features `sqlx-sqlite` pour les tests, `sqlx-postgres` pour
  plus tard), pas d'autre dépendance à ce stade
- `src/lib.rs` (nouveau) — `pub mod resource;` et rien d'autre
- `src/resource.rs` (nouveau) — le trait et l'enum ci-dessus
- `src/bin/miryad.rs` (nouveau) — stub : `clap` avec juste `--version`, aucune autre logique
- `.github/workflows/ci.yml` (nouveau) — jobs `fmt`/`clippy`/`test`, calqués sur
  `kydah-mcp-template/.gitea/workflows/ci.yml` mais en syntaxe GitHub Actions (dépôt public sur
  GitHub, pas Gitea)
- `rustfmt.toml` (nouveau) — copier celui de `vynil-core`
- `tests/resource.rs` (nouveau) — voir section Tests

## Tests

Fichier : `tests/resource.rs`

- Définir une entité SeaORM minimale d'exemple (ex: `Recipe` avec `id`, `title`, `owner_id`) dans
  le fichier de test
- Implémenter `MiryadResource` dessus : `resource_name() == "recipes"`,
  `read_policy() == AccessPolicy::Public`, `write_policy() == AccessPolicy::OwnerOnly`,
  `owner_column() == Some(Column::OwnerId)`
- Test : les quatre méthodes retournent les valeurs attendues (compile-time + assertions runtime
  triviales — l'objectif est de prouver que le trait est implémentable proprement sur une entité
  SeaORM réelle, pas de tester une logique complexe)
- Un second cas : une entité sans propriétaire (`owner_column() -> None`) avec
  `read_policy() == AccessPolicy::AdminOnly` — prouve que le cas "pas de owner" compile aussi

## Commandes de validation

Voir `AGENTS.md` section "Commandes de validation" — `cargo check`, `cargo test`,
`cargo clippy -- -D warnings`, `cargo fmt --check`.

## Commit

`feat(fondations): workspace + trait MiryadResource`

Synthèse attendue : mise en place du squelette de la crate et du contrat central sur lequel les
features suivantes (auth, REST, GraphQL, MCP) viendront se greffer.

## Risques / questions ouvertes

- `AccessPolicy` ne couvre pas encore le masquage par champ (ex: un champ visible pour l'auteur
  mais pas pour les autres lecteurs) — explicitement hors-scope MVP pour l'instant (cf. MEMORY.md),
  à revisiter si un vrai besoin apparaît
- `owner_column()` suppose un seul propriétaire par entité (une colonne). Suffisant pour le cas
  "mes propres recettes" de l'exemple cible ; pas conçu pour un ownership partagé/multi-utilisateur
  — à ouvrir en feature séparée si besoin
- Le nom et la forme exacte du trait peuvent encore bouger une fois qu'on le branche réellement à
  REST (feature suivante) — ce design n'engage que cette feature, pas la suite
