# Solution Design: Hooks de ressource asynchrones

## Objective

Donner aux hooks CRUD de `MiryadResource` une connexion à la base et la capacité d'attendre, pour que le contrôle d'autorité
et les effets de bord d'une ressource se décident **à la sauvegarde**, par REST comme par MCP.

## Scope and Non-goals

- **Ouverte le 2026-11-03**, rien n'est implémenté ni spécifié. Hors lot 2 de `miryad-core/lymaille`.
- Concerne le trait `MiryadResource` et la séquence d'appel des hooks de `rest::core` et du handler MCP. Les politiques d'accès
  déclaratives (`AccessPolicy`) ne changent pas.
- **Non-goal :** un hook « après » ou un hook de lecture (`miryad-core/resource-model#FR-6`) ; une file de tâches ou un bus d'événements.

## Applicable Domain Requirements

`miryad-core#FR-1` (parité REST/MCP : un hook s'applique aux deux par la même fonction), `miryad-core#CON-3`, `miryad-core#CON-7`.
`miryad-core/resource-model#FR-5` à `FR-7` et `CON-1` (le contrat des hooks actuels) sont amendés par cette Solution, pas dupliqués.

## Current State and Target State

- **Actuel :** les hooks `before_create`, `before_update` et `before_delete` sont synchrones, sans connexion ni principal à jour
  (Observed in: `src/resource.rs`, 2026-11-03). Ils ne peuvent ni relire les groupes d'un utilisateur ni savoir s'il est admin ; les
  contrôles d'autorité de `lymaille` (`check_grants`, droit d'usage des serveurs MCP) ne se font donc qu'à l'assemblage, à l'appel et
  dans les stores (`miryad-core/lymaille#DEC-20`, `OI-10`).
- **Cible :** hooks asynchrones qui reçoivent une connexion, appelés dans la même séquence que les hooks actuels.

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

## SpecDD Realization

Aucune. `SpecDD /src/resource.sdd` et ses consommateurs seront amendés quand les Open Issues ci-dessous seront tranchées.

## Acceptance Criteria

- **AC-1:** Un hook qui lit la base et refuse produit le même refus en REST et en MCP, avant l'écriture ; un hook qui accepte laisse la séquence inchangée.
- **AC-2:** Une définition d'agent privée qui accorde un module réservé à l'admin est refusée par REST et MCP à la sauvegarde, pas seulement à l'usage.

## Open Issues

- **OI-1:** **Forme du trait** : `async fn` natif dans le trait (bornes `Send` à porter explicitement) ou `async_trait` ; signature exacte (connexion
  générique `ConnectionTrait` pour rester appelable depuis une migration, `miryad-core#CON-7`, ou `DatabaseConnection` seule). **TBD.**
- **OI-2:** **Transaction** : le hook s'exécute-t-il dans la transaction de l'écriture (un échec annule tout) ou avant elle ? Les effets externes
  (une CR Kubernetes) ne sont pas transactionnels : ordre et compensation. **TBD.**
- **OI-3:** **Migration des hooks existants** : les hooks synchrones sont-ils conservés (un trait de compatibilité) ou tous réécrits d'un coup ? **TBD.**
- **OI-4:** **Parité** : le contrôle d'un hook doit-il aussi valoir pour le module `resources` de `lymaille` (écritures, `miryad-core/lymaille#DEC-21`) ?
  Probablement oui par `rest::core`, à confirmer. **TBD.**
