# Domain Definition: Miryad Core

## Purpose

`miryad-core` est la bibliothèque Rust opinionated (axum + SeaORM) qui sert de socle à toutes les applications
`miryad` : une application consommatrice déclare ses entités et obtient authentification OIDC, autorisation, API REST
et MCP, publication OpenAPI, représentation intermédiaire pour le frontend, migrations et exécution de workflows, sans
réécrire ce socle. Publiée sur crates.io sous licence BSD-3-Clause ; l'API est instable avant `1.0`.

## Scope and Boundary

Le Domain couvre ce que la crate fournit **comme bibliothèque** à une application consommatrice.

Hors Domain : tout ce qui se déploie (Dockerfile, chart Helm, doc de déploiement CNPG ou Authentik) et la génération du
frontend (composants Vue, générateur TypeScript). Cela appartient au template `miryad` et à son propre roadmap.

Source : `roadmap@c62a184`, section « Hors périmètre » ; `arch@c62a184`, « Support frontend ».

## Source Aliases

Les documents `docs/architecture.md` et `docs/roadmap.md` ont été remplacés par cette bibliothèque DSD. Leurs
contenus d'origine restent consultables par Git :

- `arch@c62a184` : `git show c62a184:docs/architecture.md`
- `roadmap@c62a184` : `git show c62a184:docs/roadmap.md`

## Terminology

- **Application consommatrice** (ou app fille) : le binaire qui dépend de `miryad-core`, par exemple `vanyline`. Elle
  possède son `main()`, son routeur et son infrastructure.
- **Entité** : une table SeaORM que l'app déclare comme `MiryadResource` pour obtenir REST, MCP, IR et RBAC.
- **Principal** (`AuthPrincipal`) : l'identité authentifiée d'une requête, issue d'une session, d'un token API ou d'un
  JWT Bearer.
- **Surface** : une façon d'exposer les entités (REST, MCP).
- **Profil sans base** : la crate compilée sans la feature `db`.
- **Kind, DAG, run, `vars`** : voir `miryad-core/workflow`. Les `vars` sont les variables constantes d'un run.

## Business Objectives

- **BO-1:** Une application consommatrice n'écrit que ses entités et ses spécificités métier ; l'authentification,
  l'autorisation, les API et l'exécution de workflows sont fournis.
    - **Source:** `AGENTS.md`, section « Projet »

- **BO-2:** Une application qui n'a besoin que d'authentification et de publication d'API (par exemple une API écrite à
  la main qui relaie l'`id_token` à l'API server Kubernetes) peut utiliser la crate sans base de données.
    - **Source:** `arch@c62a184`, « Profil sans base de données »

## Domain Constraints

- **CON-1 - MUST:** La crate est une bibliothèque : elle ne gère jamais l'infrastructure qu'elle consomme (base,
  cluster Restate, homeserver Matrix, ingress) et n'impose pas la forme du déploiement.
    - **Source:** `roadmap@c62a184`, « Hors périmètre »

- **CON-2 - MUST NOT:** Le code de production n'utilise ni `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`,
  `dbg!`, `println!`, ni indexation `[]` non gardée ; toute trace passe par `tracing`. La dette préexistante ne grossit
  pas et les fichiers touchés par une tâche n'ont aucun warning.
    - **Source:** `AGENTS.md`, « Harnais clippy » ; `SpecDD /tooling.sdd`

- **CON-3 - MUST:** Toute erreur interne à la crate porte un code unique `MRD-<DOMAINE>-NNN`. Un `DbErr` de passage
  n'est pas une erreur de la crate : il traverse nu jusqu'à la surface qui le traduit.
    - **Source:** `AGENTS.md`, « Conventions de code » ; `SpecDD /.specdd/bootstrap.project.md`

- **CON-4 - MUST:** Une capacité optionnelle vit derrière une feature Cargo. La batterie de tests et de clippy couvre
  chaque combinaison de features déclarée par `SpecDD /tooling.sdd`.
    - **Source:** `SpecDD /tooling.sdd`

- **CON-5 - MUST NOT:** Une migration committée n'est pas modifiée ; on ajoute la suivante. Exceptions datées dans
  `AGENTS.md` (2026-10-04 pour `m20260923_000001`, 2026-10-17 pour `m20261006_000001`), valables tant qu'aucune
  production n'existe.
    - **Source:** `AGENTS.md`, « Conventions de code »

- **CON-6 - MUST:** Toute table interne est préfixée `miryad_` (`miryad_matrix_` et `miryad_workflow_` pour les features
  correspondantes), pour ne jamais collisionner avec le schéma de l'application.
    - **Source:** `arch@c62a184`, « Migrations », « Conventions transverses »

- **CON-7 - MUST:** Toute fonction qui doit pouvoir être appelée depuis une migration est générique sur
  `C: ConnectionTrait`, car une migration fournit un `SchemaManagerConnection` et non une `DatabaseConnection`.
    - **Source:** `arch@c62a184`, « Conventions transverses »

