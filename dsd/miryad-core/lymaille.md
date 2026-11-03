# Solution Design: Intégration lymaille

## Objective

Héberger la bibliothèque `lymaille` (agent LLM durable : sessions persistées, pad Rhai, toolsets à découverte paresseuse)
derrière une feature optionnelle `lymaille` de miryad-core : modules pour que les LLM accèdent aux ressources
déclarées par `MiryadResource`, pad Rhai associé à un run de workflow (`miryad-core/workflow#OI-8`), steps de sous-agent
et appels d'MCP externes depuis les scripts.

## Scope and Non-goals

- Feature **optionnelle, hors défaut, impliquant `db`** (DEC-1). miryad-core est l'**hôte** au sens de `lymaille` : elle
  implémente les traits de stockage sur SeaORM, résout les références, porte la chaîne d'identité et la parité de
  surface.
- **Non-goal :** la conception des sessions, du scratchpad, des toolsets, de la délégation et de l'inférence. Elle vit
  dans le DSD de `lymaille` (`agent-sessions`) et n'est pas à redéduire ni à dupliquer ici.
- **Préalable levé le 2026-11-02 :** `lymaille` 0.1.0 est publiée sur crates.io (Observed in: `lymaille` commit
  `122ce23`, « Version 0.1.0 : publish levé »). **Limite de cette release :** `SessionKind` ne compte que
  `Work | Config | Sub` ; le pad de workflow (`PW-1` à `PW-5`, nature `Workflow`) est une « extension réservée, hors du
  premier jalon » (Observed in: `lymaille/src/session/state.rs`, `lymaille/dsd/agent-sessions/scratchpad.md`, 2026-11-02).
  Le step `pad` dépend donc d'une release ultérieure de `lymaille` (`OI-3`).

## Current State and Target State

