# Mémoire du projet

Ce fichier est maintenu par Claude au fil des sessions.
Les développeurs peuvent le lire, le corriger ou le compléter à tout moment.

---

## Contexte

miryad-core est né d'une session de bootstrap sur le projet **miryad** (template d'application
Rust/Vue "à la Debian", monorepo léger dans `$HOME/projets/kydah/miryad`, Gitea privé). En
discutant faisabilité, il est apparu que la mécanique (auth, RBAC, REST/GraphQL/MCP génériques,
workflow) devait vivre dans une crate séparée, publique, réutilisable — sur le modèle
`vynil`/`vynil-core`. miryad-core est cette crate : `$HOME/projets/miryad-core`, publique
(GitHub `sebt3/miryad-core` + crates.io), pattern nommage/emplacement calé sur `kuberest`/
`vanyline` (top-level sous `$HOME/projets/`, pas nested — contrairement à `vynil-core` qui est un
artefact historique d'extraction).

## Décisions d'architecture (2026-08-22)

- **Découpage par couche**, pas par mécanisme unique :
  - Composants UI (formulaires, tables) : scaffoldés dans le code consommateur via shadcn-vue —
    présentation pure, attendu que le développeur les retouche
  - Plomberie données (REST/GraphQL/MCP/RBAC/câblage front) : moteur générique dans miryad-core,
    piloté par un trait `MiryadResource` par entité SeaORM — jamais de boilerplate par entité
  - Raison : le développeur ne veut pas d'un générateur one-shot qui dérive du modèle avec le
    temps, ni d'une maintenance lourde côté applications produites ; il veut une forte intégration
    à des frameworks externes éprouvés plutôt que réinventer.
- **GraphQL : Seaography 2.0**, pas de resolvers écrits à la main. Seaography 2.0 génère un schéma
  GraphQL **dynamique à l'exécution** depuis les entités SeaORM (plus de régénération de code à
  chaque évolution du modèle depuis la 2.0) — validé par recherche web du 2026-08-22.
- **Workflow : apalis + apalis-postgres + apalis-workflow**, choisi explicitement pour rester dans
  le MVP (pas relégué en phase 2 — le développeur a été clair : "aucune application moderne
  n'arrive sans possibilité d'automatisation"). Raison technique du choix : support DAG natif,
  backend Postgres en pur client Rust (pas d'extension à installer côté cluster, contrairement à
  `pg_durable` de Microsoft) — compatible avec la contrainte "CNPG + authentik, rien d'autre à
  déployer".
- **Rhai (vynil-core) comme step-type natif du moteur de workflow** — sert le cas d'usage
  "fallback d'extraction défini par un admin en script" identifié dans l'exemple d'application
  cible (gestionnaire de recettes).
- miryad-core est **opinionated** (axum/SeaORM/Seaography/apalis imposés), contrairement à
  vynil-core qui reste générique sans framework imposé — assumé et documenté dans `AGENTS.md`
  pour éviter la confusion entre les deux philosophies.
- Licence : BSD 3-Clause, tranché le 2026-08-22 à la clôture du bootstrap — cohérence avec
  vynil/vynil-core. Fichier `LICENSE` en place.

## Frontière avec miryad (2026-08-22)

miryad-core = bibliothèque (crates.io), jamais de Dockerfile/Helm/déploiement ici. Tout ce qui
touche au build/déploiement de l'application reste dans `miryad` — corrigé après une première
version de la roadmap qui plaçait à tort un item "packaging production" ici.

## Scope MVP

Dans le MVP (pas de phase 2 pour aucun de ces points) : auth OIDC + tokens API, users/groupes/RBAC,
REST générique, GraphQL (Seaography), MCP (tools CRUD + sortie markdown), moteur de workflow
(apalis + step Rhai), frontend Vue générique (shadcn-vue) avec espace admin, CLI de scaffolding.

Hors-scope explicite pour l'instant : RBAC au niveau colonne (masquage de champs par rôle — au-delà
de lecture/écriture par entité), ownership multi-colonnes, UI de conception visuelle du DAG
(l'affichage du workflow dans l'UI suffit au MVP, pas forcément l'édition visuelle).

## Format du modèle de données (entrée du CLI de scaffolding)

**Pas encore défini.** C'est une question ouverte identifiée mais non résolue lors du bootstrap —
à trancher avant/pendant la feature qui touche au binaire `miryad`.

## Rôles

- Développeur principal : conception, décisions techniques, validation
- Claude : architecture, review
- Cadence (deepseek) : cadence l'implémentation d'une feature déjà designée, dispatche à `implement`
- Implement (Qwen3.6:35b-a3b, opencode) : implémentation guidée

## Mode de travail temporaire — Cadence/Implement indisponibles (2026-08-22)

Le workflow décrit dans `config.md` (Claude conçoit, Qwen/`implement` code, Cadence dispatche)
suppose ces deux agents disponibles. Ils ne le sont pas pour l'instant : les sessions en cours se
font en binôme direct développeur + Claude, Claude implémentant lui-même (fast-track), sans passer
par `.tasks/` ni par une copie de fichiers de référence — Claude lit directement les autres dépôts
locaux (`vanyline`, `vynil-core`, `kydah-mcp-template`, tous clonés sous `$HOME/projets/`) au
moment d'en porter un pattern, pas besoin de les dupliquer à l'avance. Le workflow `config.md`
reprendra tel quel dès que Cadence/Implement seront de nouveau disponibles.

## Clôture du bootstrap (2026-08-22)

Repo GitHub `sebt3/miryad-core` créé (public), commit initial poussé sur `origin/main`. Licence
BSD-3-Clause tranchée à cette occasion (cf. section Décisions d'architecture). `.claude/
bootstrap.md` et `.claude/retrofit.md` (fichier mort, jamais référencé par `CLAUDE.md`) supprimés.
Le projet démarre en mode feature normal — prochaine étape : valider le design de la feature
Fondations (`docs/features/01-fondations.md`) avant dispatch.
