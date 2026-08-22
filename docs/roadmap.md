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
l'étape 3 dans la résolution. Point de recherche à ouvrir en atteignant cette étape : comment
Seaography permet d'injecter une politique d'autorisation par champ/entité dans son moteur
dynamique.

## 6. Serveur MCP
Tools CRUD générés par entité (list/get/create/update/delete), sortie markdown (pas de JSON —
lisible par un SLM). Dual-auth réutilisé de l'étape 2. Base : patterns `auth.rs`/`mcp.rs` de
`kydah-mcp-template`.

## 7. Moteur de workflow
Intégration apalis + apalis-postgres + apalis-workflow. Step-type natif "script Rhai" (vynil-core)
pour les automatisations/fallbacks définis par un admin. Modèle de définition de DAG persisté en
base (pas seulement du code Rust statique — un admin doit pouvoir définir un workflow).

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

---

**Hors périmètre de miryad-core** : Dockerfile, chart Helm, doc de déploiement CNPG/Authentik.
miryad-core est une lib publiée sur crates.io, pas un déployable — le packaging production
appartient à l'application réellement déployée, donc au template `miryad` (son propre roadmap,
à écrire quand son bootstrap reprendra — cf. `$HOME/projets/kydah/miryad/.claude/MEMORY.md`).

## Statut

Étapes 1 (Fondations), 2a (Auth — OIDC + session cookie), 2b (tokens API + dual-auth), 2c
(comptes de service), 3 (Utilisateurs & Groupes) et 4 (API REST générique) implémentées le
2026-08-22 — cf. `docs/architecture.md`. Étape 4b (OpenAPI + Swagger UI) en cours d'implémentation,
puis étape 5 (API GraphQL) à designer.
