# Solution Design: Moteur de workflow

## Objective

Permettre à une application consommatrice de stocker des workflows comme **données** (un DAG de steps), de les faire
exécuter durablement par un cluster Restate self-hosté, et d'y brancher ses propres kinds de steps.

Un run dispose d'un **contexte immuable** : les résultats des steps précédents (`inputs`), l'identité du run (`run`) et
des variables constantes fournies au déclenchement (`vars`). Un contexte modifiable n'est pas dans cette Solution
(voir `OI-8`).

## Scope and Non-goals

La Solution fournit deux services Restate (`DagInterpreter`, `StepDispatcher`), la définition et la validation du DAG,
un client HTTP vers Restate, le templating des `config`, et les kinds natifs `rhai` et `subworkflow`.

Hors Solution :

- Elle ne monte **aucune** route sur le `axum::Router` : l'app construit et lie son `Endpoint` dans son `main()`.
- Elle ne gère jamais le cluster Restate (CON-1 de `miryad-core`).
- Elle ne duplique jamais le statut d'un run en base : il se lit directement dans Restate.
- Elle ne porte aucun état modifiable en cours de run : le pad Rhai de la future feature optionnelle `lymaille` en
  relève (`OI-8`).
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
  avec le contexte `{"inputs", "run", "vars"}`, en mode strict (variable absente = erreur), sans échappement HTML, en un seul
  passage, et sans jamais rendre les clés d'objet. Un kind déclare ses champs bruts via `raw_fields`.
    - **Source:** `arch@c62a184`, « Templating central » (arbitrage du 2026-10-06, livré le 2026-10-13)
    - **Realized by:** SpecDD /src/workflow/template.sdd, SpecDD /src/workflow/dispatcher.sdd
    - **Verified by:** `AC-5`

- **FR-7 - MUST:** Une erreur de rendu est un `StepError` non retryable qui ne contient jamais le contenu de `inputs` ni de `vars`.
    - **Source:** `arch@c62a184`, « Templating central » ; extension `vars` : user instruction, 2026-10-30
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

- **FR-12 - MUST:** `trigger_run` accepte des variables de run `vars` (objet JSON, vide par défaut) et les transmet en
  entrée du run avec le DAG. Elles sont constantes pour toute la durée du run et journalisées par Restate.
    - **Source:** user instruction, 2026-10-30 ; Observed in: `vanyline` `app/src/workflow/run_context.rs` (injection de
      `config._run` dans chaque step, faute de variables de run)
    - **Realized by:** SpecDD /src/workflow/client.sdd, SpecDD /src/workflow/interpreter.sdd
    - **Verified by:** `AC-9`

- **FR-13 - MUST:** Chaque invocation de step reçoit les `vars` du run, inchangées et en lecture seule, à côté de
  `RunInfo`. Un kind ordinaire comme un kind durable les lit.
    - **Source:** user instruction, 2026-10-30
    - **Realized by:** SpecDD /src/workflow/dispatcher.sdd, SpecDD /src/workflow/step.sdd
    - **Verified by:** `AC-9`

- **FR-14 - MUST:** Le kind `rhai` expose `vars` comme variable ordinaire du script, comme `inputs` et `run`.
    - **Source:** user instruction, 2026-10-30
    - **Realized by:** SpecDD /src/workflow/rhai_step.sdd
    - **Verified by:** `AC-11`

- **FR-15 - MUST:** Les `vars` d'un run enfant sont le champ `vars` du `config` du step `subworkflow`, rendu par le
  parent. L'enfant n'hérite d'aucune autre donnée du parent.
    - **Source:** user instruction, 2026-10-30 ; `DEC-5`
    - **Realized by:** SpecDD /src/workflow/subworkflow.sdd
    - **Verified by:** `AC-14`

