# Solution Design: Hooks de ressource asynchrones

## Objective

Donner aux hooks CRUD de `MiryadResource` une connexion à la base et la capacité d'attendre, pour que le contrôle d'autorité
et les effets de bord d'une ressource se décident **à la sauvegarde**, par REST comme par MCP.

## Scope and Non-goals

- **Ouverte le 2026-11-03**, **Open Issues soldés le 2026-11-11** (séance de questions) ; rien n'est implémenté ni spécifié (les specs `src/**/*.sdd` suivent). Hors lot 2 de `miryad-core/lymaille`. **Lot 3 de `miryad-core`, avant `miryad-core/scripts-hote`** (arbitrage du
  2026-11-05, user instruction : le changement transversal du trait d'abord ; vanyline peut en attendant passer par un module hôte pour les actions Kubernetes). Les quatre
  Open Issues ont été tranchés en atelier le 2026-11-11 (`DEC-2` à `DEC-5`).
- Concerne le trait `MiryadResource` et la séquence d'appel des hooks de `rest::core` et du handler MCP. Les politiques d'accès
  déclaratives (`AccessPolicy`) ne changent pas.
- **Non-goal :** un hook « après » ou un hook de lecture (`miryad-core/resource-model#FR-6`) ; une file de tâches ou un bus d'événements ; une transaction englobant hook et écriture (`DEC-3`) ;
  la création d'une CR Kubernetes depuis un hook (`DEC-6`).

## Applicable Domain Requirements

`miryad-core#FR-1` (parité REST/MCP : un hook s'applique aux deux par la même fonction), `miryad-core#CON-3`, `miryad-core#CON-7`.
`miryad-core/resource-model#FR-5` à `FR-7` et `CON-1` (le contrat des hooks actuels) sont amendés par cette Solution, pas dupliqués.

## Current State and Target State

- **Actuel :** les hooks `before_create`, `before_update` et `before_delete` sont synchrones, sans connexion ni principal à jour
  (Observed in: `src/resource.rs`, 2026-11-03). Ils ne peuvent ni relire les groupes d'un utilisateur ni savoir s'il est admin ; les
  contrôles d'autorité de `lymaille` (`check_grants`, droit d'usage des serveurs MCP) ne se font donc qu'à l'assemblage, à l'appel et
  dans les stores (`miryad-core/lymaille#DEC-20`, `OI-10`).
- **Cible :** hooks asynchrones qui reçoivent une connexion, appelés dans la même séquence que les hooks actuels.
- **Besoin constaté côté `vanyline` (2026-11-11) :** la limite « hook pur, sans accès BDD » (`docs/architecture.md` de `vanyline`) interdit de valider l'existence d'un nom référencé
  ou l'appartenance d'un `conversation_id` au même propriétaire ; ces validations passent aujourd'hui hors du moule `MiryadResource`. `Project` et `Sandbox` de `vanyline` sont des
  CR Kubernetes qui font foi (`workspaces#BR-2`) ; la base n'en porte qu'une ligne d'identité réconciliée depuis le cluster : leurs handlers custom restent légitimes.

## Functional Requirements

- **FR-1 - MUST:** Un hook de ressource peut attendre et lire la base ; il reçoit une connexion et le principal de l'appel.
    - **Source:** `miryad-core/lymaille#OI-10` ; user instruction, 2026-11-03
    - **Verified by:** `AC-1`

- **FR-2 - MUST:** La séquence ne change pas : décision RBAC, hooks, puis invariants de sécurité (`miryad-core/resource-model#FR-6`). Un refus
  de hook reste un `422` en REST et un `-32000` en MCP.
    - **Source:** `miryad-core/resource-model#FR-6`, `FR-7`
    - **Verified by:** `AC-1`

- **FR-3 - MUST:** Le contrôle de droits d'une définition `lymaille` (`check_grants`) et le droit d'usage d'un serveur MCP lié par un contexte se font à la
  sauvegarde, en plus de l'assemblage et de l'appel.
    - **Source:** `miryad-core/lymaille#FR-9`, `DEC-20` ; user instruction, 2026-11-03
    - **Verified by:** `AC-2`

## Design Decisions

- **DEC-1 - Direction retenue : hooks asynchrones avec connexion** (2026-11-03)
    - **Context:** le module générique des ressources de `lymaille` ne peut créer d'effet asynchrone (par exemple la CR Kubernetes d'une
      ressource de l'application `vanyline`) et le contrôle d'autorité à la sauvegarde est impossible.
    - **Decision:** étendre le trait `MiryadResource`, changement transversal à toutes les ressources, plutôt que de contourner par un module
      d'hôte dédié aux actions externes.
    - **Alternatives:** module d'hôte dédié aux actions Kubernetes, le module générique restant pour les lectures (écarté par le développeur principal ;
      aurait laissé le contrôle d'autorité à la sauvegarde hors de portée).
    - **Consequences:** changement cassant pour les applications consommatrices (acceptable, API instable avant `1.0`) ; amende `src/resource.sdd`,
      `src/rest/core.sdd`, `src/mcp/handler.sdd`, et chaque entité de la crate qui implémente un hook.
    - **Source:** user instruction, 2026-11-03

