# Solution Design: Serveur MCP

## Objective

Exposer les mêmes entités que REST à un agent, sous forme de tools MCP, sans réécrire aucune règle d'accès.

## Scope and Non-goals

Feature Cargo `mcp` (implique `db`). Un unique `POST /mcp` en JSON-RPC 2.0 (`initialize`, `tools/list`, `tools/call`).
Hors Solution : reconfiguration du format par appel, resources ou prompts MCP.

## Applicable Domain Requirements

`miryad-core#FR-1` (parité REST/MCP), `miryad-core#CON-3` (codes `MRD-MCP-NNN`).

## Functional Requirements

- **FR-1 - MUST:** Pour chaque entité enregistrée, cinq tools `{resource_name}_list`, `_get`, `_create`, `_update`,
  `_delete` sont exposés, dispatchés par nom d'entité (un `tools/call` arrive avec un nom en chaîne, pas un type).
    - **Source:** `arch@c62a184`, « Serveur MCP »
    - **Realized by:** SpecDD /src/mcp/registry.sdd, SpecDD /src/mcp/mod.sdd
    - **Verified by:** `AC-1`

- **FR-2 - MUST:** Chaque tool appelle directement `rest::core::{list,get,create,update,delete}::<E>` (dual-auth, RBAC,
  pagination, hooks inclus) puis rend le résultat via `OutputFormat`. Aucune règle n'est réécrite pour MCP.
    - **Source:** `arch@c62a184`, « Serveur MCP »
    - **Realized by:** SpecDD /src/mcp/registry.sdd
    - **Verified by:** `AC-1`

- **FR-3 - MUST:** Les codes d'erreur JSON-RPC sont `-32001` (refusé), `-32002` (non trouvé), `-32601` (tool inconnu),
  `-32602` (paramètres invalides) et `-32603` (erreur interne ou base). `-32001` et `-32002` sont des choix de la crate
  dans la plage libre `-32000`…`-32099`.
    - **Source:** `arch@c62a184`, « Serveur MCP »
    - **Realized by:** SpecDD /src/mcp/mod.sdd, SpecDD /src/mcp/error.sdd
    - **Verified by:** `AC-2`

- **FR-4 - MUST:** Le format de sortie est fixé une fois par l'app au montage du serveur, et non par appel. Les formats
  `Json`, `Yaml`, `Markdown` et `Custom(template)` sont tous des templates Handlebars passant par le même mécanisme de
  rendu.
    - **Source:** `arch@c62a184`, « Serveur MCP » (décision : un seul mécanisme de rendu)
    - **Realized by:** SpecDD /src/mcp/format.sdd
    - **Verified by:** `AC-3`

- **FR-5 - MUST:** Un enregistrement seul (`get`, `create`, `update`) et une page de résultats (`list`, forme
  `PagedResult`) ont chacun leur gabarit par défaut interne, sans complexité exposée à l'app.
    - **Source:** `arch@c62a184`, « Serveur MCP »
    - **Realized by:** SpecDD /src/mcp/format.sdd
    - **Verified by:** `AC-3`

## Constraints

- **CON-1 - MUST:** Pour `update`, `registry.rs` désérialise `arguments` **deux fois** : une pour `id` seul, une pour
  `E::Model` entier. La valeur de `id` dans le corps est écrasée par `core::update` (la clé primaire vient de
  l'argument, comme du chemin en REST).
    - **Source:** `arch@c62a184`, « Serveur MCP », point d'attention
    - **Rationale:** `tools/call` transporte `id` et le corps dans le même objet ; un `#[serde(flatten)]` consommerait
      la clé `id` avant que `E::Model`, qui la requiert, ne la voie, et la désérialisation échouerait toujours.
    - **Realized by:** SpecDD /src/mcp/registry.sdd

- **CON-2 - MUST:** `vynil-core` est dépendance de la feature `mcp` avec `default-features = false` et la feature `hbs`
  seule (sans `crypto`).
    - **Source:** `arch@c62a184`, « Serveur MCP »
    - **Realized by:** SpecDD /src/mcp/mod.sdd

## SpecDD Realization

Les specs de `src/mcp/`.

## Acceptance Criteria

**ASSUMPTION :** correspondance exacte avec les `Scenario` des specs non vérifiée dans la migration DSD.

- **AC-1:** Un tool `*_update` d'un non-propriétaire est refusé comme en REST, avec les mêmes hooks.
- **AC-2:** Un tool inconnu rend `-32601`, un refus RBAC `-32001`, un enregistrement absent `-32002`.
- **AC-3:** Le même `get` rend un JSON ou un YAML selon le format choisi au montage, sans changer la sémantique.

