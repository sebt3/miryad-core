# Roadmap — miryad-core

Grandes étapes vers un scaffolding utilisable. Tout ce qui suit fait partie du MVP — pas de
relégation en "phase 2" pour ces items (décision explicite : le moteur de workflow est un pilier,
pas un bonus). L'ordre reflète les dépendances techniques, pas une priorité produit.

Chaque ligne devient une ou plusieurs features (`docs/features/<nom>.md`) au moment d'y arriver —
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

## 7. Moteur de workflow — **standby (2026-08-23)**
Intégration apalis + apalis-postgres + apalis-workflow. Step-type natif "script Rhai" (vynil-core,
feature `rhai` — même dépendance débloquée que l'étape 6) pour les automatisations/fallbacks
définis par un admin. Modèle de définition de DAG persisté en base (pas seulement du code Rust
statique — un admin doit pouvoir définir un workflow).

Bloqué après une exploration comparative de plusieurs moteurs (apalis-workflow, Acts, Hatchet,
Temporal, Prefect) : aucun n'a de DAG piloté par la donnée nativement (tous exigent un graphe
déclaré en code SDK), et les deux candidats les plus prometteurs se sont révélés inutilisables en
pratique. `Acts` (embarquable, YAML runtime) tronque sa table de stockage à chaque redémarrage —
bug reproduit (le modèle déployé disparaît dès le redémarrage du process suivant), disqualifiant
pour un usage persistant. `Hatchet` (self-hosted mono-conteneur + Postgres, licence MIT, primitives
LLM/agent réelles) a un moteur serveur qui fonctionne correctement (fan-out, détection de worker
mort), mais son seul binding Rust (`hatchet-sdk`, non officiel) a un bug de désérialisation REST
qui casse `ctx.parent_output()` — le mécanisme central de passage de données entre tâches d'un DAG
— contre toute version de Hatchet actuellement disponible en self-host. Temporal (SDK Rust officiel
mais encore jeune) et Prefect (flows intrinsèquement Python, incompatible avec un écosystème
100% Rust) écartés sur d'autres critères avant d'aller jusqu'au spike. Le fait-maison sur
`apalis-postgres` reste l'option de secours mais représente ~1200-1900 lignes de code
distribué/concurrent à fiabiliser nous-mêmes (fan-in sous course, reprise sur crash) — jugé trop
risqué pour une brique dont la robustesse est justement l'exigence.

À reprendre : soit un correctif amont sur `hatchet-rust-sdk` (bug isolé, pas un défaut de la
plateforme Hatchet elle-même), soit une nouvelle option qui n'existait pas encore lors de cette
exploration.

## 7b. Hooks métier CRUD
Point d'extension par entité sur les 4 opérations d'écriture/lecture génériques (`rest/core.rs`,
donc REST **et** MCP simultanément) — validation, effets de bord, mutation avant écriture. Absorbe
une partie de ce que le moteur de workflow (7, standby) aurait couvert pour les cas simples
("à la création, fais aussi X"), pas les DAG multi-étapes avec reprise sur crash.

Point de départ existant côté GraphQL : `LifecycleHooksInterface` (Seaography, feature 5) expose
déjà `before_active_model_save`, laissé en no-op faute d'usage — la moitié du câblage est déjà là.
Reste à définir l'équivalent pour `rest/core.rs` et le mécanisme déclaratif par entité (nouvelle
méthode sur `MiryadResource`, ou trait compagnon).

## 8. Frontend générique
Vue 3 + shadcn-vue + Tailwind. Écrans CRUD génériques pilotés par la métadonnée d'entité exposée
via REST/GraphQL (liste, détail, formulaire — pas un `.vue` par entité à maintenir). Login OIDC
(pattern `vanyline/frontend`). Espace admin : utilisateurs, groupes, tokens API, visualisation des
DAG de workflow.

## 9. CLI de scaffolding (`miryad` binaire)
Format du modèle de données **à définir** (question ouverte, cf. `MEMORY.md`). Le binaire lit ce
modèle, génère les migrations/entités SeaORM, les implémentations du trait `MiryadResource`, et
instancie/complète un projet depuis le template `miryad`. Dépend de ce que les étapes 1-8 ont
stabilisé comme forme réelle — volontairement en fin de roadmap, pas figé avant d'avoir une
vraie application construite à la main dessus.

Glissée avant le filtrage (10) : probablement le seul moyen de distribution automatisée d'un
frontend généré (8). Périmètre réel dans miryad-core (bibliothèque publiée) vs. `miryad` (le
template applicatif) pas encore tranché — à discuter après l'implémentation de la feature 7b.
Décidé le 2026-08-23.

## 10. Filtrage et tri étendus — après un premier usage réel
Le filtrage REST/GraphQL/MCP actuel (`filter_column()`) est limité à une seule colonne, égalité
exacte. Pas de tri, pas de filtre multi-critères, pas de recherche texte. Gap réel mais **pas
MVP** : le scope est déjà large, à reprendre une fois le frontend (8) et le scaffolding (9) traités
et une première application miryad réelle construite dessus — pour caler le besoin sur un usage
concret plutôt que sur une complétude théorique. Décidé le 2026-08-23.

---

**Hors périmètre de miryad-core** : Dockerfile, chart Helm, doc de déploiement CNPG/Authentik.
miryad-core est une lib publiée sur crates.io, pas un déployable — le packaging production
appartient à l'application réellement déployée, donc au template `miryad` (son propre roadmap,
à écrire quand son bootstrap reprendra — cf. `$HOME/projets/kydah/miryad/.claude/MEMORY.md`).

## Statut

Étapes 1 (Fondations), 2a (Auth — OIDC + session cookie), 2b (tokens API + dual-auth), 2c
(comptes de service), 3 (Utilisateurs & Groupes), 4 (API REST générique), 4b (OpenAPI + Swagger
UI), 5 (API GraphQL) et 6 (Serveur MCP) implémentées — cf. `docs/architecture.md`. Étape 7
(Moteur de workflow) en standby, cf. section dédiée ci-dessus. Étape 7b (hooks métier CRUD)
glissée dans le flow avant le frontend (8), suivie du scaffolding (9, nécessaire pour distribuer un
frontend généré). Étape 10 (filtrage/tri étendus) explicitement hors MVP, reportée après un premier
usage réel — décision du 2026-08-23.