- **DEC-2 - Forme du trait : `async fn` natif, connexion générique** (2026-11-11)
    - **Context:** un hook doit lire la base ; `rest::core` et le handler MCP l'appellent avec une `DatabaseConnection`, mais `miryad-core#CON-7` exige des fonctions appelables aussi
      depuis une migration (connexion générique).
    - **Decision:** les hooks sont des `async fn` natives du trait, qui reçoivent `&impl ConnectionTrait` en plus du principal ; les bornes `Send` sont portées explicitement.
    - **Alternatives:** `DatabaseConnection` seule (écartée : exclut une transaction ouverte et la migration) ; `async_trait` avec `&dyn ConnectionTrait` (écartée : boxing des futures et dépendance en plus).
    - **Consequences:** signature générique sur chaque hook ; les fixtures de tests passent à une connexion `MockDatabase`.
    - **Source:** user instruction, 2026-11-11 (clôture de `OI-1`)

- **DEC-3 - Hooks de contrôle avant l'écriture, sans transaction** (2026-11-11)
    - **Context:** `rest::core` n'ouvre aucune transaction (arbitrage `rest/core.sdd` du 2026-09-27) ; la course entre relecture et écriture est un `404` légitime.
    - **Decision:** la séquence reste décision RBAC, hook, invariants, écriture simple. Le hook ne s'exécute pas dans une transaction ; un refus interrompt avant toute requête d'écriture.
    - **Alternatives:** transaction englobant hook et écriture (écartée : change toute la séquence REST/MCP, et un effet externe n'est de toute façon pas annulé au rollback) ; hook `after_commit` avec compensation (écarté : hors du besoin, non-goal conservé).
    - **Consequences:** un contrôle qui lit la base peut être dépassé par une écriture concurrente entre le hook et l'écriture ; acceptable pour des contrôles d'autorité et de référence, qui restent revérifiés à l'appel (`miryad-core/lymaille#DEC-20`).
    - **Source:** user instruction, 2026-11-11 (clôture de `OI-2`)

- **DEC-4 - Migration : réécriture totale, pas de trait de compatibilité** (2026-11-11)
    - **Context:** neuf entités `lymaille`, `workflow::definition` et les fixtures de tests implémentent des hooks synchrones ; API instable avant `1.0`, aucune production.
    - **Decision:** les trois hooks deviennent asynchrones d'un seul coup ; aucun hook synchrone n'est conservé.
    - **Alternatives:** trait de compatibilité avec adaptateur (écarté : dette à porter jusqu'à `1.0`).
    - **Consequences:** un lot unique, découpé en tâches par fichier (une entité par tâche) ; changement cassant pour les applications consommatrices, à noter au `CHANGELOG`.
    - **Source:** user instruction, 2026-11-11 (clôture de `OI-3`)

- **DEC-5 - Parité : le hook vaut pour `lymaille` par construction** (2026-11-11)
    - **Context:** les écritures du module `resources` de `lymaille` délèguent à `rest::core::create` / `update` / `delete` (`src/lymaille/resource_module.rs`, `miryad-core/lymaille#DEC-21`).
    - **Decision:** aucun travail propre à `lymaille` : le hook s'applique par `rest::core`, comme en REST et en MCP.
    - **Source:** vérifié dans le source le 2026-11-11 ; user instruction (clôture de `OI-4`)

- **DEC-6 - Les effets externes restent hors des hooks** (2026-11-11)
    - **Context:** la création d'une CR Kubernetes à la sauvegarde (besoin B6 de `vanyline`) a été envisagée comme motif de `DEC-1`.
    - **Decision:** les hooks ne portent que contrôle et mutation de l'`ActiveModel`. Une CR Kubernetes n'est pas créée par un hook : la CR fait foi et la base se réconcilie sur elle, un hook en base d'abord inverserait le sens. Les handlers custom de l'application (ou un module d'hôte, `miryad-core/scripts-hote`) restent le lieu des effets externes.
    - **Alternatives:** lever le non-goal « hook après » dans ce lot (écarté : pas de besoin avéré).
    - **Source:** user instruction, 2026-11-11, après lecture des DSD `workspaces` de `vanyline`

## SpecDD Realization

Aucune. `SpecDD /src/resource.sdd` et ses consommateurs seront amendés quand les Open Issues ci-dessous seront tranchées.

## Acceptance Criteria

- **AC-1:** Un hook qui lit la base et refuse produit le même refus en REST et en MCP, avant l'écriture ; un hook qui accepte laisse la séquence inchangée.
- **AC-2:** Une définition d'agent privée qui accorde un module réservé à l'admin est refusée par REST et MCP à la sauvegarde, pas seulement à l'usage.

## Open Issues

- **OI-1:** **Soldé le 2026-11-11 (user instruction)** : `async fn` natif, connexion générique `&impl ConnectionTrait` (`DEC-2`).
- **OI-2:** **Soldé le 2026-11-11 (user instruction)** : hook avant l'écriture, sans transaction ; effets externes hors des hooks (`DEC-3`, `DEC-6`).
- **OI-3:** **Soldé le 2026-11-11 (user instruction)** : réécriture totale d'un coup (`DEC-4`).
- **OI-4:** **Soldé le 2026-11-11** : parité acquise par `rest::core`, vérifiée dans le source (`DEC-5`).