- **CON-8 - MUST NOT:** Aucun fichier de `src/` n'existe sans spec `.sdd` approuvée ; le code ne précède pas la spec.
    - **Source:** `SpecDD /.specdd/bootstrap.project.md`

## Domain Requirements

- **FR-1 - MUST:** Tout comportement qui touche `MiryadResource` est honoré à l'identique en REST et en MCP. Un écart de
  surface est une erreur de contrat, pas une dette. Les exemptions sont explicites (routes de compte, REST-only).
    - **Source:** `SpecDD /.specdd/bootstrap.project.md`, « Contrats transverses »
    - **Realized by:** SpecDD /src/rest/core.sdd, SpecDD /src/mcp/mod.sdd
    - **Verified by:** `AC-1`

- **NFR-1 - MUST:** Sans la feature `db`, `sea-orm` n'est pas dans le graphe de dépendances normales de la crate.
    - **Source:** `AGENTS.md`, « Commandes » ; `SpecDD /tooling.sdd`
    - **Verified by:** `AC-2`

## Domain Decisions

- **DEC-1 - GraphQL supprimé (2026-10-19)**
    - **Context:** prototype sans consommateur en production, avec une désynchronisation de clé de politique en
      fail-open, des hooks update et delete non pilotés par Seaography `2.0.0-rc.9`, et une dette coûteuse (carve-out
      clippy, feature `sea-orm/seaography`, dépendance figée en release candidate).
    - **Decision:** la surface applicative est REST et MCP.
    - **Consequences:** aucune mention de GraphQL ou de Seaography ne subsiste hors historique.
    - **Source:** `arch@c62a184`, « Décisions retirées »

## Solution Landscape

- `miryad-core/resource-model` : le trait `MiryadResource`, les migrations internes et les hooks CRUD.
- `miryad-core/auth` : authentification OIDC, session, Bearer JWT, tokens API, comptes de service.
- `miryad-core/access-control` : utilisateurs, groupes, synchronisation depuis l'IdP et RBAC.
- `miryad-core/rest-api` : REST générique, pagination, filtre, OpenAPI et Swagger UI.
- `miryad-core/mcp` : serveur MCP et rendu des sorties.
- `miryad-core/workflow` : moteur de workflow à DAG-donnée sur Restate.
- `miryad-core/systemone` : client System One et son kind de workflow.
- `miryad-core/matrix` : client Matrix de bot, chiffrement de bout en bout et son kind de workflow.
- `miryad-core/frontend` : IR du frontend et service statique.

- `miryad-core/lymaille` : hôte de la bibliothèque `lymaille` — modules LLM sur les ressources, pad de workflow,
  sous-agents. Ouverte le 2026-11-01, rien d'implémenté ; préalable : première release de `lymaille`. La réserve de
  `miryad-core/workflow#OI-8` y est rattachée.

Relations : `rest-api`, `mcp` et `frontend` consomment `resource-model` et `access-control` ; `auth` produit le principal
que `rest-api` et `mcp` consomment ; `systemone` et `matrix` fournissent des kinds à `workflow` ; `lymaille` (cible)
consomme `resource-model`, `access-control` et `auth`, et fournit moteur LLM et pad à `workflow` et au glue `matrix`.

## Acceptance Criteria

- **AC-1:** Pour chaque comportement de `MiryadResource` (RBAC, hooks, pagination, filtre), un `Scenario` existe côté
  REST et côté MCP, ou l'exemption est écrite.
- **AC-2:** Les trois commandes `cargo tree … -e normal -i sea-orm` de `AGENTS.md` n'impriment rien sur stdout.

## Legacy Roadmap References

Les anciennes références « roadmap N » encore présentes dans des lignes d'historique de specs se résolvent ainsi :

| Ancienne référence | Document actuel |
| --- | --- |
| roadmap 2a (OIDC, cookie) | `miryad-core/auth` |
| roadmap 2b (tokens API) | `miryad-core/auth` |
| roadmap 2c (comptes de service) | `miryad-core/auth` |
| roadmap 4b (OpenAPI) | `miryad-core/rest-api` |
| roadmap 7, 9 (moteur de workflow) | `miryad-core/workflow` |
| roadmap 7b (hooks CRUD) | `miryad-core/resource-model` |
| roadmap 8 (support frontend) | `miryad-core/frontend` |
| roadmap 10 (filtrage étendu) | `miryad-core/rest-api` |
| roadmap 11 (System One) | `miryad-core/systemone` |
| roadmap 12 (Matrix) | `miryad-core/matrix` |

## Open Issues

- **OI-1:** Une feature n'entre pas en spec sans arbitrage explicite. Le registre de ces arbitrages vit désormais dans
  les Open Issues et les `TBD` de chaque Solution, et non plus dans une page de roadmap unique. **TBD :** décider si une
  vue transverse des Open Issues est utile. Elle s'obtient aujourd'hui par `grep -rn "Open Issues" dsd/`.
