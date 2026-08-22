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

## 2. Auth
OIDC (porté depuis `vanyline/app/src/auth/oidc.rs`) + session cookie pour le frontend + tokens API
pour les intégrations machine. Middleware dual-auth (JWT OIDC ou token API) réutilisable par REST,
GraphQL et MCP.

## 3. Utilisateurs & Groupes
Modèle `User`/`Group`/`GroupMembership`, bootstrap d'un groupe `admin`, évaluation RBAC (owner-only
/ groupe / admin / public) branchée sur le trait `MiryadResource` de l'étape 1.

## 4. API REST générique
Routeur CRUD générique (axum) construit depuis le trait `MiryadResource` + RBAC de l'étape 3.
Aucune route à écrire par entité.

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

Étape 1 (Fondations) implémentée le 2026-08-22 — cf. `docs/features/01-fondations.md`.
