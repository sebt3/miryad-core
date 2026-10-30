# Solution Design: Moteur de workflow

## Objective

Permettre à une application consommatrice de stocker des workflows comme **données** (un DAG de steps), de les faire
exécuter durablement par un cluster Restate self-hosté, et d'y brancher ses propres kinds de steps.

## Scope and Non-goals

La Solution fournit deux services Restate (`DagInterpreter`, `StepDispatcher`), la définition et la validation du DAG,
un client HTTP vers Restate, le templating des `config`, et les kinds natifs `rhai` et `subworkflow`.

Hors Solution :

- Elle ne monte **aucune** route sur le `axum::Router` : l'app construit et lie son `Endpoint` dans son `main()`.
- Elle ne gère jamais le cluster Restate (CON-1 de `miryad-core`).
- Elle ne duplique jamais le statut d'un run en base : il se lit directement dans Restate.
- Les kinds `matrix_send` et `systemone` relèvent de leurs propres Solutions (TBD, non migrées) ; seul leur mécanisme
  d'enregistrement est ici.

## Applicable Domain Requirements

S'appliquent `miryad-core#CON-1` (bibliothèque, pas déployable), `miryad-core#CON-3` (codes `MRD-WORKFLOW-NNN`) et
`miryad-core#CON-4` (feature `workflow`, qui implique `db`).

## Functional Requirements

- **FR-1 - MUST:** Un DAG est une donnée : il est stocké dans `miryad_workflow_definitions` et édité par le CRUD
  générique (`WorkflowDefinition` est une `MiryadResource`), sous une politique d'accès `AdminOnly` configurable.
    - **Source:** `arch@c62a184`, « Moteur de workflow »
    - **Realized by:** SpecDD /src/workflow/definition.sdd
    - **Verified by:** `AC-1`

- **FR-2 - MUST:** Un DAG invalide n'est jamais exécuté. La validation structurelle est la seule fonction
  `validate_dag`, appelée à l'édition (`before_update`) et en tête de `DagInterpreter::run`, qui rend alors une
  `TerminalError` 400 `MRD-WORKFLOW-004` sans exécuter aucun step.
    - **Source:** `arch@c62a184`, « Revalidation à l'édition, filet à l'exécution »
    - **Realized by:** SpecDD /src/workflow/definition.sdd, SpecDD /src/workflow/interpreter.sdd, SpecDD /src/resource.sdd
    - **Verified by:** `AC-2`

- **FR-3 - MUST:** Tous les kinds passent par un unique service `StepDispatcher` qui résout le kind dans un
  `StepRegistry` fourni par l'app. Un kind inconnu rend 404 avant tout rendu de template.
    - **Source:** `arch@c62a184`, « Architecture : deux services Restate »
    - **Realized by:** SpecDD /src/workflow/dispatcher.sdd, SpecDD /src/workflow/step.sdd
    - **Verified by:** `AC-3`

- **FR-4 - MUST:** `trigger_run` ne bloque jamais sur la complétion d'un run, génère un `run_key` UUID v4 neuf à chaque
  appel, et refuse une clé fournie par l'appelant.
    - **Source:** `arch@c62a184`, « Déduplication de double déclenchement » (décision du développeur
      principal, 2026-09-23)
    - **Realized by:** SpecDD /src/workflow/client.sdd
    - **Verified by:** `AC-4`

- **FR-5 - MUST:** `register_deployment` est idempotent : plusieurs réplicas qui l'appellent en parallèle sur la même
  `deployment_url` obtiennent tous un `2xx`, sans état partagé.
    - **Source:** `arch@c62a184`, « Schéma de déploiement »
    - **Realized by:** SpecDD /src/workflow/client.sdd
    - **Verified by:** `AC-4`

- **FR-6 - MUST:** Chaque `string` de la `config` d'un step est rendue par `vynil_core::hbs` avant l'appel du kind,
  avec le contexte `{"inputs", "run"}`, en mode strict (variable absente = erreur), sans échappement HTML, en un seul
  passage, et sans jamais rendre les clés d'objet. Un kind déclare ses champs bruts via `raw_fields`.
    - **Source:** `arch@c62a184`, « Templating central » (arbitrage du 2026-10-06, livré le 2026-10-13)
    - **Realized by:** SpecDD /src/workflow/template.sdd, SpecDD /src/workflow/dispatcher.sdd
    - **Verified by:** `AC-5`

- **FR-7 - MUST:** Une erreur de rendu est un `StepError` non retryable qui ne contient jamais le contenu de `inputs`.
    - **Source:** `arch@c62a184`, « Templating central »
    - **Realized by:** SpecDD /src/workflow/template.sdd
    - **Verified by:** `AC-5`

