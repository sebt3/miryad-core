# Solution Design: Serveur MCP

## Objective

Exposer les mêmes entités que REST à un agent, sous forme de tools MCP, sans réécrire aucune règle d'accès, et laisser
l'application hôte y ajouter ses propres outils (actions qui ne sont pas du CRUD d'entité).

## Scope and Non-goals

Feature Cargo `mcp` (implique `db`). Un unique `POST /mcp` en JSON-RPC 2.0 (`initialize`, `tools/list`, `tools/call`).
Hors Solution : reconfiguration du format par appel, resources ou prompts MCP. Un outil applicatif n'est jamais une
entité : il n'a ni route REST ni IR frontend, la crate ne lui fournit que le transport, l'accès et la validation des
arguments (`FR-6` à `FR-9`).

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

- **FR-6 - MUST:** L'hôte enregistre un **outil applicatif** (`McpToolRegistry::register_tool`) défini par un nom, une
  description, un schéma JSON de ses arguments, une politique d'accès et un gestionnaire asynchrone. Le gestionnaire reçoit
  la connexion à la base, le principal de l'appelant et les arguments, et rend une valeur JSON ou une `McpError`.
    - **Source:** issue #86 (`sebt3/miryad-core`), « Proposition » 1 et 2 ; user instruction, 2026-11-16
    - **Rationale:** un hôte (monitoring-bot) doit offrir à un agent des actions dont le résultat ne peut pas être
      fabriqué par l'appelant (analyse sur le contexte d'une occurrence de problème, fait dont la preuve est produite par
      le serveur). Le contournement par deux entités (« demande » écrite par l'agent, « résultat » écrit par l'hôte) coûte
      deux appels et une boucle d'interrogation, parce que les hooks de ressource ne connaissent pas l'identité de
      l'appelant.
    - **Realized by:** SpecDD /src/mcp/app_tool.sdd, SpecDD /src/mcp/registry.sdd, SpecDD /src/mcp/handler.sdd
    - **Verified by:** `AC-4`

