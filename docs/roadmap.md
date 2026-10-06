# Roadmap — miryad-core

Ce qui reste à arbitrer ou à faire. Ce qui est livré vit dans `docs/architecture.md` (le récit et
les décisions) et dans les `src/**/*.sdd` (le contrat) — pas ici. Une feature n'entre pas en spec
sans passer par cette page.

La numérotation est historique et conservée : des specs y renvoient (« roadmap 2c », « roadmap 7/9 »…).
Les items 1 à 8 sont livrés — fondations, auth (2a OIDC/cookie, 2b tokens API, 2c comptes de service),
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

Reporté : le pilotage par awakeable, tant qu'un besoin réel ne l'impose pas (le consommateur sonde
dans un step ordinaire). Au-delà de ce lot, toute feature de workflow passe par un arbitrage ici
avant de gagner une spec.

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