- **FR-8 - MUST:** Un kind durable (`MiryadDurableStep`) reçoit un `StepContext` étroit (`sleep`, `run_effect`,
  `run_child_dag`) et est exécuté **sans** `ctx.run()` englobant. Tout kind reçoit un `RunInfo` en lecture seule
  (`run_key`, `step_id`, `depth`).
    - **Source:** `arch@c62a184`, « Contexte de run et kinds durables » ; issue #26 (2026-10-04)
    - **Realized by:** SpecDD /src/workflow/durable.sdd, SpecDD /src/workflow/step.sdd
    - **Verified by:** `AC-6`

- **FR-9 - MUST:** Le kind `subworkflow` embarque son DAG enfant dans le `config` du step, borne la récursion par une
  profondeur maximale à l'exécution (défaut 4, `0` désactive le kind) et rend `MRD-WORKFLOW-008` non retryable au
  dépassement. La clé du run enfant est `{run_key}:{step_id}`.
    - **Source:** `arch@c62a184`, « Sous-workflow » ; issue #26
    - **Realized by:** SpecDD /src/workflow/subworkflow.sdd
    - **Verified by:** `AC-6`

- **FR-10 - MUST:** `RhaiStep` part d'un moteur neuf à chaque exécution, s'exécute dans `spawn_blocking`, plafonne les
  opérations (`MAX_OPERATIONS = 10_000_000`, surchargeable par `with_setup`), n'embarque aucun chemin de résolution par
  défaut, et tout échec de script est non retryable.
    - **Source:** `arch@c62a184`, « Rhai et fonctions hôte » ; issue #27 (2026-10-04)
    - **Realized by:** SpecDD /src/workflow/rhai_step.sdd
    - **Verified by:** `AC-7`

- **FR-11 - MUST:** Le `name` d'une définition est unique **par propriétaire** : index composite (`owner_id`, `name`)
  pour les définitions possédées, index partiel sur `name` `WHERE owner_id IS NULL` pour les autres.
    - **Source:** `arch@c62a184`, « Unicité des définitions par propriétaire » ; issue #28 (2026-10-04)
    - **Realized by:** SpecDD /src/workflow/definition.sdd
    - **Verified by:** `AC-1`

## Constraints

- **CON-1 - MUST:** Le fan-out d'une couche passe par `ctx.request(...).call()` poussé dans un
  `DurableFuturesUnordered`, jamais par `futures::future::join_all`.
    - **Source:** `arch@c62a184`, « Le spike a tranché » (spike du 2026-09-22)
    - **Rationale:** `join_all` compile mais bloque chaque couche jusqu'à un délai interne du serveur (~60 s),
      invisible sans instrumentation.
    - **Realized by:** SpecDD /src/workflow/interpreter.sdd

- **CON-2 - MUST:** `register_deployment` pose `"force": true` à chaque appel.
    - **Source:** `arch@c62a184`, « Risque de rollout » (spike du 2026-09-23)
    - **Rationale:** sans ce champ, un `POST /deployments` sur une URI déjà enregistrée répond `200` mais ne redécouvre
      pas le schéma, sans erreur visible.
    - **Realized by:** SpecDD /src/workflow/client.sdd

- **CON-3 - MUST:** La porte sortante HTTP vers Restate (admin API, ingress) est bornée à `src/workflow/client.rs` ;
  le reste de la crate ne parle à Restate que par le protocole du SDK.
    - **Source:** `arch@c62a184`, « Architecture : deux services Restate »
    - **Realized by:** SpecDD /src/workflow/client.sdd

- **CON-4 - MUST NOT:** L'API publique n'est pas couplée à la version de `restate-sdk` : un kind ordinaire s'écrit et
  se teste sans instancier `restate-sdk`.
    - **Source:** `arch@c62a184`, « Architecture » et « Contexte de run et kinds durables »
    - **Realized by:** SpecDD /src/workflow/step.sdd, SpecDD /src/workflow/durable.sdd

- **CON-5 - MUST:** La sortie d'un rendu de template n'est jamais interprétée comme template, et les helpers d'accès à
  l'hôte de `vynil-core` (`read_to_str`, `env_var`, `canonicalize`, `parent`, `file_name`, `extension`, `jsonnet`)
  sont retirés du moteur.
    - **Source:** `arch@c62a184`, « Templating central », modèle de menace
    - **Realized by:** SpecDD /src/workflow/template.sdd

## Non-functional Requirements