- **Actuel :** lot 0 livré le 2026-11-02 (`FR-10` : le moteur de workflow transporte l'identité du déclencheur — `trigger_run` → `RunInput` → `StepInvocation` → `RunInfo`, héritée par les runs enfants ; commits `667518c` → `1de97e6`, treize combinaisons vertes). Sinon rien : aucune feature `lymaille`, aucun `src/lymaille/`, aucune spec. La feature était réservée par `miryad-core/workflow#OI-8`. `lymaille`
  0.1.0 est disponible (dépendance par `path` ou crates.io).
- **Cible :** feature `lymaille`, modules sous `src/lymaille/` (stores SeaORM des traits hôte, module Rhai des ressources,
  kind de step sous-agent ; ensuite kind de step pad et glue vers `matrix`), migrations internes `miryad_lymaille_*`.

## Faits vérifiés (exploration d'intégrabilité, 2026-11-01)

- Le graphe combiné miryad-core (toutes features) + lymaille **se compile** : rhai s'unit en **1.27.0 en un seul
  exemplaire** (`vynil-core` 0.9 exige `^1.23.3` et absorbe l'épinglage de lymaille), tokio 1.54.1 et rustls 0.23.46
  unifiés, `sea-orm` 2.0.4 seul. **Verified by:** `AC-1`.
- lymaille **ne porte aucun stockage SQL** : aucun `sqlx` ni `libsqlite3-sys` dans son graphe, une quinzaine de traits
  de store laissés à l'hôte (`SessionStore`, `ToolsetStore`, `MemoryStore`, `ProfileStore`, `ContextStore`, `AgentStore`,
  etc.). L'hypothèse DEC-1 est donc exacte. **Verified by:** `AC-1`.
- lymaille **avorte si le hôte n'a pas installé un crypto provider rustls** avant le premier appel rig ou MCP client
  (`reqwest-tls-no-provider` ; Observed in: `lymaille/src/agents/rig.rs`, `lymaille/src/modules/mcp_client.rs`). C'est
  un piège d'intégration, pas un détail — tranché par `DEC-4`.
- `SessionKind = Work | Config | Sub` et la règle `agent-sessions#BR-8` (l'agent de configuration ne délègue pas,
  n'appelle aucun MCP, active rien) existent nativement chez lymaille : les deux axes de DEC-2 s'y appuient sans
  adaptation.
- La combinaison `matrix` + `lymaille` est déjà prouvée compilable dans le même graphe ; la jugée amont est le handler
  public `MatrixEventHandler::on_message` (harnais chatbot de `src/matrix/sync.sdd`) et `MatrixClient::send`. Le DSD de
  lymaille cite une « application de réception d'alertes avec agent Matrix » comme hôte type.

- **Surface de `lymaille` 0.1.0 pour le lot 1** (Observed in: `lymaille/src/**`, lecture du 2026-11-02) :
    - Les traits de store sont `async` (futures `Send`), **sans listing ni suppression** (« elles appartiennent à l'hôte »), utilisés par
      paramètre générique, jamais en objet-trait : `SessionStore` (`load`, `save`), `AgentStore` (`get_agent`, `put_agent`),
      `ProfileStore` (`get_profile`, `put_profile`), `ProviderStore` (`get_provider`, `put_provider`), `ToolsetStore` (`get_active`,
      `get_draft`, `put_draft`), `ContextTypeStore` (`get_type`, `put_type`), `ContextStore` (`get_context`, `put_context`),
      `MemoryStore` (`list`, `get`, `put`, `delete`, clé = portée `MemoryScope` puis identifiant).
    - `put_agent` et `put_draft` **valident** (`AgentDefinition::validate`, contrat du source, gabarits de skill, plafond de rendu) ;
      une implémentation hôte applique les mêmes contrôles.
    - Les objets lymaille sont `#[non_exhaustive]` et sérialisables en JSON ; les stores mémoire gardent le **texte JSON**, pas l'objet
      (un rechargement passe par le même format qu'un vrai stockage).
    - `Provider` ne porte que des références (`name`, `kind`, `address`, `auth: Option<String>`, `ProviderLevel::{User, Project}`) ; la
      résolution de `auth` en secret est à la charge de l'hôte (`DEC-16`). `AgentDefinition` désigne son modèle par un **nom de profil**,
      jamais un provider.
    - Un run de sous-agent batch s'assemble côté hôte : `ScratchpadEngine::builder(limits, grants)` avec les `Grants` produits par
      `assemble(toolsets, modules, dépendances, contexte, limites)`, `result_module(Arc<ResultCollector>)` (`submit`, `fail`),
      `restore_session(store, id, engine, provenance)` pour la reprise, `TurnRunner::new(provider, strategy, profile, context, pad,
      store, state, config)`, puis `run_batch(runner, task, collector, cancel, max_turns)`. `CancelHandle` est coopératif.
    - Le module de délégation n'existe pas pour un sous-agent (garanti par le type, `delegation#FR-2`).

## Applicable Domain Requirements

`miryad-core#FR-1` (parité des surfaces — le canal agent devient un consommateur des politiques), `miryad-core#CON-3`
(codes `MRD-LYMAILLE-NNN`, `DEC-5`), `miryad-core#CON-4` (feature et batterie), `miryad-core#CON-6` (préfixe
`miryad_lymaille_`, `DEC-5`).

## Functional Requirements

- **FR-1 - MUST:** La feature Cargo `lymaille` implique `db` ; sans `db`, rien de `lymaille` ne compile ni ne tire le
  graphe.
    - **Source:** user instruction, 2026-11-01 ; DEC-1
    - **Realized by:** aucune spec à ce jour ; le mapping s'ouvrira avec le premier lot.
    - **Verified by:** `AC-2`

- **FR-2 - MUST:** Tout module exposant aux LLM une ressource `MiryadResource` la lit et l'écrit par les mêmes
    `read_policy`, `write_policy`, `owner_column` et hooks de cycle de vie que REST et MCP. Le canal agent est une
    troisième surface au sens de la parité ; un écart y est une erreur de contrat, pas une dette.
    - **Source:** `miryad-core#FR-1` ; user instruction, 2026-11-01
    - **Lot 1 (`DEC-15`) :** lecture seule (`describe`, `list`, `get`) ; les écritures suivent dans un lot ultérieur.
    - **Verified by:** `AC-3`

- **FR-3 - MUST:** Les traits de store de `lymaille` nécessaires au lot 1 sont implémentés sur SeaORM, dans des tables
    `miryad_lymaille_*`, par une migration interne gardée par la feature, avec un propriétaire par objet (`FR-8`). L'ensemble exact des traits est qualifié à la spec
    du lot 1 (`OI-2`) ; les stores mémoire de `lymaille` ne servent qu'aux tests (`DEC-1`).
    - **Source:** `DEC-1` ; user instruction, 2026-11-02
    - **Verified by:** `AC-4`

- **FR-4 - MUST:** La session persiste le `subject` de son utilisateur et rien de plus de son identité. À chaque appel d'un
    module de l'hôte, l'`AuthPrincipal` est reconstruit depuis la table des utilisateurs (groupes à jour). Un module
    d'hôte réservé à l'admin n'est accordé, et n'est exécuté, que si ce principal est admin **au moment de l'appel** ; la
    nature de la session (`Work` ou `Config`) n'ajoute ni n'ôte aucun droit sur les ressources. Un utilisateur qui perd
    un groupe voit l'appel suivant refusé.
    - **Source:** `DEC-2`, `DEC-3` ; user instruction, 2026-11-02
    - **Verified by:** `AC-5`

- **FR-5 - MUST:** Avec la feature `lymaille`, le montage de l'état installe le crypto provider rustls de façon
    idempotente avant tout appel `rig` ou MCP, et ignore l'échec si un provider est déjà installé.
    - **Source:** `DEC-4` ; Observed in: `lymaille/src/agents/rig.rs`, `lymaille/src/modules/mcp_client.rs`, 2026-11-01
    - **Verified by:** `AC-6`

- **FR-6 - MUST:** Un kind de workflow `subagent` (nom provisoire) exécute un sous-agent en run batch autonome et rend sa
    sortie structurée comme sortie de step (`agent-sessions#DEC-4`). Le run s'exécute **dans un seul `ctx.run`** sur une
    session de nom déterministe, dérivé de la clé du workflow et de l'identifiant du step ; à un rejeu, la session
    sauvegardée est reprise (`resume` de `lymaille`), jamais recommencée. Le step tourne sous le principal du
    déclencheur (`FR-10`) et porte un plafond de durée `max_secs` dans son `config` (défaut 480 s), appliqué par l'annulation
    coopérative de `lymaille` (`CancelHandle`), strictement inférieur à la borne `ctx.run` du service (`OI-4`) : l'agent s'arrête
    de lui-même au lieu d'être coupé en plein tour. Un échec du run est un `StepError` ; le contenu des sessions et des secrets n'y figure jamais.
    - **Source:** `agent-sessions#DEC-4` ; `DEC-10`, `DEC-11` ; user instruction, 2026-11-02
    - **Verified by:** `AC-10`

- **FR-7 - SHOULD:** Un kind de workflow `pad` porte un pad Rhai par workflow, sur les exigences `PW-1` à `PW-5` de
    `lymaille`, distinct du kind `rhai` (qui reste inchangé).
    - **Source:** `miryad-core/workflow#OI-8` ; `lymaille/dsd/agent-sessions/scratchpad.md`, « Extension réservée : pad de
      workflow » ; user instruction, 2026-10-31
    - **Verified by:** hors lot 1 : bloqué par `OI-3`.

- **FR-8 - MUST:** Les définitions d'agent, toolsets, skills, brouillons et sessions ont un propriétaire (le `subject`).
    Un utilisateur ne lit ni ne modifie que les siens, plus le **catalogue partagé** : des définitions, toolsets et skills
    publiés par un admin, lisibles et utilisables par tous, modifiables par les seuls admins. Tout identifiant du catalogue porte le préfixe réservé `G_` (`DEC-9`). Une session de configuration
    ne lit que les sessions passées de son propriétaire, et n'inspecte que les agents et contextes qu'il peut lire.
    - **Source:** `DEC-7` ; user instruction, 2026-11-02
    - **Verified by:** `AC-7`

- **FR-9 - MUST:** Un utilisateur non-admin peut ouvrir une session de configuration, et c'est le seul mode où il écrit
    des sous-agents, toolsets et skills. L'activation de ses propres définitions est un self-service ; publier au catalogue
    partagé exige un admin. Une définition ne peut accorder, à l'agent ou à ses sous-agents, que des modules que son
    propriétaire pourrait lui-même utiliser ; le refus a lieu à la sauvegarde et, en défense en profondeur, à l'appel
    (`FR-4`).
    - **Source:** `DEC-2`, `DEC-8` ; user instruction, 2026-11-02
    - **Lot :** la session de configuration interactive est livrée au lot 2 (`DEC-13`) ; le contrôle de droits à la sauvegarde d'une
      définition (`DEC-14`) l'est dès le lot 1.
    - **Verified by:** `AC-8` (lot 2)

- **FR-10 - MUST:** `RunInfo` transporte l'identité du déclencheur (`trigger`, `subject` optionnel, porté par le corps `RunInput` comme
    `vars`, pas par un en-tête), posée par `trigger_run` et héritée par les runs enfants. Un step qui exige un principal (`subagent`) sans identité de
    déclencheur échoue en `StepError` non retryable, sans repli sur un principal par défaut.
    - **Source:** `DEC-12` ; `src/workflow/step.sdd`, case « Identité du déclencheur dans RunInfo » (reportée le 2026-10-04) ;
      user instruction, 2026-11-02
    - **Realized by:** `SpecDD /src/workflow/step.sdd` (T1), `/src/workflow/template.sdd` (T2), `/src/workflow/dispatcher.sdd` (T3),
      `/src/workflow/interpreter.sdd` (T4), `/src/workflow/client.sdd` (T5), `/src/workflow/durable.sdd` et
      `/src/workflow/subworkflow.sdd` (T6), amendées le 2026-11-02 et approuvées par Sébastien ce jour — **livré le 2026-11-02**
      (commits `667518c` → `1de97e6`, une tâche par commit, validator GO à chaque tâche et batterie complète à la clôture)
    - **Verified by:** `AC-11`

## Design Decisions

- **DEC-1 - Le stockage des objets de lymaille est porté par la BDD de miryad-core**
    - **Context:** lymaille laisse ses stores à l'hôte ; l'alternative (stores mémoire de la librairie) fragmenterait
      persistance et sauvegarde.
    - **Decision:** miryad-core implémente les traits de store sur SeaORM ; activer `lymaille` active `db`.
    - **Consequences:** migrations internes dédiées (préfixe `miryad_lymaille_`, `DEC-5`) ; les stores mémoire de lymaille restent
      cantonnés aux tests.
    - **Source:** user instruction, 2026-11-01

- **DEC-2 - Deux axes indépendants : nature de session et principal** (amendé le 2026-11-02)
    - **Context:** lymaille distingue sessions `Config` et `Work` ; la première version de cette décision liait les
      ressources admin aux sessions de configuration. Or un utilisateur non-admin doit pouvoir configurer ses sous-agents
      et ses skills, ce qui n'est possible qu'en mode configuration.
    - **Decision:** la **nature** de la session décide de ce que l'agent peut faire de sa propre configuration (brouillons
      de toolsets et skills, définitions d'agents, lecture des sessions passées et inspection : `BR-8` de lymaille) ; le
      **principal** décide de ce qu'il peut faire des ressources, par les politiques de `MiryadResource`. Les modules
      d'hôte réservés à l'admin ne sont accordés qu'à un principal admin, dans les deux natures.
    - **Alternatives:** ressources admin réservées aux sessions de configuration (écarté : interdit la configuration aux
      non-admins et mêle privilège et mode de travail).
    - **Consequences:** `DEC-3` n'a plus de revérification spécifique à la configuration ; le cloisonnement par
      propriétaire (`DEC-7`) devient une exigence des stores.
    - **Source:** user instruction, 2026-11-01 ; amendement, user instruction, 2026-11-02

- **DEC-3 - La session ne persiste que le `subject`**
    - **Context:** une session est durable, ses utilisateurs changent de groupes ; un principal figé conserverait des
      droits retirés.
    - **Decision:** liaison session ↔ utilisateur par le `subject` seul ; principal reconstruit à chaque appel, donc droits
      (admin compris) toujours à jour.
    - **Alternatives:** instantané du principal à l'ouverture (écarté : droits périmés dans les sessions longues).
    - **Consequences:** un accès à la table des utilisateurs par appel de module ; un `subject` sans compte local donne un
      refus, pas un principal vide.
    - **Source:** user instruction, 2026-11-02

- **DEC-4 - Le provider rustls est installé par miryad-core, de façon idempotente**
    - **Context:** `ring` et `aws-lc-rs` sont tous deux dans le graphe (Observed in: `Cargo.lock`, 2026-11-02), donc rustls
      0.23 ne choisit pas seul ; `lymaille` avorte sans provider.
    - **Decision:** le montage de l'état installe un provider si aucun ne l'est, sans échec si un autre l'a déjà fait.
      **Arbitré le 2026-11-02 : `ring`** (plus léger, sans toolchain C/cmake ; `ring` et `aws-lc-rs` sont tous deux tirés par rustls 0.23.43 via
      `reqwest`/`matrix-sdk`, Observed in: `cargo tree --all-features -e normal -i ring`, 2026-11-02, donc aucune dépendance ajoutée).
    - **Alternatives:** laisser le template `miryad` s'en charger (écarté : un hôte qui oublie fait avorter le processus ;
      contraire à l'autoconfiguration de Kydah).
    - **Source:** user instruction, 2026-11-02

- **DEC-5 - Conventions de nommage : `miryad_lymaille_*` et `MRD-LYMAILLE-NNN`**
    - **Decision:** tables préfixées `miryad_lymaille_`, codes d'erreur `MRD-LYMAILLE-NNN`, par analogie avec
      `miryad_matrix_`/`MRD-MATRIX` et `miryad_workflow_`/`MRD-WORKFLOW`.
    - **Consequences:** `miryad-core#CON-3` et `CON-6` listent ces noms au prochain amendement du Domain.
    - **Source:** proposition Claude, 2026-11-02, non contestée par le développeur principal

- **DEC-6 - Lots de réalisation** (amendé le 2026-11-02, `DEC-13`)
    - **Decision:** lot 0 = identité du déclencheur dans `RunInfo` (`FR-10`, `DEC-12`, côté `workflow`). Lot 1 = socle (feature,
      dépendance, rustls, migration, stores de l'exécution), définitions en `MiryadResource` (`DEC-14`), module des ressources en
      lecture seule (`DEC-15`) et kind `subagent`, en batch. **Lot 1 livré d'un bloc (arbitré le 2026-11-02 : l'exécution est
      déléguée à un modèle dédié, la durée n'est pas une contrainte).** Lot 2 = sessions interactives et de configuration avec **leurs deux
      surfaces, REST+SSE et Matrix** (arbitré le 2026-11-02, specs indépendantes par canal), glue `matrix` (`OI-5`, `DEC-23`), écritures du module des ressources (`DEC-21`), contextes et serveurs MCP (`DEC-20`), rétention (`DEC-22`) ; **un seul lot, sans sous-découpage (2026-11-03)**. Lot 3 = kind `pad`, après une release de `lymaille` qui livre
      `PW-1` à `PW-5` (`OI-3`).
    - **Source:** user instruction, 2026-11-02

- **DEC-7 - Privé par utilisateur, plus un catalogue partagé administré**
    - **Context:** les traits `AgentStore`, `ToolsetStore`, `ContextStore` et `DraftStore` de `lymaille` indexent par
      identifiant, sans propriétaire (Observed in: `lymaille/src/agents/catalog.rs`, 2026-11-02) ; le cloisonnement est donc
      à la charge de l'hôte.
    - **Decision:** chaque objet a un propriétaire (le `subject`) ; un catalogue partagé, écrit par les admins, est lisible
      par tous.
    - **Alternatives:** tout privé sans catalogue (écarté : un admin ne distribuerait plus d'agent ni de skill) ; espace
      commun modifiable par tous (écarté : un utilisateur modifierait l'agent d'un autre).
    - **Consequences:** colonne propriétaire et espace de noms par utilisateur dans les tables `miryad_lymaille_*` ; un
      identifiant fourni par le LLM ne sort jamais de l'espace de son propriétaire et du catalogue. Les traits de store ne
      prennent aucun propriétaire en paramètre : les stores de miryad-core sont donc **construits par principal** (un
      store lié à un `subject`), pas partagés. Les collisions de nom sont réglées par `DEC-9`.
    - **Source:** user instruction, 2026-11-02

- **DEC-8 - Activation en self-service, publication par un admin**
    - **Context:** `BR-8` de `lymaille` : un agent de configuration ne peut pas activer un toolset ; l'activation est une
      action de l'hôte.
    - **Decision:** l'hôte active les définitions de leur propriétaire sur demande de celui-ci ; publier au catalogue
      partagé exige un admin.
    - **Alternatives:** validation admin systématique (écartée : un non-admin ne pourrait rien configurer seul).
    - **Consequences:** l'activation est une action de l'hôte hors du tour de l'agent ; elle porte sur des définitions qui ne
      peuvent dépasser les droits du propriétaire (`FR-9`).
    - **Source:** user instruction, 2026-11-02

- **DEC-9 - Préfixe réservé `G_` pour le catalogue partagé**
    - **Context:** sans règle, un admin qui publie un agent du même nom que celui d'un utilisateur, ou l'inverse, crée un
      faux-ami dans l'un des deux sens ; interdire à l'utilisateur de créer un nom déjà public ne couvre pas la publication
      postérieure.
    - **Decision:** tout identifiant du catalogue (agents, toolsets) commence par `G_` ; aucun utilisateur, admin compris
      dans son espace privé, ne crée un identifiant commençant par `G_`, sans tenir compte de la casse. Seuls les admins
      écrivent un identifiant `G_…`, dans le catalogue. La résolution d'un identifiant est sans ambiguïté : `G_…` va au
      catalogue, le reste à l'espace du propriétaire. Les skills suivent leur toolset.
    - **Alternatives:** refus de création quand un nom public existe (écarté : ne couvre que le sens
      utilisateur-après-public) ; masquage du public par le privé (écarté : faux-ami silencieux) ; préfixe `G-` (écarté :
      le tiret n'est pas un caractère valide d'un identifiant Rhai, donc d'un nom de toolset ou de module — Observed in:
      `lymaille/src/toolsets/toolset.rs`, `lymaille/src/scratchpad/grant.rs`, 2026-11-02).
    - **Consequences:** collision impossible dans les deux sens, sans test d'existence ; le contrôle est un test de préfixe à
      l'écriture (`FR-8`), au même endroit que le contrôle de droits ; les références d'une définition (sous-agents
      délégables, toolsets accordés) se lisent sans ambiguïté.
    - **Source:** user instruction, 2026-11-02 (principe « pas de faux-amis », préfixe réservé) ; Observed in: `lymaille`, 2026-11-02

- **DEC-10 - Un `ctx.run` par run de sous-agent, session de nom déterministe**
    - **Context:** un run LLM n'est pas déterministe, et `lymaille` persiste déjà la session par tour dans le
      `SessionStore` ; rejouer la fermeture depuis zéro paierait l'inférence deux fois et changerait de chemin.
    - **Decision:** le run entier est la fermeture d'un `ctx.run` ; l'identifiant de session se dérive de (clé du workflow,
      identifiant du step), donc identique à chaque tentative ; un rejeu retrouve la session sauvegardée et la reprend. Le
      résultat se journalise une fois, à la fin.
    - **Alternatives:** un `ctx.run` par tour (écarté : couple le kind à l'API de tour de `lymaille` et alourdit le journal) ;
      lancement détaché et awakeable (écarté : exige l'awakeable reporté et une reprise au démarrage du processus).
    - **Consequences:** la durée d'un run est bornée par celle d'une fermeture `ctx.run`, soit ≈ 11 min par défaut, réglable (spike
      `OI-4`) ; le kind est ordinaire
      (`MiryadWorkflowStep`), pas durable, tant que cette borne convient ; une session de step est une session `Sub`.
    - **Source:** user instruction, 2026-11-02

- **DEC-11 - Effets de bord de l'agent : au moins une fois, sur le seul tour interrompu**
    - **Context:** les tours terminés sont dans l'historique de session ; seul le tour en cours au moment du crash peut
      refaire ses appels de module.
    - **Decision:** accepté et documenté ; le step est `retryable: false` (`NFR-2` : il n'est pas idempotent). La spec le
      dit en toutes lettres.
    - **Alternatives:** modules en lecture seule dans un step de workflow (écarté : bride l'agent) ; clé d'idempotence sur
      chaque écriture (écarté pour le lot 1 : travail propre dans les politiques d'écriture, à rouvrir si un incident le
      justifie).
    - **Consequences:** une écriture d'agent doit pouvoir être refaite sans dommage, ou le DAG l'enchaîne dans un step
      ordinaire idempotent ; le principal doit le savoir en écrivant son DAG.
    - **Source:** user instruction, 2026-11-02

- **DEC-12 - Le step `subagent` tourne sous l'identité du déclencheur**
    - **Context:** `FR-4` exige un principal par appel de module ; `RunInfo` n'en porte aucun.
    - **Decision:** lever le report de `step.sdd` : `trigger_run` transporte le `subject` du déclencheur, hérité par les runs
      enfants ; l'agent agit avec les droits de celui qui a lancé le run, jamais plus.
    - **Alternatives:** propriétaire de la définition d'agent (écarté : un agent du catalogue `G_` n'a pas de propriétaire
      utilisateur, et un run lancé par A agirait avec les droits de B) ; compte de service nommé dans le `config` du step
      (écarté : à maintenir à la main, le déclencheur pourrait élever ses droits par le DAG).
    - **Consequences:** amendement de `step.sdd`, `client.sdd`, `interpreter.sdd` et `subworkflow.sdd` avant le step
      `subagent` ; les droits de l'enfant d'un sous-workflow (case reportée de `subworkflow.sdd`) se règlent du même coup.
    - **Source:** user instruction, 2026-11-02

- **DEC-13 - Le lot 1 est batch seul ; les sessions interactives viennent avec leur surface**
    - **Context:** une session de configuration est interactive et réclame une surface de conversation (routes, flux ou
      Matrix) que le step `subagent` n'exige pas.
    - **Decision:** lot 1 = step `subagent` en batch, module des ressources, stores de l'exécution. Lot 2 = sessions
      interactives et de configuration, avec leur surface et le glue Matrix (`OI-5`).
    - **Alternatives:** une surface REST/SSE dès le lot 1 (écarté : double le lot et touche `rest-api`).
    - **Consequences:** `DraftStore` et `SessionIndex` attendent le lot 2 ; `AC-8` aussi.
    - **Source:** user instruction, 2026-11-02

- **DEC-14 - Les définitions administrées sont des `MiryadResource`** (amendé le 2026-11-02)
    - **Context:** sans session de configuration, il faut un moyen de créer et de modifier agents, toolsets et profils ; la
      crate sait déjà exposer des entités en REST et MCP avec propriétaire, politiques et hooks.
    - **Decision:** seules les tables qu'un admin administre à la main au lot 1 sont des `MiryadResource` : **agents, toolsets, profils et
      providers**. CRUD REST et MCP par le moteur existant ; le préfixe `G_` (`DEC-9`) et la validation de la définition sont contrôlés par
      les hooks `before_create`/`before_update`. Les stores de `lymaille` lisent ces mêmes tables. Les autres tables (sessions, mémoires,
      types de contexte, contextes) sont **internes**, sans CRUD.
    - **Amendement 2026-11-02 (remarque du développeur principal : « est-ce utile pour tout ? ») :** au lot 1 seuls les admins écrivent
      (`FR-9` : un non-admin n'écrit des définitions qu'en session de configuration, lot 2). Les lignes `MiryadResource` du lot 1 sont donc
      toutes du **catalogue** : `owner_column() = None`, politique de lecture `Public` (agents, toolsets, profils ; `AdminOnly` pour les
      providers) et d'écriture `AdminOnly`, identifiant `G_…` imposé par hook. L'expression « privé + catalogue » est tranchée par
      `DEC-19` : une paire de ressources et deux tables par type, sans toucher au moteur d'accès ; les entités privées arrivent au lot 2.
    - **Alternatives:** stores seuls, définitions fournies par l'application (écarté : `DEC-7` à `DEC-9` n'auraient aucun point
      d'application dans la crate).
    - **Consequences:** la validation d'une définition (`AgentDefinition::validate`, droits du propriétaire, `FR-9`) vit dans
      les hooks ; les tables internes de l'exécution (sessions, mémoires, types de contexte, contextes) restent hors du CRUD.
    - **Source:** user instruction, 2026-11-02

- **DEC-15 - Module des ressources générique, en lecture seule au lot 1**
    - **Decision:** un seul module Rhai, ressource en argument (`describe()`, `list(<ressource>, <filtre>)`,
      `get(<ressource>, <id>)`), dont le contexte initial est constant (`agent-sessions#BO-2`) ; les ressources visibles sont
      celles que le principal peut lire. Les écritures (`create`, `update`, `delete`) sont hors lot 1.
    - **Alternatives:** un module par ressource (écarté : le contexte croît avec l'application) ; écriture dès le lot 1 (écarté :
      exposerait l'effet de bord rejoué de `DEC-11` à des écritures réelles).
    - **Consequences:** `AC-3` ne vise que la lecture au lot 1 ; les écritures reprendront `write_policy`, `owner_column` et hooks.
    - **Source:** user instruction, 2026-11-02

- **DEC-16 - Les références d'authentification des providers sont résolues par un trait de l'hôte, défaut environnement**
    - **Context:** `Provider` ne porte que des références (`auth`), jamais de secret ; lymaille ne les résout pas, et le secret ne doit
      figurer ni en base, ni en erreur, ni en log, ni dans une session.
    - **Decision:** un trait `SecretResolver` (nom provisoire) fourni par l'application au montage de l'état ; l'implémentation par défaut
      lit une variable d'environnement dont le nom est la référence. Le secret n'est résolu qu'à la construction du client d'inférence, vit
      dans un `Zeroizing`, et ne traverse aucune erreur.
    - **Alternatives:** variable d'environnement seule, sans trait (écarté : ferme la porte à Vault ou aux fichiers montés) ; chemin de
      fichier (écarté : convention moins courante pour une clé d'API).
    - **Consequences:** une référence introuvable est une erreur `MRD-LYMAILLE-*` qui ne cite pas la valeur ; spec `src/lymaille/secret.sdd`.
    - **Source:** user instruction, 2026-11-02

- **DEC-17 - Définitions stockées en colonnes clés plus colonnes JSON**
    - **Context:** `AgentDefinition` et consorts portent de l'imbriqué (modules, toolsets, sampling, sandbox, délégables, schéma de sortie) ;
      REST et MCP filtrent sur des colonnes typées ; les traits relèvent par identifiant.
    - **Decision:** chaque table de définition porte des colonnes typées pour ce qui se filtre ou se contrôle (identifiant, propriétaire,
      nature, nom de profil…) et des colonnes JSON pour l'imbriqué. Un objet lymaille est reconstruit depuis les colonnes et le JSON, et
      repasse par sa validation.
    - **Alternatives:** tout normalisé (écarté : lourd, et chaque champ ajouté par lymaille impose une migration) ; document JSON unique
      (écarté : REST/MCP ne valident ni ne filtrent l'interne).
    - **Consequences:** la migration interne reste stable quand lymaille ajoute un champ optionnel ; le JSON stocké est celui de lymaille
      (`non_exhaustive`, tolérant).
    - **Source:** user instruction, 2026-11-02

- **DEC-18 - Toolset : source active et brouillon dans la même ligne**
    - **Context:** `ToolsetStore` distingue version active et brouillon ; l'activation est une action de l'hôte (`DEC-8`) ; au lot 1 les
      toolsets s'écrivent par CRUD (`DEC-14`), sans session de configuration.
    - **Decision:** la ligne `MiryadResource` du toolset porte la source active et une source brouillon nullable. `put_draft` écrit le
      brouillon ; `get_active` ne lit jamais le brouillon ; l'activation (self-service du propriétaire, admin pour `G_…`) copie le brouillon
      dans l'actif par une action contrôlée par hook. Le lot 2 branche `DraftStore` sur le même stockage.
    - **Alternatives:** table de brouillons séparée (écarté : l'activation devient une opération inter-tables sans surface CRUD) ; version
      active seule (écarté : perd la garantie que le LLM ne voit jamais un brouillon).
    - **Consequences:** `get_active`/`get_draft` se lisent sur une seule ligne. **Précision du 2026-11-02 :** au lot 1 l'activation est l'écriture de
      `active` par un admin via l'entité (`DEC-14`) ; l'activation self-service du propriétaire et le contrôle de hook associé arrivent avec le lot 2.
    - **Source:** user instruction, 2026-11-02

- **DEC-19 - Catalogue et privé : une paire de ressources, deux tables, aucune union**
    - **Context:** `AccessPolicy` est statique par entité ; « mes définitions + le catalogue » ne s'y exprime pas (une ligne sans propriétaire
      est refusée hors admin sous `OwnerOnly`). Une vue union n'a pas de sens : elle ignore l'appelant, et sa variante « catalogue × utilisateurs »
      donne N copies de chaque entrée (clé primaire à synthétiser, coût en nombre d'utilisateurs).
    - **Decision:** agents et toolsets existent en deux tables, `miryad_lymaille_catalog_*` (catalogue `G_…`, `AdminOnly` en écriture, `Public` en
      lecture) et `miryad_lymaille_*` (privé, `owner_id NOT NULL`, `OwnerOnly`), chacune exposée par sa propre `MiryadResource` aux droits
      distincts. Le préfixe `G_` (`DEC-9`) décide de la table : une résolution est une requête sur une table, jamais une union. À l'API REST/MCP,
      ce sont deux ressources distinctes.
    - **Alternatives:** variante d'`AccessPolicy` « propriétaire ou sans propriétaire » (écarté : change le moteur d'accès pour toutes les
      ressources) ; vue union (écarté, ci-dessus).
    - **Consequences:** unicité par simples contraintes (`UNIQUE(agent_id)` au catalogue, `UNIQUE(owner_id, agent_id)` au privé, plus d'index
      partiel) ; les tables privées existent dès le lot 1 (les stores les écrivent) mais leurs entités arrivent au lot 2.
    - **Source:** user instruction, 2026-11-02 (paire de ressources) ; vue écartée par le développeur principal le 2026-11-02

- **DEC-20 - Types de contexte, contextes et serveurs MCP : qui écrit, qui peut lier** (2026-11-03, séance de questions)
    - **Context:** `OI-9`. Un toolset à rôles MCP exige un contexte ; un contexte lie un rôle à un serveur MCP ; `lymaille` 0.1.0 ne fournit aucun
      store de définitions de serveur (`McpClientConfig` = URL + délais, `McpAuth` = en-têtes, tous deux de l'hôte).
    - **Decision:** (1) **types de contexte** : `MiryadResource` du catalogue, lecture `Public`, écriture `AdminOnly`. (2) **contextes** : paire de
      ressources comme `DEC-19` ; niveau `User` = `OwnerOnly`, niveau `Project` (= partagé par toute l'instance : miryad-core n'a pas d'entité
      projet) = catalogue `AdminOnly` ; niveau `Session` interne, sans CRUD. (3) **`mcp_servers`** : `MiryadResource` admin (nom, URL, délais,
      `auth_ref`, mode d'auth, `allowed_groups`) ; la référence d'auth est résolue par le `SecretResolver` de `DEC-16`, le secret n'est jamais en base.
      (4) **droit d'usage** : `allowed_groups` liste des noms de groupes, plus les valeurs réservées `all` (tout authentifié) et `admin` ; un hook
      refuse à la sauvegarde d'un contexte une liaison de rôle vers un serveur non autorisé à son propriétaire, et l'appel revalide
      (défense en profondeur, `FR-9`) ; l'appartenance est relue en base à chaque appel, jamais mise en cache. (5) **modes d'auth** : `static`
      (référence de secret, compte de service) et `forward` (le serveur reçoit le jeton de l'utilisateur) ; `forward` n'est valable qu'en
      session interactive, avec le jeton frais de la requête, jamais persisté ; un step batch qui lie un serveur `forward` échoue en `StepError`
      non retryable.
    - **Alternatives:** serveurs en configuration d'application (écarté : pas de modification à chaud, la session de configuration n'y touche pas) ;
      `forward` en batch par échange de jeton RFC 8693 (écarté : dépend de l'IdP, hors périmètre ; à rouvrir) ou par refresh token stocké (écarté :
      secret longue durée au repos) ; contextes `User` à liaison libre (écarté : un utilisateur se brancherait sur un serveur à compte de service puissant).
    - **Consequences:** `mcp_servers`, `context_types`, contextes User et Project sont des ressources du lot 2 ; la session de configuration
      peut écrire des contextes `User` sous les mêmes hooks. Voir `DEC-21` pour l'écriture par l'agent.
    - **Source:** user instruction, 2026-11-03

- **DEC-21 - Écritures du module des ressources : aussi en batch, idempotentes** (2026-11-03, amende `DEC-11` et `DEC-15`)
    - **Context:** `DEC-15` limitait le module à la lecture ; `DEC-11` rejoue un step batch.
    - **Decision:** `create`, `update` et `delete` existent aussi dans le module du step batch, avec les mêmes `write_policy`, `owner_column` et
      hooks que REST et MCP (parité, `FR-1`), sous l'identité du déclencheur. Idempotence obligatoire : une clé d'idempotence dérivée du run, du
      step et du rang de l'appel d'outil dans la session restaurée.
    - **Pitfall (signalé par Claude, à traiter en spec) :** au rejeu le modèle peut émettre d'autres appels que la première fois ; le rang d'appel
      ne garantit donc pas l'identité d'une opération. À spécifier : `update`/`delete` naturellement idempotents ; `create` exige un identifiant
      fourni (ou dérivé du contenu) et un échec de doublon traité comme un succès si le contenu est identique ; journal des écritures du step pour
      qu'une reprise restaurée ne les refasse pas. Une écriture dont le `subject` du déclencheur manque échoue (`FR-10`).
    - **Consequences:** `resource_module.sdd` (lot 1) est amendé, et `DEC-11` doit être relu ; tests de rejeu contre `restate-server`.
    - **Source:** user instruction, 2026-11-03

- **DEC-22 - Rétention des sessions : durée configurable et fonction de purge** (2026-11-03, solde `OI-7`)
    - **Decision:** une durée de rétention fournie par l'application (défaut proposé : 30 jours après la dernière activité) et une fonction
      `purge_sessions(db, now)` livrée par la crate, que l'application planifie ; aucun couplage au cycle de vie des workflows. Livrée au lot 2.
    - **Source:** user instruction, 2026-11-03

- **DEC-23 - Annuaire mxid → principal : liaison explicite à code à usage unique** (2026-11-03, solde `OI-5` pour la liaison)
    - **Decision:** l'utilisateur, connecté en OIDC, génère un code à usage unique et le renvoie au bot par Matrix ; la liaison mxid↔subject est
      stockée dans une table dédiée. Un mxid non lié n'a aucun droit. Une revendication OIDC portant le mxid pourra compléter la liaison plus tard.
    - **Source:** user instruction, 2026-11-03

## Contour du lot 2

Le lot 2 est **un seul lot, sans sous-découpage** (arbitré le 2026-11-03 : la spec Matrix n'a pas besoin du code REST). Il est **posé, non spécifié** :
aucune tâche n'est ouverte, aucune spec n'est écrite. Périmètre (`DEC-6`, `DEC-13`, `DEC-20` à `DEC-23`) :

- **Socle commun d'une session interactive** : ouvrir (nature `Work` ou `Config`), envoyer un message, suivre les événements
  (`EventSink`), annuler, reprendre. Indépendant du canal.
- **Surface REST + SSE** : routes de conversation et flux d'événements. Ces routes ne sont pas des `MiryadResource` ; elles sont **exemptées de la
  règle de parité des ressources, comme `/auth/*`** (acté le 2026-11-03).
- **Surface Matrix** : mapping salle ↔ session, dédoublonnage des messages remis deux fois après bascule de bail, mise en forme des sorties longues,
  annuaire mxid → principal (`DEC-23`).
- **Session de configuration** (`FR-9`, `AC-8`) : `DraftStore` et `SessionIndex` branchés sur le stockage du lot 1 (`DEC-18`), activation self-service.
- **Définitions privées** : entités `OwnerOnly` des tables privées `agents` et `toolsets` (`DEC-19`), hooks du préfixe réservé et du contrôle des
  droits d'accord de modules du propriétaire (`FR-9`).
- **Types de contexte, contextes, serveurs MCP** (`DEC-20`, solde `OI-9`).
- **Écritures du module des ressources**, interactives et batch idempotentes (`DEC-21`).
- **Rétention des sessions** (`DEC-22`, solde `OI-7`).
- **Hors lot 2 :** kind `pad` (lot 3, `OI-3`) ; `forward` de jeton en batch (`DEC-20`).

## Constraints and Pitfalls

- `lymaille` épingle exactement `rmcp =3.8.0` et `rig-core =0.43.0` : leurs versions dans le graphe du consommateur
  sont dictées par la version de lymaille retenue. Toute montée se fait par la montée de lymaille. (Observed in:
  `lymaille/Cargo.toml`, 2026-11-01.)
- Ajouter la dépendance fait passer le rhai de tout le graphe de 1.26.1 à 1.27.0, `rhai_step` du workflow compris : la
  bascule se prouve par la batterie `workflow` du moment de l'ajout, jamais en silence.
- Les relais de features `fs` et `shell` de lymaille ne doivent jamais être activés par miryad-core : la discipline de
  liste blanche des moteurs Rhai (workflow `DEC-8`) s'étend au pad de lymaille ; à confirmer et à traduire en
  `Scenario` de preuve comme pour `vynil-core` (A1). **ASSUMPTION:** prolongation voulue de DEC-8, à confirmer.

## SpecDD Realization

Specs du lot 0 écrites et livrées le 2026-11-02. **Specs du lot 1 rédigées le 2026-11-02, en attente d'approbation** (`DEC-6`, `DEC-13` à
`DEC-18`), un fichier source = une spec : `/src/lymaille/mod.sdd` (feature et diff `Cargo.toml`, `FR-1`), `error.sdd` (`MRD-LYMAILLE-*`),
`tls.sdd` (`FR-5`), `secret.sdd` (`DEC-16`), `migration.sdd` (`FR-3`), `identity.sdd` (`FR-4`, `FR-8`, `FR-9`), `store/mod.sdd`, `store/session.sdd`,
`store/catalog.sdd`, `store/toolset.sdd`, `store/memory.sdd` (`FR-3`, huit traits), `definitions/mod.sdd`, `definitions/agent.sdd`,
`definitions/toolset.sdd`, `definitions/profile.sdd`, `definitions/provider.sdd` (`DEC-14`, `DEC-17`, `DEC-18`), `resource_module.sdd` (`FR-2`,
lecture seule), `/src/workflow/subagent_step.sdd` (`FR-6`, `FR-10`). Amendements d'accompagnement : `/src/lib.sdd`, `/src/migration/mod.sdd`,
`/src/workflow/mod.sdd`, `/src/auth/principal.sdd` (variante `PrincipalSource::Agent`), `/miryad-core.sdd`, `/tooling.sdd` ; `/AGENTS.md`, la CI et
`workflows.sdd` restent à amender avec accord. Lot 2 : contour posé, aucune spec ni tâche. Lot 3 : `/src/workflow/pad_step.sdd`.

## Acceptance Criteria

- **AC-1:** Un graphe combinant miryad-core (toutes features) et lymaille (path) compile, avec un seul rhai, un seul
  tokio et un seul rustls en `cargo tree`. Vérifié le 2026-11-01 (crate tampon, `cargo check` vert).
- **AC-2:** `cargo tree --no-default-features --features lymaille -e normal -i sea-orm` imprime une ligne (la feature
  implique `db`), et `lymaille` absent du graphe par défaut. À poser avec la première spec.

- **AC-3:** Un utilisateur sans droit de lecture d'une ressource ne la lit pas par le module des ressources (lecture seule au lot 1) ; mêmes refus que REST et MCP pour `read_policy`, `write_policy`, `owner_column` et hooks.
- **AC-4:** Une session sauvegardée par les stores SeaORM puis rechargée est identique, sur SQLite et PostgreSQL.
- **AC-5:** Un utilisateur retiré du groupe admin entre deux appels voit le second appel d'un module réservé à l'admin
  refusé, dans une session `Work` comme `Config` ; un utilisateur non-admin n'a jamais ces modules, quelle que soit la
  nature de sa session.
- **AC-6:** Deux montages d'état successifs dans le même processus réussissent, et un appel MCP de `lymaille` n'avorte pas.

- **AC-7:** Deux utilisateurs ne voient jamais les agents, toolsets, skills, brouillons ni sessions l'un de l'autre ; tous
  deux voient le catalogue partagé, que seul un admin modifie.
- **AC-8 (lot 2) :** Un non-admin ouvre une session de configuration, écrit et active un sous-agent ; sa définition qui accorde un
  module réservé à l'admin est refusée à la sauvegarde. Au lot 1, un non-admin est refusé dès la politique d'écriture `AdminOnly` des définitions ; le hook valide les droits du propriétaire pour les lignes privées du lot 2.

- **AC-9:** Un utilisateur, admin compris, qui crée dans son espace privé un identifiant commençant par `G_` ou `g_` est
  refusé ; un admin qui publie `G_x` alors qu'un utilisateur a un agent `x` réussit, et l'utilisateur voit les deux sans
  confusion possible.

- **AC-10:** Un step `subagent` dont le processus est tué en milieu de run reprend sur la session sauvegardée au rejeu : le
  nombre de tours déjà terminés n'est pas refait, et le résultat est journalisé une seule fois. Preuve par la méthode du
  spike de `workflow#AC-8` (non verrouillée par la CI).
- **AC-11:** Un run déclenché par un utilisateur A donne à ses steps et à ses runs enfants le `subject` de A ; un run sans
  identité fait échouer un step `subagent` en `StepError` non retryable.
  - État 2026-11-02 (lot 0) : première moitié prouvée (tests nommés de T1–T6 et intégration `subworkflow_lenfant_hérite_du_trigger_du_parent` rejouée contre `restate-server`) ; le step `subagent` est du lot 1 (`DEC-13`) et verrouillera la seconde.

## Open Issues

- **OI-1:** **Soldé le 2026-11-02 (user instruction)** : garder les défauts de `lymaille` (`mcp-http`, `rig`), nécessaires au lot 1
  (inférence et MCP distants), sans les relayer en sous-features miryad tant qu'aucun hôte ne les désactive ; `fs`/`shell` : jamais
  (Constraints, avec `Scenario` de preuve type A1).
- **OI-2:** **Stores du lot 1** (`DEC-13`) : huit traits, `SessionStore`, `AgentStore`, `ProfileStore`, `ProviderStore`,
  `ToolsetStore`, `ContextTypeStore`, `ContextStore`, `MemoryStore`, cloisonnés par propriétaire (`DEC-7`), stores construits
  par principal. `DraftStore`, `SessionIndex` : lot 2. **Arbitré le 2026-11-02 (user instruction), révisé le même jour
  (`DEC-14`) :** agents, toolsets, profils et providers sont des `MiryadResource` (providers **réservés aux admins** : `ProviderLevel`,
  références, jamais de secret, Observed in: `lymaille/src/agents/provider.rs`, 2026-11-02) ; sessions, mémoires, types de contexte et
  contextes restent internes, hors CRUD (aucune surface ne les écrit au lot 1, `OI-9`).
- **OI-3:** **Pad de workflow** : `PW-1` à `PW-5` ne sont pas dans `lymaille` 0.1.0. À faire côté `lymaille` d'abord
  (nature `Workflow` de `SessionState`, cycle de vie sans tour, instantané JSON). Le kind `pad` (`FR-7`) attend cette
  release. **TBD :** versionnage de la demande côté `lymaille`.
- **OI-4:** **Soldé le 2026-11-02 par spike** (durée d'un run dans un `ctx.run`, `DEC-10`). Banc : `tests/workflow_restate.rs`
  (vrai `restate-server`, image `latest` du 2026-09-28, `restate-sdk` 0.12.1), un step ordinaire qui dort dans son unique
  `ctx.run`, réglages par défaut sauf mention :

  | Durée du step | Réglage | Résultat |
  |---|---|---|
  | 30 s, 2 min, 5 min | défaut | terminé, un seul `start`, aucun rejeu |
  | 12 min | défaut | **coupé** : « the invocation stream was closed after the 'abort timeout' (10m) fired », relance (`RT0001`), le step repart de zéro |
  | 90 s | `inactivity_timeout` 10 s, `abort_timeout` 20 s | coupé à 20 s après l'inactivité, relances en boucle : le réglage s'applique |
  | 12 min | `inactivity_timeout` 600 s, `abort_timeout` 900 s | terminé en 12 min, un seul `start`, aucun rejeu |

  Conclusions : (1) la borne par défaut d'un `ctx.run` est d'environ 11 minutes (coupure observée à ≈ 11 min après le début ; le
  message de Restate cite un `abort timeout` de 10 minutes) ; (2) elle se règle par service ou par handler
  (`ServiceOptions::inactivity_timeout` / `abort_timeout`, Observed in: `restate-sdk-0.12.1/src/endpoint/builder.rs`) ; (3) au-delà,
  Restate relance la fermeture **depuis zéro**, ce qui confirme le besoin de reprise sur session déterministe (`DEC-10`) ; (4) le
  réglage vit sur le service `StepDispatcher`, donc il s'applique à tous les kinds : le relever retarde aussi la détection d'un
  step ordinaire bloqué. **Décidé dans `subagent_step.sdd` (en attente d'approbation) :** `recommended_options()` inchangé au lot 1 ; `max_secs` plafonné à 540 s, défaut 480 s.
    - **Source:** spike du 2026-11-02, résultats ci-dessus ; user instruction, 2026-11-02
- **OI-5:** **Glue Matrix** (`matrix` + `lymaille`) : mapping salle ↔ session, dédoublonnage des messages remis deux
  fois après bascule de bail, mise en forme des sorties longues, annuaire mxid → principal (inexistant :
  `miryad_matrix_tracked_users` est le store crypto E2EE du bot, pas un annuaire). **TBD** — lot 2.
  **Annuaire soldé le 2026-11-03 : `DEC-23`.** Restent à spécifier : mapping salle ↔ session, dédoublonnage, sorties longues.
- **OI-6:** **Contrat du module des ressources** (`DEC-15`) : forme de `describe()` (champs, types, filtres permis), syntaxe du
  `<filtre>` de `list` (celle de `src/query.rs`), pagination et plafond de taille d'une réponse au LLM (rendu par
  `src/mcp/format.rs` ?). **TBD** à la spec `resource_module.sdd`.

- **OI-9:** **Soldé le 2026-11-03 (user instruction)** : `DEC-20`. Reste hors lot 2 : `forward` de jeton en batch (échange RFC 8693 à rouvrir).
- **OI-7:** **Soldé le 2026-11-03 (user instruction)** : `DEC-22` (durée configurable et `purge_sessions`, lot 2).
- **OI-8:** **Fingerprint et reprise** : `restore_session` compare la `Provenance` sauvegardée à l'actuelle (empreinte de la définition,
  versions de toolsets) et rend des avis si elles diffèrent. Si l'agent est modifié entre deux tentatives d'un step, la reprise continue
  avec ces avis. **Tranché provisoirement dans `subagent_step.sdd` (en attente d'approbation) :** la reprise continue avec les avis de `lymaille`, sans échec sur empreinte différente.

## References

- Dépôt `lymaille` (`sebt3/lymaille`) et sa bibliothèque DSD `agent-sessions` (domaine de l'agent LLM lui-même).
- `miryad-core/workflow#OI-8` (pad de workflow réservé) et l'« Extension réservée : pad de workflow » de
  `lymaille/dsd/agent-sessions/scratchpad.md` (PW-1 à PW-3).
- Exploration d'intégrabilité du 2026-11-01 (graphes combinés, harness lymaille vert : 1327 tests, clippy propre).