- **FR-7 - MUST:** La politique d'accès d'un outil applicatif est évaluée par la crate **avant** l'appel du gestionnaire,
  avec la même sémantique que pour une entité (`Public`, `Group`, `AdminOnly` ; l'admin passe toujours). Un refus rend
  `-32001` sans exécuter le gestionnaire.
    - **Source:** issue #86, « Proposition » 2 et 3 ; `access-control` (`rbac::static_verdict`)
    - **Realized by:** SpecDD /src/mcp/app_tool.sdd, SpecDD /src/mcp/handler.sdd
    - **Verified by:** `AC-4`

- **FR-8 - MUST:** `tools/list` expose les outils applicatifs à la suite de ceux des entités, dans l'ordre croissant de leur
  nom ; `tools/call` les dispatche. Les codes JSON-RPC restent ceux de `FR-3` (`-32001` refusé, `-32602` paramètres
  invalides, `-32603` interne).
    - **Source:** issue #86, « Proposition » 3
    - **Realized by:** SpecDD /src/mcp/handler.sdd
    - **Verified by:** `AC-4`

- **FR-9 - MUST:** Des arguments qui ne respectent pas le schéma déclaré rendent `-32602` sans exécuter le gestionnaire.
    - **Source:** issue #86, « Critère d'acceptation »
    - **Realized by:** SpecDD /src/mcp/app_tool.sdd (validation par `jsonschema`, `DEC-3`)
    - **Verified by:** `AC-5`

- **FR-10 - MUST:** Un nom d'outil applicatif qui entre en collision avec un autre outil applicatif ou avec un outil
  d'entité (`{resource_name}_{list|get|create|update|delete}`, **quel que soit l'ordre d'enregistrement**) fait échouer
  l'enregistrement au démarrage, comme `register::<E>`.
    - **Source:** issue #86, « Proposition » 4 ; `SpecDD /src/mcp/registry.sdd`, `Must` (collision par `assert!`)
    - **Rationale:** `tools/call` découpe un nom en `{entité}_{opération}` (suffixe) ; un outil applicatif nommé
      `recipes_list` serait ambigu avec l'entité `recipes`, et l'ordre des deux appels ne doit pas décider du gagnant.
    - **Realized by:** SpecDD /src/mcp/registry.sdd
    - **Verified by:** `AC-6`

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

## Design Decisions

- **DEC-1 - Un outil applicatif est hors parité REST/MCP**
    - **Context:** `miryad-core#FR-1` impose la parité des surfaces pour ce qui touche `MiryadResource`.
    - **Decision:** un outil applicatif n'est pas une entité : il n'a pas d'équivalent REST et n'est pas visé par la
      règle de parité. Exemption nommée, à reporter dans `SpecDD /.specdd/bootstrap.project.md` (« Contrats
      transverses ») par le développeur principal, au même titre que les routes de compte.
    - **Consequences:** la règle d'accès de l'outil vit dans sa politique déclarée (`FR-7`), jamais dans un hook de
      ressource.
    - **Source:** issue #86, « Proposition » 5

- **DEC-2 - `OwnerOnly` est refusée pour un outil applicatif** (confirmée le 2026-11-16, voir `OI-3`)
    - **Context:** `OwnerOnly` compare une colonne propriétaire d'un enregistrement ; `rbac::static_verdict` rend `None`
      pour elle, faute d'enregistrement. Un outil applicatif n'a pas d'enregistrement à l'évaluation.
    - **Decision:** l'enregistrement d'un outil avec `AccessPolicy::OwnerOnly` échoue au démarrage (même mécanisme que
      `FR-10`), plutôt que d'ouvrir ou de fermer l'accès en silence.
    - **Source:** `SpecDD /src/rbac.rs`, `static_verdict` (observé) ; recommandation, 2026-11-16

- **DEC-3 - La crate valide les arguments d'un outil applicatif contre son schéma**
    - **Context:** la crate ne valide aucun schéma aujourd'hui ; les `inputSchema` des tools d'entité sont déclaratifs.
    - **Decision:** `jsonschema` devient dépendance de la feature `mcp` ; un argument non conforme rend `-32602` avant
      le gestionnaire (`FR-9`). Un schéma qui n'est pas lui-même un schéma valide fait échouer l'enregistrement au démarrage.
    - **Pitfall:** voir `OI-1` (features unifiées) : un récupérateur de `$ref` refusant tout est posé à la construction du
      validateur.
    - **Consequences:** `jsonschema` entre dans le graphe de `--features mcp` (jusqu'ici seulement sous `lymaille`) ; la
      batterie de `tooling.sdd` gagne un contrôle `cargo tree` correspondant, à définir avec la spec.
    - **Source:** user instruction, 2026-11-16 (arbitrage de `OI-1`)

- **DEC-4 - Le gestionnaire rend `Result<Value, McpError>`, `McpError` devient `#[non_exhaustive]`**
    - **Decision:** aucun type d'erreur nouveau ; l'hôte exprime un refus métier par `McpError::Application(HookError)`
      (hors taxonomie `MRD-*`). `McpError` est marqué `#[non_exhaustive]` à cette occasion : toute variante future ne casse
      plus les `match` des hôtes. Cassure assumée (API instable avant `1.0`).
    - **Source:** user instruction, 2026-11-16 (arbitrage de `OI-4`)

- **DEC-5 - Une colonne filtrable d'un type hors {entier, booléen, texte} est refusée à l'enregistrement**
    - **Context:** `GET` de liste REST et `_list` MCP déclarent un paramètre par colonne filtrable ; seuls les entiers, les
      booléens et le texte ont une forme de paramètre. Une colonne d'un autre type serait silencieusement non filtrable
      (omise du schéma sans erreur).
    - **Decision:** `McpToolRegistry::register::<E>` échoue par `assert!`, en citant la colonne et le nom de l'entité, comme le
      fait `resource_router` côté REST (`rest-api`, même règle) : la colonne n'est donc jamais annoncée.
    - **Consequences:** erreur de programmation détectée au démarrage, aucun nouveau code `MRD-*` ; les deux surfaces refusent
      le même domaine, conformément à la règle de parité.
    - **Source:** user instruction, 2026-11-16 (arbitrage #105, `SpecDD /src/mcp/registry.sdd`)

## SpecDD Realization

Les specs de `src/mcp/`.

## Acceptance Criteria

**ASSUMPTION :** correspondance exacte avec les `Scenario` des specs non vérifiée dans la migration DSD.

- **AC-1:** Un tool `*_update` d'un non-propriétaire est refusé comme en REST, avec les mêmes hooks.
- **AC-2:** Un tool inconnu rend `-32601`, un refus RBAC `-32001`, un enregistrement absent `-32002`.
- **AC-3:** Le même `get` rend un JSON ou un YAML selon le format choisi au montage, sans changer la sémantique.
- **AC-4:** Un outil applicatif enregistré apparaît dans `tools/list` ; un appel par un principal que sa politique refuse
  rend `-32001` et le gestionnaire n'est pas exécuté.
- **AC-5:** Des arguments qui ne respectent pas le schéma rendent `-32602` et le gestionnaire n'est pas exécuté.
- **AC-6:** Un nom en collision (avec un outil applicatif ou un outil d'entité, dans les deux ordres d'enregistrement)
  fait échouer l'enregistrement au démarrage.

## Open Issues

Arbitrages de l'issue #86, tous soldés le 2026-11-16.

- **OI-1:** **Soldé le 2026-11-16** par `DEC-3`. Impact mesuré le 2026-11-17 (`cargo tree --no-default-features -e normal --prefix none`, entrées uniques) : `--features mcp` en compte 409 contre 323 sous `--features db` (`mcp` implique `db`), soit 86 de plus, qui viennent de `vynil-core` **et** de `jsonschema` (`jsonschema`, `jsonschema-regex`, `jsonschema-value`, `referencing`, `fancy-regex`, `email_address`, `fluent-uri`, `fraction`, `num-*`, `bit-set`, `ahash`…) : la part de `jsonschema` seule n'est pas isolable par `cargo tree`. Le contrôle de `tooling.sdd` (`cargo tree --features mcp -e normal -i jsonschema` non vide, sans `mcp` stdout vide) est en place et vert.
  Piège constaté (source de `jsonschema` 0.60.2) : `lymaille` tire `jsonschema` avec `resolve-http` et `resolve-file` ; Cargo unifie
  les features, donc `default-features = false` ne suffit pas. Arbitré le 2026-11-16 : le validateur est construit avec un
  récupérateur de `$ref` qui refuse toute résolution (`ValidationOptions::with_retriever`).
- **OI-2:** **Soldé le 2026-11-16.** Le résultat d'un outil applicatif passe par `OutputFormat` en forme `Record` (un seul
  mécanisme de rendu, `FR-4`). Limite acceptée : le gabarit Markdown par défaut suppose un objet. Source : user instruction,
  2026-11-16.
- **OI-3:** **Soldé le 2026-11-16.** `DEC-2` confirmée (`OwnerOnly` refusée à l'enregistrement) ; `tools/list` n'est pas filtré par
  politique, comme pour les tools d'entité. Source : user instruction, 2026-11-16.
- **OI-4:** **Soldé le 2026-11-16** par `DEC-4`.
