# Roadmap — miryad-core

Ce qui reste à arbitrer ou à faire. Ce qui est livré vit dans `docs/architecture.md` (le récit et
les décisions) et dans les `src/**/*.sdd` (le contrat) — pas ici. Une feature n'entre pas en spec
sans passer par cette page.

La numérotation est historique et conservée : des specs y renvoient (« roadmap 2c », « roadmap 7/9 »…).
Les items 1 à 8 et 11 sont livrés — fondations, auth (2a OIDC/cookie, 2b tokens API, 2c comptes de service),
utilisateurs/groupes, REST, OpenAPI (4b), GraphQL, MCP, moteur de workflow Restate (7), hooks CRUD
(7b), support frontend (8).

## 9. Moteur de workflow — évolutions
Premier lot issu des besoins du consommateur `vanyline` (issues #26 et #27, 2026-10-04) :
- `RhaiStep::with_setup` (#27) : fonctions hôte fournies par l'application aux scripts Rhai —
  `src/workflow/rhai_step.sdd`, cible 0.1.5.
- Contexte de run lisible par tous les kinds (`RunInfo`), puis steps « durables » avec accès au
  contexte Restate (#26) : trait `MiryadDurableStep`, `StepContext` étroit (`sleep`, effets
  journalisés, sous-DAG), kind natif `"subworkflow"` avec garde-fou de profondeur —
  `src/workflow/{step,dispatcher,interpreter,durable,subworkflow}.sdd`.

Second lot (arbitré le 2026-10-06) — **templating Handlebars central** : livré le 2026-10-13, voir
`docs/architecture.md` (« Templating central »). Reste ouvert : retirer la liste noire de helpers quand
`sebt3/vynil-core#13` sera livrée (`src/workflow/template.sdd`).

Reporté : le pilotage par awakeable, tant qu'un besoin réel ne l'impose pas (le consommateur sonde
dans un step ordinaire). Au-delà de ce lot, toute feature de workflow passe par un arbitrage ici
avant de gagner une spec.

## 11. Client System One — livré le 2026-10-13
Voir `docs/architecture.md` (« Client System One »). Rien de ce lot n'est resté ouvert.

## 12. Client Matrix pour bots (feature `matrix`, hors défaut) — arbitré le 2026-10-06
Primitives pour bâtir des chatbots Matrix : connexion (OAuth device code contre MAS, ou import de session),
envoi (identifiant de transaction idempotent), jonction, lecture d'historique et flux de messages
(`MatrixEventHandler`), chiffrement de bout en bout sur `matrix-sdk` 0.18 avec un **store de chiffrement dans
la base SeaORM de l'application** (module `matrix::store`, conçu pour être extrait en crate dédiée) et un
kind de workflow `matrix_send` (sous `workflow` + `matrix`) — `src/matrix/**/*.sdd`,
`src/workflow/matrix_step.sdd`. Contexte : piloter l'application fille `vanyline` depuis un client Matrix,
et servir de base à un nouveau `kydah-monitoring-agents` bâti sur miryad-core.
Livré et validé en réel le 2026-10-18 (login device code, auto-join, écho, voir `docs/architecture.md`, « Client
Matrix de bot »). Reste ouvert : store éprouvé sur **PostgreSQL** (aujourd'hui SQLite mémoire seulement),
persistance des rotations de session contre un MAS réel, cross-signing du bot, provisionnement automatisé du compte
et de la session de bot (`kydah/box#454`), envoi chiffré depuis un réplica qui ne synchronise pas, rattrapage des
messages reçus pendant une coupure (`matrix/mod.sdd`), licence du code porté depuis `matrix-sdk-sqlite`
(`crypto_sessions.sdd`).
Hors périmètre, **à venir** : le harnais de chatbot (outillage standardisé issu de vanyline), dont la remontée de
code et de spec n'est pas prête — il consommera `MatrixEventHandler` ; l'identité Matrix → utilisateur miryad
(RBAC) y sera tranchée.

## 10. Filtrage et tri étendus — après un premier usage réel
Le filtrage REST/GraphQL/MCP (`filter_column()`) est limité à une seule colonne, égalité exacte.
Pas de tri, pas de filtre multi-critères, pas de recherche texte. Gap réel mais **pas MVP** : à
reprendre une fois une première application miryad réelle construite dessus, pour caler le besoin
sur un usage concret plutôt que sur une complétude théorique. Décidé le 2026-08-23.

## À planifier, non MVP
**Fragment OpenAPI des routes de compte** (arbitré 2026-09-29) : `GET /api/v1/me`,
`GET /api/v1/users`, `/api/v1/tokens` sont absentes du document OpenAPI ; le template `miryad` en
aura besoin pour son client. Ces routes restent REST-only par nature (exemption de parité,
`.specdd/bootstrap.project.md`) ; `/auth/*` reste hors document.

## Hors périmètre
Dockerfile, chart Helm, doc de déploiement CNPG/Authentik, et la génération du frontend elle-même
(composants Vue, générateur TypeScript) : miryad-core est une lib publiée sur crates.io, pas un
déployable. Tout cela appartient au template `miryad` et à son propre roadmap.
