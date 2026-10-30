# Solution Design: Client System One

## Objective

Appeler `POST /v1/systemone` (modèles de décision : questions typées `noul`, `choice`, `score`, réponses en probabilités
calibrées) exposé par `kydah-ai-proxy` (issue `kydah/kydah-ai-proxy#111`), et le brancher comme kind de workflow.

## Scope and Non-goals

Feature **hors défaut**, **sans dépendance nouvelle** (`reqwest`, `serde`, `thiserror` sont déjà directes). Aucune
bibliothèque Rust n'existe pour cette API et c'est un simple `POST` : la crate le porte donc elle-même.

## Applicable Domain Requirements

`miryad-core#CON-3` (codes `MRD-SYSTEMONE-NNN`), `miryad-core#CON-4`.

## Functional Requirements

- **FR-1 - MUST:** `SystemOneClient` est sans état hors son pool HTTP et ne dépend ni de `workflow`, ni d'axum, ni de la
  base. Questions et réponses sont des `serde_json::Value` opaques : un nouveau type de question de l'API n'impose
  aucun changement de la crate.
    - **Source:** `arch@c62a184`, « Client System One »
    - **Realized by:** SpecDD /src/systemone/mod.sdd
    - **Verified by:** `AC-1`

- **FR-2 - MUST:** La clé Bearer est posée en en-tête par défaut marqué sensible, les redirections sont désactivées (la
  clé ne part jamais vers un autre hôte), le `Debug` de la configuration masque la clé et le corps d'une erreur est
  tronqué à 512 caractères.
    - **Source:** `arch@c62a184`, « Client System One »
    - **Realized by:** SpecDD /src/systemone/mod.sdd
    - **Verified by:** `AC-2`

- **FR-3 - MUST:** Le client ne fait aucune nouvelle tentative interne : la politique de retry appartient à l'appelant.
  `retryable()` est vrai pour un délai ou une connexion, `429`, `529` et `5xx`, faux pour le reste. Les erreurs sont
  `MRD-SYSTEMONE-001` (configuration), `-002` (transport), `-003` (statut rejeté), `-004` (réponse invalide).
    - **Source:** `arch@c62a184`, « Client System One »
    - **Realized by:** SpecDD /src/systemone/mod.sdd
    - **Verified by:** `AC-3`

- **FR-4 - MUST:** Le kind de workflow `systemone` (sous `workflow` **et** `systemone`) est enregistré par l'application
  (`registry.register(SystemOneStep::new(client))`). Il pose les questions puis évalue des gates (seuils inclusifs sur la
  probabilité d'une question `noul`, d'une option de `choice` ou d'un `score`).
    - **Source:** `arch@c62a184`, « Client System One »
    - **Realized by:** SpecDD /src/workflow/systemone_step.sdd
    - **Verified by:** `AC-4`

- **FR-5 - MUST:** Un gate non franchi arrête le workflow par défaut (échec non retryable) ; avec `on_fail: "continue"`
  le step rend `passed: false` et laisse l'aval décider. La configuration des gates est validée avant tout appel réseau,
  et un gate refuse les clés inconnues.
    - **Source:** `arch@c62a184`, « Client System One »
    - **Realized by:** SpecDD /src/workflow/systemone_step.sdd
    - **Verified by:** `AC-4`

## Design Decisions

- **DEC-1 - Un gate non franchi n'est pas une panne**
    - **Context:** une probabilité calibrée sous le seuil est un résultat, pas une erreur transitoire.
    - **Decision:** échec non retryable par défaut.
    - **Source:** `arch@c62a184`, « Client System One »

- **DEC-2 - Un gate refuse les clés inconnues**
    - **Context:** une borne mal orthographiée ne doit pas disparaître en silence.
    - **Decision:** validation stricte avant tout appel réseau.
    - **Source:** `arch@c62a184`, « Client System One »

## SpecDD Realization

`SpecDD /src/systemone/mod.sdd` et ses enfants ; `SpecDD /src/workflow/systemone_step.sdd`.

## Acceptance Criteria

**ASSUMPTION :** correspondance exacte avec les `Scenario` des specs non vérifiée dans la migration DSD.

- **AC-1:** Le client compile sans la feature `workflow`.
- **AC-2:** La clé n'apparaît ni dans `Debug`, ni dans une erreur, ni dans une redirection suivie.
- **AC-3:** Un `429` est rejouable, un `400` ne l'est pas.
- **AC-4:** Un gate sous le seuil échoue sans retry par défaut ; avec `on_fail: "continue"`, `passed` vaut `false`.

