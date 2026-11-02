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

- **Actuel :** rien dans la crate ; aucune spec. La feature était réservée par `miryad-core/workflow#OI-8`. `lymaille`
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
    déclencheur (`FR-10`). Un échec du run est un `StepError` ; le contenu des sessions et des secrets n'y figure jamais.
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
    - **Verified by:** `AC-8`

- **FR-10 - MUST:** `RunInfo` transporte l'identité du déclencheur (`subject`, optionnel, en-tête Restate comme `depth`), posée
    par `trigger_run` et héritée par les runs enfants. Un step qui exige un principal (`subagent`) sans identité de
    déclencheur échoue en `StepError` non retryable, sans repli sur un principal par défaut.
    - **Source:** `DEC-12` ; `src/workflow/step.sdd`, case « Identité du déclencheur dans RunInfo » (reportée le 2026-10-04) ;
      user instruction, 2026-11-02
    - **Realized by:** aucune spec à ce jour ; `SpecDD /src/workflow/step.sdd`, `/src/workflow/client.sdd`,
      `/src/workflow/interpreter.sdd` et `/src/workflow/subworkflow.sdd` à amender, avant le step `subagent`
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
      Le choix du provider (`ring` ou `aws-lc-rs`) est fixé à la spec.
    - **Alternatives:** laisser le template `miryad` s'en charger (écarté : un hôte qui oublie fait avorter le processus ;
      contraire à l'autoconfiguration de Kydah).
    - **Source:** user instruction, 2026-11-02

- **DEC-5 - Conventions de nommage : `miryad_lymaille_*` et `MRD-LYMAILLE-NNN`**
    - **Decision:** tables préfixées `miryad_lymaille_`, codes d'erreur `MRD-LYMAILLE-NNN`, par analogie avec
      `miryad_matrix_`/`MRD-MATRIX` et `miryad_workflow_`/`MRD-WORKFLOW`.
    - **Consequences:** `miryad-core#CON-3` et `CON-6` listent ces noms au prochain amendement du Domain.
    - **Source:** proposition Claude, 2026-11-02, non contestée par le développeur principal

- **DEC-6 - Lots de réalisation**
    - **Decision:** lot 0 = identité du déclencheur dans `RunInfo` (`FR-10`, `DEC-12`, côté `workflow`). Lot 1 = socle
      (feature, dépendance, rustls, migration, stores) + module Rhai des ressources + kind `subagent`. Lot 2 = glue `matrix` (`OI-5`). Lot 3 = kind `pad`, après une release de `lymaille` qui livre
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
    - **Consequences:** la durée d'un run est bornée par celle d'une fermeture `ctx.run` (`OI-4`) ; le kind est ordinaire
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

Aucune spec écrite à ce jour. Lot 1 (`DEC-6`), noms provisoires, **un fichier source = une spec** :
`SpecDD /Cargo.toml` (feature, dépendance, relais), `SpecDD /src/lymaille/mod.sdd`, `/src/lymaille/error.sdd`
(`MRD-LYMAILLE-*`), `/src/lymaille/tls.sdd` (`FR-5`), `/src/lymaille/identity.sdd` (`FR-4`, `FR-8`, `FR-9`),
`/src/lymaille/store/*.sdd` (`FR-3`, un fichier par groupe de traits), `/src/lymaille/resource_module.sdd` (`FR-2`),
`/src/migration/` (migration interne), `/src/workflow/subagent_step.sdd` (`FR-6`). Lot 0, préalable : amendements de
`/src/workflow/step.sdd`, `client.sdd`, `interpreter.sdd` et `subworkflow.sdd` (`FR-10`). Lot 3 : `/src/workflow/pad_step.sdd`.

## Acceptance Criteria

- **AC-1:** Un graphe combinant miryad-core (toutes features) et lymaille (path) compile, avec un seul rhai, un seul
  tokio et un seul rustls en `cargo tree`. Vérifié le 2026-11-01 (crate tampon, `cargo check` vert).
- **AC-2:** `cargo tree --no-default-features --features lymaille -e normal -i sea-orm` imprime une ligne (la feature
  implique `db`), et `lymaille` absent du graphe par défaut. À poser avec la première spec.

- **AC-3:** Un utilisateur sans droit de lecture d'une ressource ne la lit pas par le module des ressources d'une session
  productive ; mêmes refus que REST et MCP pour `read_policy`, `write_policy`, `owner_column` et hooks.