- **NFR-1 - MUST:** Après un crash du processus service en milieu de couche, aucun step journalisé n'est rejoué et le
  fan-in reste correct.
    - **Source:** `arch@c62a184`, « Note d'exploitation » (spike du 2026-09-22)
    - **Verified by:** `AC-8`

- **NFR-2 - MUST:** Un step déclaré `retryable: true` est idempotent, car il est rejoué entier à chaque tentative.
    - **Source:** `arch@c62a184`, « Retry : la politique est posée au `bind()` »
    - **Realized by:** SpecDD /src/workflow/step.sdd

## Design Decisions

- **DEC-1 - Restate plutôt qu'un moteur de jobs**
    - **Context:** aucun des moteurs évalués n'avait un DAG piloté par la donnée nativement.
    - **Decision:** Restate (`restate-sdk` 0.12), retenu après le spike du 2026-09-22.
    - **Alternatives:** apalis-workflow (mis en standby le 2026-08-23), Acts (stockage tronqué à chaque redémarrage),
      Hatchet (binding Rust non officiel cassé sur `ctx.parent_output()`), Temporal, Prefect.
    - **Consequences:** un cluster Restate à héberger, que la crate ne gère pas (CON-1 de `miryad-core`).
    - **Source:** `arch@c62a184`, « Moteurs écartés »

- **DEC-2 - Un seul `StepDispatcher`, la dynamique dans un registre**
    - **Context:** `restate-sdk` lie ses services à la compilation ; aucun enregistrement dynamique par kind.
    - **Decision:** un service `StepDispatcher` unique et un `StepRegistry` en Rust pur fourni par l'app.
    - **Consequences:** un kind se teste comme une fonction async ordinaire (CON-4).
    - **Source:** `arch@c62a184`, « Architecture : deux services Restate »

- **DEC-3 - Pas de politique de retry dans la crate**
    - **Context:** sans `ServiceOptions` au `bind()`, le défaut serveur est un retry exponentiel indéfini.
    - **Decision:** la crate expose `recommended_options()` (5 tentatives puis `pause`) sans l'imposer ; relancer un run
      `paused` est une action manuelle de l'opérateur.
    - **Consequences:** ignorer `recommended_options()` est un choix de l'app. Un panic dans un `ctx.run()` n'interrompt
      que l'invocation en cours.
    - **Source:** `arch@c62a184`, « Retry » (décision du 2026-09-23)
    - **Realized by:** SpecDD /src/workflow/dispatcher.sdd

- **DEC-4 - La crate n'impose pas la forme de `deployment_url`**
    - **Context:** une IP de pod éphémère devient invisible au premier re-scheduling.
    - **Decision:** la crate ne valide pas l'URL ; la recommandation d'un `Service` Kubernetes stable vit ici, jamais
      en code.
    - **Source:** `arch@c62a184`, « Recommandation `deployment_url` » (décision du développeur principal,
      2026-09-23)

- **DEC-5 - Le sous-DAG est en ligne, garde-fou à l'exécution**
    - **Context:** un cycle A → B → A traverse plusieurs définitions et n'est pas détectable statiquement.
    - **Decision:** le DAG enfant est dans le `config` du step ; la protection est la profondeur maximale (FR-9).
    - **Consequences:** en v1, l'enfant n'hérite d'aucune donnée du parent et son échec fait échouer le parent.
    - **Source:** `arch@c62a184`, « Sous-workflow »

- **DEC-6 - Liste noire de helpers, transitoire et fail-open**
    - **Context:** `vynil-core` n'offrait pas de moteur sans helpers d'accès à l'hôte.
    - **Decision:** une liste noire `DENIED_HELPERS` retire les helpers dangereux, en attendant
      `sebt3/vynil-core#13` (arbitrage du développeur principal).
    - **Consequences:** un helper dangereux ajouté par un futur `vynil-core` serait exposé sans que rien ne casse ;
      `CORE_HBS_HELPERS` doit être relu à chaque montée de version. Sortie de transition : voir `OI-1`.
    - **Source:** `arch@c62a184`, « La liste noire est transitoire et fail-open »
    - **Realized by:** SpecDD /src/workflow/template.sdd

## Operations and Resilience

- **Rollout :** `force: true` (CON-2) peut, selon l'OpenAPI de l'admin API de Restate, laisser des invocations en cours
  dans un état irrécupérable. Un rolling update qui change le schéma exposé pendant que des invocations tournent sur
  l'ancien est donc un risque d'exploitation. La stratégie (drain avant enregistrement, ou versionnement) appartient
  à l'app, car la crate ne peut pas la détecter depuis un appel HTTP synchrone.
    - **Source:** `arch@c62a184`, « Risque de rollout »