- **FR-16 - MUST:** `RhaiStep` construit son moteur par **liste blanche explicite** : le groupe pur du cœur de
  `vynil-core` (`core_pure_rhai_register`) et les registrars purs `chrono`, `hashes`, `semver`, `yaml` et `glob`. Il
  n'enregistre ni le groupe hôte (`get_env`, `log_*`), ni `sleep`, ni les accès fichiers, `shell`, `http`, `s3`, `oci`
  ou `k8s`, quelles que soient les features de `vynil-core` activées ailleurs dans le graphe de dépendances. L'inscription
  « ne pas enregistrer » ne couvre pas le substrat de rhai lui-même : `sleep(INT)`/`sleep(FLOAT)` est enregistré par le
  `lang_core` de rhai sur tout `Engine::new` et reste appelable — substrat accepté (`DEC-8`, amendé le 2026-11-01).
    - **Source:** user instruction, 2026-10-30 ; vynil-core `CHANGELOG` 0.9.0 (#18) ; substrat `sleep`, vérifié au source
      de rhai 1.26.1 et arbitré par le développeur principal le 2026-11-01
    - **Rationale:** `Script::new_bare` enregistre tout ce que les features unifiées activent ; un auteur de workflow
      lirait alors l'environnement du processus ou lancerait un shell.
    - **Realized by:** SpecDD /src/workflow/rhai_step.sdd
    - **Verified by:** `AC-11`

- **FR-17 - MUST:** La crate fournit aux scripts `log_debug`, `log_info`, `log_warn` et `log_error`, qui émettent un
  événement `tracing` portant `run_key` et `step_id` du step courant. `log_debug` est requis par les shims
  `import_run` et `import_template` de `Script::add_common`.
    - **Source:** user instruction, 2026-10-30 ; Observed in: vynil-core 0.9.0 `src/engine.rs` (`add_common`)
    - **Realized by:** SpecDD /src/workflow/rhai_step.sdd
    - **Verified by:** `AC-11`

- **FR-18 - MUST:** `RhaiStep` borne la durée réelle d'un script. À l'expiration, l'exécution est annulée (jeton
  d'annulation de `vynil-core` pour les appels natifs bloquants, `on_progress` pour les boucles Rhai) et le step échoue
  en `MRD-WORKFLOW-009`, non retryable. TBD soldé le 2026-11-01 par le développeur principal : valeur par défaut `300` s,
  plafond `3600` s, surcharge par le champ `timeout_secs` du `config`.
    - **Source:** user instruction, 2026-10-30 ; vynil-core `CHANGELOG` 0.9.0 (#21) ; arbitrage valeurs, user instruction, 2026-11-01
    - **Realized by:** SpecDD /src/workflow/rhai_step.sdd, SpecDD /src/workflow/error.sdd
    - **Verified by:** `AC-12`

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

- **CON-5 - MUST:** La sortie d'un rendu de template n'est jamais interprétée comme template, et le moteur de rendu est
  le moteur restreint `HandleBars::new_restricted()` de `vynil-core` : aucun helper d'accès à l'hôte (`read_to_str`,
  `env_var`, `canonicalize`, `parent`, `file_name`, `extension`) n'est enregistré, et `jsonnet` ne lit aucun fichier de
  l'hôte.
    - **Source:** `arch@c62a184`, « Templating central », modèle de menace ; vynil-core `CHANGELOG` 0.9.0 (#27)
    - **Rationale:** les templates sont écrits par les auteurs de workflows. `jsonnet` reste enregistré par
      `new_restricted()` en 0.9.0 et construit une VM sans résolveur d'imports : la lecture de fichiers par
      `importstr` est probable mais **non exécutée** (voir `AC-13`).
    - **Realized by:** SpecDD /src/workflow/template.sdd
    - **Verified by:** `AC-13`

- **CON-6 - MUST NOT:** Les `vars` ne contiennent pas de secret.
    - **Source:** user instruction, 2026-10-30
    - **Rationale:** l'entrée d'un run est journalisée par Restate et visible dans son interface.
    - **Realized by:** SpecDD /src/workflow/client.sdd

- **CON-7 - MUST NOT:** Une valeur de `vars` n'est jamais interprétée comme gabarit : elle est de la donnée dans le
  contexte de rendu, pas du texte source.
    - **Source:** user instruction, 2026-10-30 ; `CON-5`
    - **Rationale:** l'injection dans le `config` de chaque step (usage actuel de `vanyline`) soumet les valeurs au
      templating de `FR-6`.
    - **Realized by:** SpecDD /src/workflow/template.sdd
    - **Verified by:** `AC-10`

- **CON-8 - MUST NOT:** `vars`, `inputs` et `run` ne sont jamais posées en constante dans le scope Rhai
  (`Scope::push_constant`) : elles le sont comme variables ordinaires.
    - **Source:** Observed in: `lymaille` `dsd/agent-sessions/scratchpad.md`, « Faits vérifiés »
    - **Rationale:** modifier une propriété d'une constante map fait paniquer le processus avec l'optimiseur par défaut
      (Rhai 1.25.1 et 1.27.0). **ASSUMPTION :** Rhai 1.26.1, verrouillé ici, est concerné ; non reproduit.
    - **Realized by:** SpecDD /src/workflow/rhai_step.sdd
    - **Verified by:** `AC-11`

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

- **DEC-7 - Templating : liste blanche amont plutôt que liste noire locale**
    - **Context:** une liste noire locale (`DENIED_HELPERS`) était fail-open : un helper dangereux ajouté par un futur
      `vynil-core` serait exposé sans que rien ne casse.
    - **Decision:** monter `vynil-core` à 0.9.0 et employer `HandleBars::new_restricted()`, construit depuis une liste
      blanche (`sebt3/vynil-core#13`, #27).
    - **Alternatives:** conserver la liste noire (écarté : fail-open) ; `vynil-core` 0.8.0 (écarté : `new_restricted()`
      y retire six helpers de `new()` au lieu de partir d'une liste blanche).
    - **Consequences:** `DENIED_HELPERS` disparaît, sauf pour `jsonnet` si `AC-13` montre qu'il lit des fichiers ; dans
      ce cas la correction relève de `vynil-core` (`OI-1`).
    - **Source:** user instruction, 2026-10-30 ; vynil-core `CHANGELOG` 0.8.0 et 0.9.0

- **DEC-8 - Rhai : moteur construit par liste blanche, journal et attente fournis par la crate**
    - **Context:** `Script::new_bare` enregistre `get_env` (donc l'environnement du processus), `sleep`, et tout ce que
      les features de `vynil-core` unifiées dans le graphe activent.
    - **Decision:** construire le `Script` à la main (`Engine::new()`, résolveur, plafonds, registrars de `FR-16`,
      `log_*` de la crate, puis `add_common`). Un `log_*` propre à la crate remplace le journal du processus par un
      journal du step.
    - **Alternatives:** garder `new_bare` (écarté : exposition de l'hôte) ; masquer `get_env` et `sleep` par des
      surcharges qui échouent (écarté : fonctions présentes et appelables ; impossible pour `sleep` depuis le
      2026-11-01 — il vient du `lang_core` de rhai, pas d'un registrar `vynil-core`, et une surcharge le laisserait
      de toute façon appelable sous un autre nom) ; demander un constructeur à sélection de
      groupes à `vynil-core` (non retenu pour ce lot).
    - **Consequences:** la liste de registrars est dupliquée de `new_bare` et doit être relue à chaque montée de
      `vynil-core`. Un script qui employait `get_env` casse. `sleep` **ne casse pas** : correction actée le 2026-11-01,
      vérifiée au source de rhai 1.26.1 (`src/packages/lang_core.rs`), `sleep(INT)`/`sleep(FLOAT)` est un substrat du langage
      présent sur tout `Engine::new`, indissociable du cœur sans casser le langage — comportement **accepté** par le
      développeur principal. Il est bloquant pour le fil de travail et non interrompu par l'annulation coopérative du
      `FR-18`. Aucun usage dans le code de `vanyline`
      (vérifié le 2026-10-30) ; les scripts écrits par les utilisateurs vivent en base et n'ont pas été examinés.
      Un sommeil durable existe par `MiryadDurableStep`.
    - **Source:** user instruction, 2026-10-30 ; vynil-core 0.9.0 (#18, `src/engine.rs`)
    - **Realized by:** SpecDD /src/workflow/rhai_step.sdd

- **DEC-9 - Contexte de run immuable, variables en entrée du run**
    - **Context:** le moteur ne donne à un step que `config` et `inputs`. `vanyline` contourne en recopiant un contexte
      dans `config._run` de chaque step d'un DAG fabriqué par run.
    - **Decision:** `vars` est une entrée du run, à côté du DAG, objet JSON libre sans schéma déclaré (`OI-7`).
    - **Alternatives:** injection dans le `config` de chaque step (statu quo : duplication, valeurs soumises au
      templating, convention d'écrasement d'une clé réservée) ; variables modifiables par les steps (écarté : voir
      `OI-8`).
    - **Consequences:** changement cassant de la signature de `trigger_run` et de l'entrée du handler `run`
      (pré-1.0, sans production) ; `vanyline` adapte son appel et peut retirer l'injection de `config._run`.
    - **Source:** user instruction, 2026-10-30

- **DEC-10 - Évaluation Rhai sur un fil dédié à pile dimensionnée**
    - **Context:** `spawn_blocking` donne une pile de 2 Mio ; le test A6 du 2026-11-01 reproduit sur `RhaiStep` le débordement mesuré par `lymaille` :
      SIGABRT dès 144 niveaux de récursion en debug, entre 256 et 384 en release — la pile native s'épuise avant le
      plafond logique de 512 niveaux, et Restate rejouerait à l'infini une invocation avortée.
    - **Decision:** l'évaluation du script s'exécute dans un fil `std::thread` à pile de 256 Mio (deux fois le pire cas
      de 125 Mio mesuré par `lymaille`), créé dans le `spawn_blocking` et joint par canal ; les plafonds Rhai restent
      la seule limite logique.
    - **Alternatives:** abaisser les plafonds (écarté : casse les scripts légitimes et dépend du profil et de la
      plateforme) ; différer (écarté : lot A non clos, suite de tests qui avorte sous toute combinaison `workflow`).
    - **Consequences:** un thread OS et 256 Mio réservés (engagés à la touche) par évaluation ; le fil survit au
      timeout coopératif comme celui du `spawn_blocking` ; la panique de ce fil rejoint le rendu `JoinError` de l'étape 4.
    - **Source:** user instruction, 2026-11-01 ; mesures du test A6, 2026-11-01 ; `lymaille` `dsd/agent-sessions/scratchpad.md`, DEC-3 (amendement du 2026-10-27)

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
`SpecDD /src/workflow/error.sdd` porte le registre `MRD-WORKFLOW-001` à `009`. Les specs
`SpecDD /src/workflow/matrix_step.sdd` et `SpecDD /src/workflow/systemone_step.sdd` appartiennent aux Solutions de
leurs clients (TBD).

## Acceptance Criteria

**ASSUMPTION :** `AC-1` à `AC-7` et `AC-9` à `AC-14` sont formulés d'après les requirements et les `Scenario` des specs
(`src/workflow/*.sdd`). Leur correspondance exacte avec chaque `Scenario` n'a pas été vérifiée dans ce pilote.

- **AC-1:** Une `WorkflowDefinition` créée par un admin est relue à l'identique ; deux définitions de même `name`
  coexistent si leurs propriétaires diffèrent, et sont refusées pour un même propriétaire.
- **AC-2:** Mettre à jour une définition avec un DAG cyclique est refusé ; déclencher un run sur un DAG invalide
  rend `MRD-WORKFLOW-004` sans exécuter de step.
- **AC-3:** Un step de kind inconnu rend 404 et son template n'est pas rendu.
- **AC-4:** Deux `trigger_run` successifs produisent deux `run_key` distincts ; une clé fournie par l'appelant est
  refusée ; deux `register_deployment` concurrents sur la même URL aboutissent tous deux.
- **AC-5:** Une variable absente dans un template échoue le step sans retry, et le message d'erreur ne contient ni les
  `inputs` ni les `vars`.
- **AC-6:** Un `subworkflow` de profondeur supérieure au maximum rend `MRD-WORKFLOW-008` sans retry ; un kind durable
  peut dormir et lancer un sous-DAG via `StepContext`.
- **AC-7:** Un script Rhai en boucle infinie s'arrête au plafond d'opérations ; deux exécutions successives ne
  partagent aucun état.
- **AC-9:** Un run déclenché avec `vars = {"project": "p1"}` rend `p1` pour `{{vars.project}}` dans deux steps de couches
  différentes ; un run déclenché sans `vars` échoue sur cette même variable (mode strict) ; `vars` est vide par défaut.
- **AC-10:** Une valeur de `vars` qui contient `{{inputs.x}}` est rendue littéralement et jamais évaluée.
- **AC-11:** Dans un script Rhai, `vars` est lisible et `vars.a = 1` ne panique pas ; `get_env` et les accès
  fichiers ou `shell` sont introuvables, y compris quand une dépendance de développement active les features `fs` et
  `shell` de `vynil-core` ; `log_info` répond et émet un événement `tracing` portant `run_key` et `step_id`. (`sleep`
  retiré le 2026-11-01 : substrat rhai accepté, `DEC-8`.)
- **AC-12:** Un script en boucle Rhai pure et un script bloqué dans un appel natif sont interrompus à l'échéance, en
  `MRD-WORKFLOW-009` non retryable.
- **AC-13:** `{{#jsonnet}}importstr "<fichier réel>"{{/jsonnet}}` rendu par le moteur de template ne produit pas le
  contenu du fichier.
- **AC-14:** Un `subworkflow` dont le `config` porte `vars = {"a": "{{inputs.x}}"}` donne à l'enfant la valeur rendue ;
  l'enfant ne voit pas les `vars` du parent.
- **AC-8:** Reprise sur crash du service en milieu de couche sans rejeu des steps journalisés.
    - **Verification:** par la méthode du spike du 2026-09-22 (tuer le processus, le relancer, vérifier). Les tests
      `#[ignore]` de `tests/workflow_restate.rs` exigent `podman` et ne tournent pas en CI : AC-8 n'est donc **pas
      verrouillé par la CI** (voir `OI-2`).

## Open Issues

- **OI-1:** **`jsonnet` reste désinscrit côté crate** : la montée à `vynil-core` 0.9.0 est livrée (lot A, A1), mais
  `AC-13` a échoué (`jsonnet` lit des fichiers par `importstr`), d'où `DENIED_HELPERS = ["jsonnet"]` dans
  `src/workflow/template.rs` (`DEC-8`). **TBD :** correction en amont dans `vynil-core` (`new_restricted()` devrait
  retirer `jsonnet` lui-même), après quoi `DENIED_HELPERS` disparaît.
- **OI-2:** AC-8 (reprise sur crash) n'a pas de test automatisable. Une preuve par spike est à refaire à toute évolution
  majeure de `restate-sdk`. **TBD :** décider si un test d'intégration en cluster de CI est souhaitable.
- **OI-3:** Pilotage d'un run par awakeable : reporté par arbitrage tant qu'aucun besoin réel ne l'impose (le
  consommateur sonde dans un step ordinaire). Toute feature de workflow au-delà passe par un arbitrage avant de gagner
  une spec. Source : `roadmap@c62a184`, item 9.
- **OI-4:** Lecture du statut d'un run : `client.rs` ne fait qu'enregistrer et déclencher ; une fonction de lecture est
  « un fichier à spécifier si le besoin se confirme ». **TBD.** Source : `arch@c62a184`, « Schéma de
  déploiement ».
- **OI-5:** **Conditions d'exécution** : le DAG est statique (`id`, `depends_on`, `kind`, `config`), sans `when`, boucle ni
  branche. **TBD :** sémantique d'un step ignoré pour ses dépendants. Un échec de step est aujourd'hui le seul moyen
  de décider (par exemple `on_fail: "continue"` de `systemone`, qui « laisse l'aval décider »). **TBD :** le kind
  `gate` de `vanyline` couvre-t-il ce besoin chez lui ?
- **OI-6:** **« Faits » établis par des steps** : non retenus pour l'instant. Sans condition, aucun consommateur ; modifiables, leur
  valeur dépendrait de l'ordonnancement des steps d'une même couche ; en écriture unique, ils ne sont qu'un alias de
  sortie. Définition candidate si le besoin se confirme après `OI-5` : un **export nommé** déclaré par un step, à
  producteur unique, dont `validate_dag` vérifie que tout consommateur dépend du producteur.
- **OI-7:** **Déclaration des `vars`** (noms attendus, défauts, obligatoires) : absente en v1. **TBD :** nécessaire si une
  interface expose les paramètres d'une définition à l'utilisateur.
- **OI-8:** **Contexte modifiable** : relève d'une feature optionnelle `lymaille`, distincte de `workflow` (pad Rhai
  associé à un run, hors du premier jalon de `lymaille`). Exigences à y porter : steps de pad en chaîne
  totalement ordonnée, vérifiée par `validate_dag` (des steps parallèles qui écrivent la même variable donneraient un
  résultat dépendant de l'ordonnancement) ; instantané en JSON ; idempotence au rejeu d'un step interrompu (`NFR-2`).
  **TBD :** état persisté dans l'état clé-valeur du workflow Restate plutôt que dans un second store.
    - **Source:** `lymaille` `dsd/agent-sessions/scratchpad.md`, « Extension réservée : pad de workflow » ;
      user instruction, 2026-10-30
    - La Solution `miryad-core/lymaille` est ouverte depuis le 2026-11-01 et rattache cette réserve ; le **TBD**
      persistant reste à trancher dans cette Solution.
- **OI-9:** **Soldé le 2026-11-02** (lot A, A6) : pile des threads de `RhaiStep`. `spawn_blocking` (2 Mio) avortait le
  processus avant le plafond Rhai de 512 niveaux (SIGABRT dès 144 niveaux en debug, 384 en release) ; Restate aurait
  rejoué l'invocation en boucle. Tranché le 2026-11-01 : fil dédié à pile de 256 Mio, voir `DEC-10`. Conservé comme
  renvoi des specs et du code.
    - **Source:** `lymaille` `dsd/agent-sessions/scratchpad.md`, DEC-3 (amendement du 2026-10-27) ; mesures du test A6,
      2026-11-01 ; arbitrage, user instruction, 2026-11-01
