# miryad-core — Contexte architectural

## Nature du projet

Crate Rust publique (crates.io + GitHub), moteur générique derrière le template d'application
**miryad**. Fournit le code commun (auth OIDC, RBAC/ownership par entité, API REST/GraphQL/MCP
génériques, moteur de workflow) ainsi qu'un binaire de scaffolding qui instancie une application
miryad à partir d'un modèle de données.

Contrairement à `vynil-core` (toolbox générique sans identité ni framework imposé), miryad-core
est **opinionated** : il prescrit axum, SeaORM, Seaography, apalis. La promesse "80% de
l'application vient gratuitement" exige ce choix — un moteur agnostique du framework ne peut pas
tenir cette promesse.

Licence : à trancher (probablement BSD-3, comme vynil-core/vynil, cohérence de l'écosystème).

## Position dans l'écosystème

```
miryad (Gitea kydah, privé, léger) ──dépend de──> miryad-core (GitHub, public, crates.io)
                                                          │
                                                          ├── vynil-core (Rhai/Handlebars)
                                                          ├── SeaORM 2.0 + Seaography 2.0
                                                          └── apalis + apalis-postgres + apalis-workflow
```

`miryad` (le template) reste le plus léger possible : bootstrap docs, personalities Qwen,
squelette d'app qui déclare ses entités et consomme miryad-core. Toute la mécanique vit ici.

**Frontière stricte** : miryad-core est une bibliothèque publiée sur crates.io, pas un déployable.
Dockerfile, chart Helm, doc de déploiement CNPG/Authentik n'ont pas leur place ici — c'est le
périmètre de `miryad`, qui produit l'application réellement déployée.

## Architecture cible (couches)

```
[ Frontend Vue 3 + shadcn-vue ]     <- composants scaffoldés dans le code consommateur, retouchables
        │  REST / GraphQL (OIDC cookie ou token API)
        ▼
[ Couche générique miryad-core ]    <- jamais de code par entité à écrire/maintenir
  ├─ REST CRUD générique (axum)
  ├─ GraphQL (Seaography 2.0, schéma dynamique depuis les entités SeaORM)
  ├─ MCP (tools CRUD générés par entité, sortie markdown)
  ├─ Auth (OIDC + session cookie + tokens API, dual-auth JWT/token)
  ├─ RBAC/ownership (trait `MiryadResource` par entité)
  └─ Workflow (apalis + apalis-postgres, step Rhai natif via vynil-core)
        │  SeaORM
        ▼
[ Entités SeaORM générées depuis les migrations ]
        │
        ▼
[ PostgreSQL (CNPG) ]
```

Point clé : REST, GraphQL et MCP ne sont **pas** trois implémentations séparées à maintenir par
entité — ils lisent tous la même métadonnée déclarée via le trait `MiryadResource` (nom exposé,
politique de lecture/écriture, colonne de propriétaire). Ajouter une entité au modèle de données
= implémenter ce trait, rien d'autre à écrire à la main dans ces trois couches.

## Composants (crate unique)

- **lib** (`src/lib.rs` + modules) : le moteur — resource/RBAC, auth, REST, GraphQL, MCP, workflow
- **bin** (`src/bin/miryad.rs`) : CLI de scaffolding — lit un modèle de données (format à définir,
  cf. roadmap) et génère/instancie une application depuis le template `miryad`

Pas de workspace multi-crates pour l'instant — une seule crate publiée, lib + bin, comme
`sandbox` de vanyline (bin secondaire `vanyline-maint`) ou `kydah-mcp-template`. À revisiter si la
crate devient difficile à faire évoluer d'un bloc.

## Interfaces inter-composants

| Source | Destination | Protocole | Auth |
|--------|-------------|-----------|------|
| Frontend | app consommatrice (REST) | HTTP REST | Cookie OIDC ou token API |
| Frontend | app consommatrice (GraphQL) | HTTP GraphQL | Cookie OIDC ou token API |
| Client MCP (LLM/agent) | app consommatrice | MCP HTTP streaming | JWT OIDC ou token API |
| app consommatrice | PostgreSQL (CNPG) | SeaORM | credentials DB (secret K8s) |
| Workflow (apalis) | PostgreSQL (CNPG) | apalis-postgres | même connexion DB |

## Logging

Convention héritée de vanyline/kydah-mcp-template : jamais `println!`/`dbg!`/`eprintln!` — logger
`tracing`. Détail de la config (format, niveaux) à définir en implémentant la feature Fondations.

## Stack technique

| Domaine | Choix | Justification |
|---------|-------|----------------|
| ORM | SeaORM 2.0 | Entités générées depuis migrations ; base commune REST/GraphQL/MCP |
| GraphQL | Seaography 2.0 | Schéma **dynamique** à l'exécution (async-graphql), pas de régénération à chaque évolution du modèle |
| REST | axum, générique fait main | Couche fine au-dessus du même trait d'entité que GraphQL/MCP |
| MCP | axum + JSON-RPC (cf. kydah-mcp-template) | Auth OIDC/JWKS + token API déjà éprouvée dans ce projet |
| Workflow | apalis + apalis-postgres + apalis-workflow | Support DAG natif, backend Postgres pur client (pas d'extension DB à installer — compatible CNPG générique), pas d'infra supplémentaire |
| Scripting workflow | vynil-core (Rhai) | Step-type natif pour les scripts de fallback définis par un admin |
| Auth | openidconnect (cf. `vanyline/app/src/auth/oidc.rs`) | Pattern OIDC déjà fonctionnel, à porter/généraliser |
| Frontend | Vue 3 + shadcn-vue + Tailwind | Composants scaffoldés dans le code consommateur, faible surface de maintenance |

## Structure des répertoires cible

```
miryad-core/
├── Cargo.toml
├── src/
│   ├── lib.rs
│   ├── resource.rs      # trait MiryadResource, AccessPolicy
│   ├── auth/             # OIDC, session cookie, tokens API, middleware dual-auth
│   ├── rest/              # routeur CRUD générique
│   ├── graphql/           # intégration Seaography
│   ├── mcp/                # tools CRUD génériques, sortie markdown
│   ├── workflow/           # intégration apalis + step Rhai
│   └── bin/
│       └── miryad.rs      # CLI de scaffolding
├── tests/
├── docs/
│   ├── roadmap.md
│   └── features/          # design docs en cours (jamais commités une fois clos, migrés dans architecture.md)
└── .github/workflows/      # CI (public -> GitHub Actions, pas Gitea)
```

## Commandes de validation

```bash
cargo check --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --check
cargo audit          # cf. CI kydah-mcp-template
```

## Conventions

- Pas de `println!`/`dbg!`/`eprintln!` — `tracing`
- Messages d'erreur avec identifiant unique (format à définir — cf. `VNL-AUTH-00X` dans vanyline
  comme précédent)
- TDD : tests avant implémentation
- `.tasks/` jamais commité
- Modifications atomiques : une tâche = un périmètre limité de fichiers
- Pas de `Co-Authored-By` dans les messages de commit

## Doc détaillée

- Roadmap grosse maille : `docs/roadmap.md`
- Design de la feature en cours : `docs/features/`