- **Pièges constatés sur le templating :**
    - `to_json` est un helper de bloc : en forme inline il rend une chaîne vide sans erreur. Utiliser `json_to_str`.
    - Le plafond de 1 Mio par rendu est contrôlé après le rendu : il borne la sortie, pas la mémoire ni le CPU. À
      reprendre si un consommateur expose des templates à des tiers non fiables.
    - Un kind durable est rendu dans un `ctx.run()` dédié : le rendu est journalisé, donc un helper non déterministe
      (`uuid_new_v4`) ne change pas de valeur au rejeu.
    - **Source:** `arch@c62a184`, « Pièges constatés »

## SpecDD Realization

Onze des treize specs de `src/workflow/` réalisent cette Solution. Les mappings fins sont portés par les champs
`Realized by` ci-dessus. `SpecDD /src/workflow/mod.sdd` agrège le module et son graphe de dépendances Cargo ;
`SpecDD /src/workflow/error.sdd` porte le registre `MRD-WORKFLOW-001` à `008`. Les specs
`SpecDD /src/workflow/matrix_step.sdd` et `SpecDD /src/workflow/systemone_step.sdd` appartiennent aux Solutions de
leurs clients (TBD).

## Acceptance Criteria

**ASSUMPTION :** `AC-1` à `AC-7` sont formulés d'après les requirements et les `Scenario` des specs
(`src/workflow/*.sdd`). Leur correspondance exacte avec chaque `Scenario` n'a pas été vérifiée dans ce pilote.

- **AC-1:** Une `WorkflowDefinition` créée par un admin est relue à l'identique ; deux définitions de même `name`
  coexistent si leurs propriétaires diffèrent, et sont refusées pour un même propriétaire.
- **AC-2:** Mettre à jour une définition avec un DAG cyclique est refusé ; déclencher un run sur un DAG invalide
  rend `MRD-WORKFLOW-004` sans exécuter de step.
- **AC-3:** Un step de kind inconnu rend 404 et son template n'est pas rendu.
- **AC-4:** Deux `trigger_run` successifs produisent deux `run_key` distincts ; une clé fournie par l'appelant est
  refusée ; deux `register_deployment` concurrents sur la même URL aboutissent tous deux.
- **AC-5:** Une variable absente dans un template échoue le step sans retry, et le message d'erreur ne contient pas les
  `inputs`.
- **AC-6:** Un `subworkflow` de profondeur supérieure au maximum rend `MRD-WORKFLOW-008` sans retry ; un kind durable
  peut dormir et lancer un sous-DAG via `StepContext`.
- **AC-7:** Un script Rhai en boucle infinie s'arrête au plafond d'opérations ; deux exécutions successives ne
  partagent aucun état.
- **AC-8:** Reprise sur crash du service en milieu de couche sans rejeu des steps journalisés.
    - **Verification:** par la méthode du spike du 2026-09-22 (tuer le processus, le relancer, vérifier). Les tests
      `#[ignore]` de `tests/workflow_restate.rs` exigent `podman` et ne tournent pas en CI : AC-8 n'est donc **pas
      verrouillé par la CI** (voir `OI-2`).

## Open Issues

- **OI-1:** `sebt3/vynil-core#13` est **fermée, livrée dans vynil-core 0.8.0** (2026-10-11,
  `HandleBars::new_restricted()`), alors que `Cargo.toml` pin encore `vynil-core` 0.7.7 (la dernière est 0.9.0).
  La tâche ouverte de `SpecDD /src/workflow/template.sdd` (remplacer `DENIED_HELPERS` par le constructeur restreint)
  n'est donc plus bloquée. `roadmap@c62a184` la présente encore comme conditionnelle à une livraison qui a eu lieu.
  Source : issue `sebt3/vynil-core#13`.
- **OI-2:** AC-8 (reprise sur crash) n'a pas de test automatisable. Une preuve par spike est à refaire à toute évolution
  majeure de `restate-sdk`. **TBD :** décider si un test d'intégration en cluster de CI est souhaitable.
- **OI-3:** Pilotage d'un run par awakeable : reporté par arbitrage tant qu'aucun besoin réel ne l'impose (le
  consommateur sonde dans un step ordinaire). Toute feature de workflow au-delà passe par un arbitrage avant de gagner
  une spec. Source : `roadmap@c62a184`, item 9.
- **OI-4:** Lecture du statut d'un run : `client.rs` ne fait qu'enregistrer et déclencher ; une fonction de lecture est
  « un fichier à spécifier si le besoin se confirme ». **TBD.** Source : `arch@c62a184`, « Schéma de
  déploiement ».
