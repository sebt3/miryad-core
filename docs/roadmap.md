# Roadmap — miryad-core

Grandes étapes vers un scaffolding utilisable. Tout ce qui suit fait partie du MVP — pas de
relégation en "phase 2" pour ces items (décision explicite : le moteur de workflow est un pilier,
pas un bonus). L'ordre reflète les dépendances techniques, pas une priorité produit.

Chaque ligne devient une ou plusieurs specs `.sdd` (et leurs `Tasks`) au moment d'y arriver —
pas de design détaillé à l'avance au-delà de ce qui est nécessaire pour ordonner le travail.

## 1. Fondations
Workspace Cargo, CI (fmt/clippy/test/audit — cf. `kydah-mcp-template`), conventions de logging
(`tracing`), format des identifiants d'erreur. Trait central `MiryadResource` : politique de
lecture/écriture par entité, colonne de propriétaire. Rien de branché dessus encore — juste le
contrat.

## 2a. Auth — OIDC + session cookie
OIDC (porté depuis `vanyline/app/src/auth/oidc.rs`) + session cookie pour le frontend : login,
callback, logout, extracteur `AuthUser`. Pas de tokens API ni de dual-auth ici — juste le flow
navigateur, scindé de 2b pour rester dans une taille de feature raisonnable (décision du
2026-08-22).

## 2b. Auth — tokens API + dual-auth
Entité `ApiToken` (stockage hashé, `subject: String` sans FK vers `User`) + middleware dual-auth
(cookie de 2a **ou** token API) réutilisable par REST, GraphQL et MCP. La feature 3 résout
`subject` → `User` par requête (get-or-create), pas par contrainte de schéma — décision actée en
feature 3, pas de FK ajoutée après coup.

## 2c. Auth — comptes de service
Fonction idempotente (`ensure_service_account`), appelée explicitement par l'app cible à son
démarrage (après ses migrations, si elle le décide) : garantit l'existence d'un compte "machine"
(jamais de login OIDC), membre des groupes donnés, authentifiable par un token dont la valeur est
fournie par l'appelant (pas générée aléatoirement) — typiquement lue d'une variable
d'environnement, pour que l'automatisation de déploiement (kuberest) connaisse le secret à
l'avance. Décidé le 2026-08-22, après la feature 4.

## 3. Utilisateurs & Groupes
Modèle `User`/`Group`/`GroupMembership`, groupe `admin` pré-câblé (seedé par migration).
Appartenance synchronisée depuis le claim `groups` OIDC à chaque login — Authentik décide,
miryad-core reflète (pas d'API d'assignation manuelle). Évaluation RBAC (owner-only / groupe /
admin / public) branchée sur le trait `MiryadResource` de l'étape 1.

## 4. API REST générique
Routeur CRUD générique (axum) construit depuis le trait `MiryadResource` + RBAC de l'étape 3.
Aucune route à écrire par entité. Liste paginée (page/per_page, défaut 100, plafond 1000) et
filtrable sur un champ texte unique déclaré par l'entité.

