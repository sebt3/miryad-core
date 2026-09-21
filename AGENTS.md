# AGENTS.md — miryad-core

Before working on this project, read `.specdd/bootstrap.md`, then `.specdd/bootstrap.project.md`.

Assume the role, rules, workflow and implementation constraints described in SpecDD. Treat the
SpecDD specs as source-adjacent development contracts, not optional documentation.

## Projet

Crate Rust **opinionated** (axum + SeaORM + Seaography) publiée sur crates.io, moteur générique
derrière le template d'application `miryad`. Une application consommatrice déclare ses
entités via le trait @MiryadResource et obtient auth OIDC, RBAC/ownership, REST, GraphQL, MCP,
OpenAPI, IR frontend et migrations. Le moteur de workflow (apalis + step Rhai) est en standby —
aucune dépendance `apalis` dans `Cargo.toml` tant que l'option d'implémentation n'est pas
tranchée (`docs/roadmap.md`, items 7 et 9). BSD-3-Clause, API instable avant `1.0`.

Le contexte durable vit ici et n'est pas à re-déduire du code :

- `miryad-core.sdd` : spec racine — périmètre, frontières, interdits de la crate, checklist de
  production des specs
- `src/**/*.sdd` : le **contrat** d'un fichier source — une spec par fichier de `src/`, même
  basename dans le même répertoire (`src/auth/oidc.rs` ↔ `src/auth/oidc.sdd`)
- `tooling.sdd` : le harnais clippy/rustfmt/lints et sa batterie de vérification
- `.github/workflows/workflows.sdd` : ce que la CI doit prouver
- `docs/architecture.md` : le récit d'architecture et les décisions (le *pourquoi*, les pièges
  constatés dans le source des dépendances amont)
- `docs/roadmap.md` : la priorisation grosse maille — une feature n'y entre pas sans arbitrage

## Flux de travail : SpecDD + test-first (TTD)

`Spec approuvée → tests depuis les Scenario (rouges) → implémentation minimale → validation → [x]`

- **Aucun fichier de `src/` sans spec approuvée par Sébastien.** Le code ne précède jamais la spec.
- Une spec décrit **tout** le comportement observable de son fichier (`Exposes`, `Accepts`,
  `Returns`, `Raises`, `Handles`, `Must`, `Must not`, `Scenario`) — une description partielle
  n'est pas une spec.
- Les `Scenario` sont la matière première des tests : l'agent `implementer` les convertit d'abord
  en tests qui échouent, puis écrit le minimum de code de production qui les fait passer.
- Aucune tâche `Tasks` ne passe à `[x]` sans synthèse `validator` au vert.
- Le détail du flux, la règle de regroupement de fichiers et les contrats transverses
  (`MRD-*`, `tracing`, gating de features, parité REST/GraphQL/MCP) sont dans
  `.specdd/bootstrap.project.md`.

## Agents (`.opencode/agents/`)

| Agent | Mode | Rôle |
|---|---|---|
| `spec-dd` | primary | Rédige les specs avec Sébastien et orchestre les sous-agents. Ne code jamais. |
| `spec-reverse` | subagent | Reverse-engineering : produit la spec la plus complète possible d'un fichier existant, sans jamais toucher le code. |
| `implementer` | subagent | Réalise une spec précise : tests d'abord depuis les `Scenario`, puis implémentation minimale dans `Owns`/`Can modify`. |
| `validator` | subagent | Lecture seule : confronte spec ↔ code ↔ tests, lance la batterie, produit une synthèse d'écarts. |

## Harnais clippy

La table `[lints]` de `Cargo.toml` porte le harnais (contractualisé par `tooling.sdd`) :
`unsafe_code` interdit, `missing_docs` warn, `pedantic` et `cargo` en `deny`, famille stricte en
`deny` (`unwrap_used`, `expect_used`, `panic`, `unreachable`, `dbg_macro`, `todo`,
`unimplemented`, `print_stdout`, `print_stderr`, `arithmetic_side_effects`, `indexing_slicing`,
`unwrap_in_result`, `panic_in_result_fn`). Seul `multiple_crate_versions` est `allow`
(doublons imposés par les dépendances amont).

En production : aucun `unwrap` / `expect` / `panic!` / `todo!` / `unimplemented!` / `dbg!` /
`println!` ; pas d'indexation `[]` non gardée. La dette préexistante est tracée dans
`tooling.sdd` : elle ne doit pas grossir, et **zéro warning sur les fichiers touchés par une
tâche**. La purge se fait module par module, avec Sébastien.

## Commandes

Avant toute tâche `[x]`, la batterie complète (définie par `tooling.sdd`, reprise par
`validator`) :

```bash
cargo test
cargo test --no-default-features
cargo test --no-default-features --features swagger-ui
cargo test --no-default-features --features graphql
cargo test --no-default-features --features graphiql
cargo test --no-default-features --features mcp
cargo test --all-features
cargo clippy --all-targets -- -D warnings
cargo clippy --no-default-features --all-targets -- -D warnings
cargo clippy --no-default-features --features swagger-ui --all-targets -- -D warnings
cargo clippy --no-default-features --features graphql --all-targets -- -D warnings
cargo clippy --no-default-features --features graphiql --all-targets -- -D warnings
cargo clippy --no-default-features --features mcp --all-targets -- -D warnings
cargo clippy --all-features --all-targets -- -D warnings
cargo fmt --check
```

Le harnais doit tourner sur **toutes** ces combinaisons : des lints `pedantic`
(`must_use_candidate`, `missing_errors_doc`) dépendent du graphe de features et peuvent ne se
déclencher que sur l'une d'elles. Les `deny` de `[lints]` font déjà échouer clippy sans `-D
warnings` ; le `-D warnings` n'est là que pour les avertissements hors harnais.

Outils SpecDD disponibles via `npx specdd` (`lint`, `resolve`, `inspect`) — `specdd lint` valide
la syntaxe `.sdd`, `specdd resolve <cible>` vérifie la chaîne de specs avant de travailler.

## Conventions de code

- `tracing` pour toute trace ; jamais `println!` / `eprintln!` / `dbg!`.
- Erreurs internes à la crate : code unique `MRD-<DOMAINE>-NNN`. @HookError reste une erreur
  applicative de l'application consommatrice, sans code `MRD-*`.
- Une tâche = un périmètre de fichiers limité, défini par le `Owns`/`Can modify` de la spec.
- Modification d'une migration committée interdite : on ajoute la migration suivante.

## Git

- Pas de `Co-Authored-By` dans les messages de commit.
- Spec, code et tests dans la même modification ; tâche `[x]` seulement après vérification verte.
- `git push` uniquement sur demande explicite de Sébastien — il n'est jamais une étape de clôture.

## Doc détaillée

- `.specdd/bootstrap.project.md` — le flux, la règle une-spec-par-fichier, les contrats transverses
- `docs/roadmap.md` — priorisation grosse maille
- `docs/architecture.md` — architecture par module, décisions et pièges