- **AC-4:** Une session sauvegardée par les stores SeaORM puis rechargée est identique, sur SQLite et PostgreSQL.
- **AC-5:** Un utilisateur retiré du groupe admin entre deux appels voit le second appel d'un module réservé à l'admin
  refusé, dans une session `Work` comme `Config` ; un utilisateur non-admin n'a jamais ces modules, quelle que soit la
  nature de sa session.
- **AC-6:** Deux montages d'état successifs dans le même processus réussissent, et un appel MCP de `lymaille` n'avorte pas.

- **AC-7:** Deux utilisateurs ne voient jamais les agents, toolsets, skills, brouillons ni sessions l'un de l'autre ; tous
  deux voient le catalogue partagé, que seul un admin modifie.
- **AC-8:** Un non-admin ouvre une session de configuration, écrit et active un sous-agent ; sa définition qui accorde un
  module réservé à l'admin est refusée à la sauvegarde.

- **AC-9:** Un utilisateur, admin compris, qui crée dans son espace privé un identifiant commençant par `G_` ou `g_` est
  refusé ; un admin qui publie `G_x` alors qu'un utilisateur a un agent `x` réussit, et l'utilisateur voit les deux sans
  confusion possible.

- **AC-10:** Un step `subagent` dont le processus est tué en milieu de run reprend sur la session sauvegardée au rejeu : le
  nombre de tours déjà terminés n'est pas refait, et le résultat est journalisé une seule fois. Preuve par la méthode du
  spike de `workflow#AC-8` (non verrouillée par la CI).
- **AC-11:** Un run déclenché par un utilisateur A donne à ses steps et à ses runs enfants le `subject` de A ; un run sans
  identité fait échouer un step `subagent` en `StepError` non retryable.

## Open Issues

- **OI-1:** **Relais de features** : proposition Claude à confirmer : garder les défauts de `lymaille` (`mcp-http`, `rig`),
  nécessaires au lot 1 (inférence et MCP distants), sans les relayer en sous-features miryad tant qu'aucun hôte ne les
  désactive ; `fs`/`shell` : jamais (Constraints). **TBD.**
- **OI-2:** **Traits de store du lot 1** : sur 23 traits publics de `lymaille`, lesquels le lot 1 implémente-t-il ?
  Candidats : `SessionStore`, `AgentStore`, `ProfileStore`, `ProviderStore`, `ToolsetStore`, `ContextTypeStore`,
  `ContextStore`, `MemoryStore`. Les autres (`DraftStore`, `SessionIndex`, `EventSource`, `RequestCounter`…) à qualifier.
  Tous cloisonnés par propriétaire (`DEC-7`), stores construits par principal. **TBD** à la rédaction de la spec `store`.
- **OI-3:** **Pad de workflow** : `PW-1` à `PW-5` ne sont pas dans `lymaille` 0.1.0. À faire côté `lymaille` d'abord
  (nature `Workflow` de `SessionState`, cycle de vie sans tour, instantané JSON). Le kind `pad` (`FR-7`) attend cette
  release. **TBD :** versionnage de la demande côté `lymaille`.
- **OI-4:** **Durée d'un run dans un `ctx.run`** (`DEC-10`) : les délais d'inactivité et d'abandon de Restate ne sont pas
  lus ici. **TBD :** spike à faire avant la spec `subagent_step.sdd` (durée maximale tenue, comportement à l'expiration,
  réglage du service) ; si la borne ne convient pas, le kind passe en durable par tour ou en awakeable (`DEC-10`,
  alternatives).
- **OI-5:** **Glue Matrix** (`matrix` + `lymaille`) : mapping salle ↔ session, dédoublonnage des messages remis deux
  fois après bascule de bail, mise en forme des sorties longues, annuaire mxid → principal (inexistant :
  `miryad_matrix_tracked_users` est le store crypto E2EE du bot, pas un annuaire). **TBD** — lot 2.
- **OI-6:** **Granularité du module des ressources** (`FR-2`) : une fonction Rhai par opération et par ressource
  (`list`, `get`, `create`, `update`, `delete`), schéma découvrable, pagination et filtres de `src/query.rs`. À
  qualifier avec la spec `resource_module.sdd`. **TBD.**

## References

- Dépôt `lymaille` (`sebt3/lymaille`) et sa bibliothèque DSD `agent-sessions` (domaine de l'agent LLM lui-même).
- `miryad-core/workflow#OI-8` (pad de workflow réservé) et l'« Extension réservée : pad de workflow » de
  `lymaille/dsd/agent-sessions/scratchpad.md` (PW-1 à PW-3).
- Exploration d'intégrabilité du 2026-11-01 (graphes combinés, harness lymaille vert : 1327 tests, clippy propre).