## 4b. OpenAPI + Swagger UI (Swagger UI optionnel)
Génération d'un document OpenAPI 3 pour les routes CRUD génériques de la feature 4 — toujours
disponible (`utoipa` en dépendance normale), construite via l'API bas niveau d'`utoipa` (pas la
macro `#[utoipa::path]`, qui exige une fonction concrète par route) pour rester générique par
entité, sans boilerplate. Seule la UI Swagger est derrière une feature Cargo `swagger-ui`
(activable par miryad-core et transitivement par l'app cible). Décidé le 2026-08-22, après la
feature 4.

**À planifier (arbitré 2026-09-29, non MVP)** : fragment OpenAPI des routes de compte de la crate
(`GET /api/v1/me`, `GET /api/v1/users`, `/api/v1/tokens`) — aujourd'hui absentes du document, le
template `miryad` en aura besoin pour son client. Ces routes restent REST-only par nature
(exemption de parité, `.specdd/bootstrap.project.md`) ; `/auth/*` reste hors document.

## 5. API GraphQL
Intégration Seaography 2.0 (schéma dynamique depuis les entités SeaORM) + injection du RBAC de
l'étape 3 dans la résolution, via `LifecycleHooksInterface` (pas le RBAC natif de Seaography/
SeaORM — table-level et un seul rôle par utilisateur, incompatible avec `OwnerOnly` et le
multi-groupe de l'étape 3). Deux features Cargo distinctes : `graphql` (le cœur) et `graphiql`
(le client interactif). Pas de subscriptions — nécessiterait un mécanisme de détection de
changement cohérent avec tous les chemins d'écriture (REST compris), pas juste câblé en dépendance
aujourd'hui ; à reprendre en feature séparée si le besoin se confirme.

## 6. Serveur MCP
Tools CRUD générés par entité (list/get/create/update/delete), sortie configurable par l'app
(json/yaml/markdown, ou template Handlebars custom — un seul mécanisme de rendu, cf.
`docs/architecture.md`). Dual-auth et RBAC réutilisés (`rest/core.rs`). Base : patterns
`auth.rs`/`mcp.rs` de `kydah-mcp-template`.

Implémentée le 2026-08-23, une fois le blocage amont levé (`vynil-core` v0.7.3, cf.
[sebt3/vynil-core#7](https://github.com/sebt3/vynil-core/issues/7) et
[sebt3/vynil-core#8](https://github.com/sebt3/vynil-core/issues/8)) — feature Cargo `mcp`
(`vynil-core`, features `hbs` + `crypto`).

## 7. Moteur de workflow — **implémenté sur Restate (0.1.4)**
Livré derrière la feature Cargo `workflow` : `restate-sdk` 0.12 (cluster Restate self-hosté, jamais géré
par la crate), `DagInterpreter` (service `#[workflow]`, marche d'un DAG par couches), `StepDispatcher`
(service qui exécute un step par `kind` via un `StepRegistry` fourni par l'application),
`WorkflowDefinition` (DAG persisté en base, éditable par un admin via le CRUD générique), client
d'enregistrement/déclenchement, et le kind natif `"rhai"` (vynil-core, feature `rhai`) pour les
automatisations/fallbacks définis par un admin. Specs : `src/workflow/*.sdd` ; architecture et pièges :
`docs/architecture.md`.

Historique : l'option initiale (apalis + apalis-postgres + apalis-workflow) a été mise en standby le
2026-08-23 après une exploration comparative (apalis-workflow, Acts, Hatchet, Temporal, Prefect) —
aucun moteur n'avait de DAG piloté par la donnée nativement et les deux candidats les plus prometteurs
(Acts : stockage tronqué à chaque redémarrage ; Hatchet : binding Rust non officiel cassé sur
`ctx.parent_output()`) étaient inutilisables. Restate a été retenu après le spike du 2026-09-22
(fan-out/fan-in, reprise sur crash sans rejeu des steps journalisés) ; aucune dépendance `apalis`
n'existe dans `Cargo.toml`.

## 7b. Hooks métier CRUD
Point d'extension par entité sur `create` (`rest/core.rs`, donc REST **et** MCP simultanément, et
`MiryadHooks::before_active_model_save` côté GraphQL) — validation, mutation avant écriture. Scope
limité à `Create` : Seaography ne déclenche son hook équivalent que sur un insert, et un hook qui
ne se comporterait pas à l'identique sur les 3 surfaces a été jugé no-go. Couvre en synchrone les
cas simples ("à la création, fais aussi X") sans passer par le moteur de workflow (7), qui reste
l'outil des DAG multi-étapes avec reprise sur crash.

Implémentée le 2026-08-23 — cf. `docs/architecture.md`, section "Hooks métier CRUD".

## 8. Support frontend (IR + service statique)
Recentrée le 2026-08-23 après discussion sur l'articulation front/back : la génération du
frontend lui-même (Vue 3 + shadcn-vue + Tailwind, écrans CRUD, admin) **sort de miryad-core** et
devient le roadmap du template `miryad` (générateur TypeScript) — cohérent avec la frontière déjà
posée ("Hors périmètre de miryad-core", ci-dessous), étendue de la production/déploiement à la
génération frontend elle-même.

Ce qui reste côté miryad-core, strictement backend :
- `resource_ir::<E>()` — fonction pure exposant une représentation intermédiaire par entité (champs
  + types via `EntityTrait::Column`/`ColumnType`, RBAC, `owner_column`, `filter_column`), pour que
  le générateur TypeScript de `miryad` s'y cale. **Séparée d'`openapi.json`** (feature 4b) — ne pas
  faire porter à un contrat public destiné aux consommateurs externes des métadonnées internes de
  scaffolding ; deux publics, deux artefacts.
- Service statique du frontend compilé (routeur générique servant un répertoire d'assets avec
  fallback SPA) — générique, ne connaît rien du contenu réel.

Implémentée le 2026-08-23 — cf. `docs/architecture.md`, section "Support frontend (IR + service
statique)".

## 9. Moteur de workflow — évolutions
Suite de la feature 7 (implémentée). Premier lot, issu des besoins du consommateur `vanyline`
(issues #26 et #27, 2026-10-04) :
- `RhaiStep::with_setup` (#27) : fonctions hôte fournies par l'application aux scripts Rhai —
  `src/workflow/rhai_step.sdd`, cible 0.1.5.
- Contexte de run lisible par tous les kinds (`RunInfo`), puis steps « durables » avec accès au
  contexte Restate (#26) : trait `MiryadDurableStep`, `StepContext` étroit (`sleep`, effets
  journalisés, sous-DAG), kind natif `"subworkflow"` avec garde-fou de profondeur —
  `src/workflow/{step,dispatcher,interpreter,durable,subworkflow}.sdd`. Le pilotage par awakeable
  est reporté tant qu'un besoin réel ne l'impose pas (le consommateur sonde dans un step ordinaire).
Au-delà : toute feature de workflow passe par un arbitrage ici avant de gagner une spec.

## 10. Filtrage et tri étendus — après un premier usage réel
Le filtrage REST/GraphQL/MCP actuel (`filter_column()`) est limité à une seule colonne, égalité
exacte. Pas de tri, pas de filtre multi-critères, pas de recherche texte. Gap réel mais **pas
MVP** : le scope est déjà large, à reprendre une fois le frontend (8) traité et une première
application miryad réelle construite dessus — pour caler le besoin sur un usage concret plutôt que
sur une complétude théorique. Décidé le 2026-08-23.

---

**Hors périmètre de miryad-core** : Dockerfile, chart Helm, doc de déploiement CNPG/Authentik, et
**la génération du frontend elle-même** (composants Vue, générateur TypeScript — décision du
2026-08-23, cf. feature 8). miryad-core est une lib publiée sur crates.io, pas un déployable — le
packaging production et la génération frontend appartiennent à l'application réellement déployée,
donc au template `miryad` (son propre roadmap, à écrire quand son bootstrap reprendra — cf.
`$HOME/projets/kydah/miryad/.claude/MEMORY.md`).

## Statut

Étapes 1 (Fondations), 2a (Auth — OIDC + session cookie), 2b (tokens API + dual-auth), 2c
(comptes de service), 3 (Utilisateurs & Groupes), 4 (API REST générique), 4b (OpenAPI + Swagger
UI), 5 (API GraphQL), 6 (Serveur MCP), 7b (hooks métier CRUD) et 8 (support frontend : IR +
service statique) implémentées — cf. `docs/architecture.md`. Le moteur de workflow (7, sur Restate)
est livré en 0.1.4. Prochaine étape : 9 (évolutions du moteur de workflow, #26/#27), puis 10 (filtrage/tri étendus,
explicitement hors MVP jusqu'à un premier usage réel).
