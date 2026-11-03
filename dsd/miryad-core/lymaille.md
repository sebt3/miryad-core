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
- **Extensions de l'hôte applicatif :** les modules Rhai fournis par l'application et l'exécution de script sans tour de LLM sont
  conçus dans `miryad-core/scripts-hote` ; les hooks de ressource asynchrones, dans `miryad-core/hooks-async`.
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
    déclencheur (`FR-10`) et porte un plafond de durée `max_secs` dans son `config` (défaut 480 s, plafond relevable par l'application, `DEC-29`), appliqué par l'annulation
    coopérative de `lymaille` (`CancelHandle`), strictement inférieur à la borne `ctx.run` du handler dédié (`DEC-29`) : l'agent s'arrête
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
      `/src/workflow/subworkflow.sdd` (T6), amendées le 2026-11-02 et approuvées par le développeur principal ce jour — **livré le 2026-11-02**
      (commits `667518c` → `1de97e6`, une tâche par commit, validator GO à chaque tâche et batterie complète à la clôture)
    - **Verified by:** `AC-11`

- **FR-11 - MUST:** Un utilisateur ouvre, suit, annule, reprend et ferme une session interactive **dans un contexte** (la nature `Work` ou `Config` et l'agent par défaut
    découlent du type du contexte, `DEC-28`) ; au plus un tour actif par session (application mono-réplique, `DEC-24`) ; un tour survit à la déconnexion du client ; les événements sont diffusés à tous les abonnés de la session ; chaque
    appel relit le principal. Le socle est indépendant du canal.
    - **Source:** `DEC-6`, `DEC-13`, `DEC-24`, `DEC-28` ; user instruction, 2026-11-03
    - **Verified by:** `AC-15`, `AC-19`

- **FR-12 - MUST:** Les sessions interactives sont servies en REST+SSE sous `/api/v1/lymaille`, exemptées de la parité des ressources (comme `/auth/*`) ;
    l'envoi d'un message répond `202` sans attendre le tour.
    - **Source:** `DEC-25` ; user instruction, 2026-11-03
    - **Verified by:** `AC-16`

- **FR-13 - MUST:** Les sessions interactives sont servies par Matrix : une salle directe = une session ; un mxid non lié n'a aucun droit ; un
    événement remis deux fois ne déclenche qu'un tour ; les sorties longues sont découpées.
    - **Source:** `DEC-23`, `DEC-26` ; user instruction, 2026-11-03
    - **Verified by:** `AC-17`

- **FR-14 - MUST:** L'application hôte fournit, par un trait unique `LymailleHost`, ce que miryad-core n'a pas : le serveur MCP propre à un
    contexte (URL, jeton frais, décision d'autorité) et les contextes de niveau projet. Sans ce trait, le comportement est celui de `DEC-20` seul.
    - **Source:** `DEC-27` ; `NOTE-vanyline-orientation-lymaille.md` §7 (B1, B3) ; user instruction, 2026-11-03
    - **Verified by:** `AC-18`

- **FR-15 - MUST:** Les événements d'une session interactive sont typés et exportables vers TypeScript, extensibles par l'hôte sans que la crate en
    interprète le contenu, et l'historique d'une session se relit par une route dédiée.
    - **Source:** `DEC-30` ; user instruction, 2026-11-03
    - **Verified by:** `AC-20`

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
      catalogue, le reste à l'espace du propriétaire. Les skills suivent leur toolset. **Amendé le 2026-11-03 (`DEC-27`) :** un préfixe `H_` désigne un contexte résolu par l'hôte, jamais stocké.
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
      (`MiryadWorkflowStep`), pas durable, tant que cette borne convient ; une session de step est une session `Sub`. **Amendé le 2026-11-03 :** la borne devient celle d'un handler dédié, voir `DEC-29`.
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
    - **Decision:** (1) **types de contexte** : `MiryadResource` du catalogue, lecture `Public`, écriture `AdminOnly` ; ils portent la nature, l'agent par défaut et les agents autorisés (`DEC-28`). (2) **contextes** : paire de
      ressources comme `DEC-19` ; niveau `User` = `OwnerOnly`, niveau `Shared` (ex-`Project`, renommé le 2026-11-03 : « projet » est le mot de l'hôte ; partagé par toute l'instance, miryad-core n'a pas d'entité
      projet) = catalogue `AdminOnly` ; niveau `Session` interne, sans CRUD. (3) **`mcp_servers`** : `MiryadResource` admin (nom, URL, délais,
      `auth_ref`, mode d'auth, `allowed_groups`) ; la référence d'auth est résolue par le `SecretResolver` de `DEC-16`, le secret n'est jamais en base.
      (4) **droit d'usage** : `allowed_groups` liste des noms de groupes, plus les valeurs réservées `all` (tout authentifié) et `admin` ; le contrôle d'autorité
      est fait **à l'assemblage de la session et à l'appel** (`mcp_access`), et à `ContextStore::put_context` pour la session de configuration. **Correction
      du 2026-11-03 :** un hook `MiryadResource` est synchrone et sans connexion, il ne peut ni relire les groupes ni l'état admin ; le refus « à la sauvegarde »
      par REST/MCP n'existe donc pas (même limite pour `check_grants` des agents et toolsets privés, `FR-9`) ; un accord excessif échoue à l'usage, il n'élève jamais un droit.
      Lever cette limite demanderait des hooks asynchrones dans le moteur de ressources : hors lot 2, arbitré le 2026-11-03 : `miryad-core/hooks-async` ; l'appartenance est relue en base à chaque appel, jamais mise en cache. (5) **modes d'auth** : `static`
      (référence de secret, compte de service) et `forward` (le serveur reçoit le jeton de l'utilisateur) ; `forward` n'est valable qu'en
      session interactive, avec le jeton frais de la requête, jamais persisté ; un step batch qui lie un serveur `forward` échoue en `StepError`
      non retryable. **Amendé le 2026-11-03 (`DEC-27`) :** un serveur MCP propre à un contexte est fourni par l'hôte, sans ligne `mcp_servers` ni mode d'auth supplémentaire.
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

- **DEC-24 - Sessions interactives : gestionnaire, hub d'événements, tour détaché** (2026-11-03)
    - **Decision:** un `SessionManager` partagé porte le cycle de vie (ouvrir, envoyer, suivre, annuler, reprendre, fermer) ; un `EventHub` (`broadcast` par
      session) diffuse les événements de `lymaille` ; le tour est une tâche `tokio` détachée de la requête. Le jeton d'une session `forward` n'existe
      qu'en mémoire pour un tour. `Config` ne monte aucun client MCP. L'activation d'un toolset est une action de l'hôte (route ou commande), jamais un
      appel de l'agent. Les erreurs de script sont persistées par un `AuditWriter` pour `SessionIndex::script_errors`.
    - **Alternatives:** tour attaché à la requête (écarté : un onglet fermé tuerait le tour) ; rejeu d'événements par `Last-Event-ID` (écarté : les événements
      ne sont pas persistés ; l'état de la session l'est).
    - **Consequences:** pas de coordination entre processus (`session#DEC-1` de `lymaille`). **Limite assumée le 2026-11-03 :** l'application est
      **mono-réplique** pour les sessions interactives : le hub d'événements est en mémoire (un abonné SSE d'une autre réplique ne verrait rien) et un
      tour actif n'est protégé qu'au sein du processus. Un verrou en base n'est rouvert qu'avec un besoin de haute disponibilité. L'ouverture se fait
      par contexte (`DEC-28`).
    - **Source:** proposition Claude, 2026-11-03, à approuver

- **DEC-25 - REST+SSE : routeur auto-monté, exempté de la parité** (2026-11-03)
    - **Decision:** `conversations_router` sous `/api/v1/lymaille`, monté par l'application ; JSON pour les commandes, SSE pour le flux ; aucune décision
      de droit propre. Le jeton Bearer de la requête est transmis à `send` (usage `forward`) ; une requête par cookie n'en transmet aucun.
    - **Consequences:** `forward` n'est donc utilisable que par un client authentifié par Bearer ; ni un navigateur par cookie, ni Matrix.
    - **Source:** user instruction (exemption), 2026-11-03

- **DEC-26 - Matrix : un pont `MatrixEventHandler`, salle directe, dédoublonnage en base** (2026-11-03)
    - **Decision:** `ChatBridge` implémente `MatrixEventHandler` ; une salle = un utilisateur lié = une session ouverte dans un contexte (`DEC-28`) (`!new <contexte> [agent]`, `!cancel`, `!resume`,
      `!activate`, `!link`, `!help`) ; dédoublonnage par `event_id` en base, fenêtre 7 jours ; événements plus vieux que 5 minutes ignorés ; réponse
      en messages de 4000 caractères maximum, huit au plus ; pas d'édition de message.
    - **Source:** proposition Claude, 2026-11-03, à approuver

- **DEC-27 - `LymailleHost` : serveurs MCP et contextes fournis par l'application** (2026-11-03)
    - **Context:** l'application `vanyline` lie un contexte à une sandbox dont l'URL MCP est créée et détruite dynamiquement, dont le jeton est
      minté à la demande (API `TokenRequest` de Kubernetes, courte durée) et dont l'autorité est un lien utilisateur↔propriétaire connu de
      l'application seule. Ni `static` ni `forward` (`DEC-20`) ne couvrent ce cas ; un « projet » est de plus une entité réelle de l'application.
    - **Decision:** (1) un trait unique `LymailleHost`, fourni par l'application au montage et optionnel. (2) `resolve_mcp(contexte, rôle, principal)`
      rend `Refusé` ou `Accordé{url, jeton, expiration}` ; il est appelé à l'assemblage **et** à chaque appel d'outil. Aucune liaison n'est stockée
      en table ; un rôle que l'hôte ne pourvoit pas retombe sur la liaison de contexte classique (`mcp_servers`, `DEC-20`). Le cache du jeton est
      l'affaire de l'hôte : miryad-core ne garde ni ne persiste aucun jeton. En batch, l'autorité est tranchée sous le `subject` du déclencheur
      (`FR-10`) ; sans `subject`, le step échoue en `StepError` non retryable. Pas de contrôle à la sauvegarde (hooks synchrones, `DEC-20`) : un accord
      excessif échoue à l'usage. La session `Config` voit ces liaisons en lecture seule, à titre informatif, sans les éditer et sans monter de client.
      (3) `resolve_context(id, principal)` rend `Option<ToolsetContext>` pour un identifiant de préfixe `H_` : jamais stocké, jamais écrit par REST,
      MCP ni la session `Config`, aiguillé vers le trait par `identity::classify`. (4) Une panne de l'hôte est un refus (échec fermé).
    - **Alternatives:** troisième mode d'auth `host` dans `mcp_servers` (écarté : une ligne administrée par sandbox dynamique) ; deux traits
      distincts pour MCP et contextes (écarté : deux points d'extension à enregistrer) ; `authorize` et `mint_token` séparés (écarté : fenêtre entre
      l'autorité et le jeton) ; cache de jeton dans miryad-core (écarté : un jeton survivrait à une révocation).
    - **Consequences:** amende `identity.sdd` (variante de `classify`), `mcp_access.sdd`, `interactive/assemble.sdd`, `definitions/user_context.sdd` et
      `mod.sdd` (montage) ; tests avec un hôte factice. L'hôte garde seul la définition de « cet utilisateur peut agir sur ce propriétaire ».
    - **Source:** `NOTE-vanyline-orientation-lymaille.md` §7 (B1, B3) ; user instruction, 2026-11-03

- **DEC-28 - Le contexte pilote la session** (2026-11-03, amende `FR-11`, `DEC-24`, `DEC-26`)
    - **Context:** l'utilisateur choisit un contexte de travail, pas un agent parmi N ; la nature de la session en découle.
    - **Decision:** l'ouverture prend `(contexte, agent optionnel)`. Le **type de contexte** porte la nature (`Work` ou `Config`), l'agent par défaut et
      l'ensemble des agents autorisés. Le contexte est persisté dans la session et immuable. Un agent explicite hors de l'ensemble autorisé est
      refusé par une erreur typée `MRD-LYMAILLE-*`. Ouvrir une session `Config` suit le droit de l'utilisateur sur le contexte, sans statut admin
      implicite. Le niveau de contexte `Project` devient `Shared`. Sous Matrix, `!new <contexte> [agent]` remplace `!agent`.
    - **Alternatives:** règle fournie par l'hôte (écarté : ni administrable ni lisible par la session `Config`) ; ouverture `Config` réservée aux admins
      (écarté : la configuration d'un projet par un non-admin est voulue, `FR-9`).
    - **Consequences:** trois champs de plus sur la ressource des types de contexte, donc sur la table de la migration `m20261104_000001`, non encore
      déployée (modification en place à actualiser dans les exceptions d'`AGENTS.md`, ou migration suivante). **ASSUMPTION :** ces champs vivent en
      colonnes de miryad-core et non dans l'objet `ContextType` de `lymaille` ; à vérifier contre `ContextTypeStore`. Le renommage `Shared` est à
      signaler à `lymaille` (`ProviderLevel` et niveau de contexte).
    - **Source:** `NOTE-vanyline-orientation-lymaille.md` §7 (B2, B3) ; user instruction, 2026-11-03

- **DEC-29 - Handler Restate dédié au step `subagent`** (2026-11-03, amende `DEC-10`, `FR-6`, `OI-4`)
    - **Context:** un agent de code qui compile et teste dure de 30 à 45 minutes ; la borne par défaut d'un `ctx.run` est d'environ 11 minutes, et le
      réglage par service retarderait la détection des steps ordinaires bloqués (`OI-4`, conclusion 4).
    - **Decision:** le step `subagent` est dispatché par un handler Restate dédié, aux bornes `inactivity_timeout` et `abort_timeout` relevées ; le
      plafond `max_secs` se règle par l'application (défaut inchangé à 480 s). Le step reste `retryable: false` et la reprise sur session
      déterministe (`DEC-10`) est inchangée.
    - **Alternatives:** relever les bornes du service `StepDispatcher` (écarté : tous les kinds en pâtissent).
    - **Consequences:** amende `dispatcher.sdd` et `subagent_step.sdd`. Le réglage par handler est prouvé sur 40 minutes (`OI-11`, spike du 2026-11-03) ; le kill du processus en cours de run reste couvert par `AC-10`.
    - **Source:** `NOTE-vanyline-orientation-lymaille.md` §7 (B4) ; user instruction, 2026-11-03

- **DEC-30 - Événements de session typés, extensibles, relisibles** (2026-11-03, amende `DEC-24`, `DEC-25`)
    - **Decision:** les événements du flux sont un enum `SessionEvent` exportable vers TypeScript, avec une variante `Host{kind, payload}` que
      l'application type de son côté ; la crate n'interprète jamais le `payload` et n'emploie aucun générique. Une route de lecture rend l'état
      persisté de la session (messages, appels d'outil) sous les mêmes types ; le client lit l'historique puis s'abonne au SSE. Les événements `Host` ne
      sont pas rejoués, et `Last-Event-ID` reste écarté (`DEC-24`).
    - **Alternatives:** enum générique `SessionEvent<H>` (écarté : propage un paramètre de type dans le hub, le gestionnaire et les routes) ;
      journal d'événements rejouables (écarté : volumétrie et rétention, déjà écarté par `DEC-24`).
    - **Consequences:** amende `http/routes.sdd`, `http/sse.sdd`, `interactive/hub.sdd` ; une route de plus que les neuf de `AC-16`. L'outil d'export
      TypeScript reste à choisir (`OI-12`).
    - **Source:** `NOTE-vanyline-orientation-lymaille.md` §7 (B7) ; user instruction, 2026-11-03

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
- **Hôte applicatif** : `LymailleHost` (`DEC-27`), ouverture par contexte (`DEC-28`), handler dédié du step `subagent` (`DEC-29`), événements typés et
  historique (`DEC-30`).
- **Hors lot 2 :** kind `pad` (lot 3, `OI-3`) ; `forward` de jeton en batch (`DEC-20`) ; hooks asynchrones (`miryad-core/hooks-async`) ; modules
  d'hôte et exécuteur de script (`miryad-core/scripts-hote`).

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
`workflows.sdd` restent à amender avec accord. **Lot 2 : specs rédigées le 2026-11-03, en attente d'approbation** (`FR-11` à `FR-13`, `DEC-20` à `DEC-26`) : `/src/lymaille/interactive/{mod,hub,manager,assemble,activation}.sdd`, `/src/lymaille/http/{mod,routes,sse}.sdd`, `/src/lymaille/chat/{mod,link,render,bridge}.sdd`, `/src/lymaille/mcp_access.sdd`, `/src/lymaille/definitions/{mcp_server,context_type,catalog_context,user_context,private_agent,private_toolset}.sdd`, `/src/lymaille/store/{draft,session_index,audit,purge,write_journal}.sdd` ; amendements : `error.sdd`, `migration.sdd`, `store/{mod,toolset}.sdd`, `definitions/mod.sdd`, `resource_module.sdd`, `mod.sdd`, `/src/workflow/subagent_step.sdd`. Lot 3 : `/src/workflow/pad_step.sdd`.

**Réalignement du 2026-11-03 (`DEC-27` à `DEC-30`) :** le code du lot 2 est à l'arrêt le temps de l'amendement. Specs à ré-aligner avant approbation : `identity.sdd`,
`mcp_access.sdd`, `definitions/{context_type,user_context}.sdd`, `interactive/{mod,hub,manager,assemble}.sdd`, `http/{routes,sse}.sdd`, `chat/{bridge,link}.sdd`,
`migration.sdd`, `mod.sdd`, `/src/workflow/{dispatcher,subagent_step}.sdd`.

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

- **AC-12:** Un utilisateur ne lie ni n'appelle un serveur MCP hors de ses groupes ; absent et interdit sont indiscernables ; l'appartenance est relue à chaque
  appel ; `forward` est refusé hors session interactive ; le secret `static` est résolu à chaque connexion et n'apparaît jamais en base, en erreur ni en trace.
- **AC-13:** `purge_sessions` supprime sessions, erreurs de script et journal d'écritures plus anciens que la rétention, rien d'autre, et est idempotente.
- **AC-14:** Un step batch rejoué par `restate-server` ne duplique aucune écriture de ressource ; les écritures suivent les politiques de REST et MCP.
- **AC-15:** Une session interactive ouverte, alimentée, annulée puis reprise après redémarrage conserve son historique ; un second tour concurrent est refusé ;
  une session d'un autre est introuvable.
- **AC-16:** Les neuf routes REST+SSE répondent comme spécifié ; une déconnexion SSE n'annule pas le tour ; `lagged` signale un abonné lent.
- **AC-17:** Un message Matrix d'un mxid lié ouvre une session et reçoit la réponse ; un mxid non lié n'a aucun droit ; un événement remis deux fois ne fait qu'un tour.

- **AC-18:** Avec un hôte factice, un rôle MCP pourvu par l'hôte rend l'URL et un jeton frais à chaque appel ; un refus de l'hôte, ou sa panne, refuse l'appel ;
  un rôle non pourvu retombe sur `mcp_servers` ; en batch, un step sans `subject` échoue en `StepError` non retryable ; aucun jeton n'est écrit en base, en erreur ni en trace ;
  un identifiant `H_…` ne peut être écrit par aucune surface.
- **AC-19:** Une session s'ouvre sur un contexte : nature et agent par défaut viennent du type ; le contexte est immuable ; un agent hors ensemble est refusé par une erreur typée ;
  une session `Config` s'ouvre sans être admin si le contexte est utilisable.
- **AC-20:** Les événements du flux et l'historique partagent les mêmes types exportés ; un rechargement retrouve la conversation ; un événement `Host` traverse le hub sans être interprété.

## Open Issues

- **OI-11:** **Soldé le 2026-11-03 par spike** (durée réelle d'un sous-agent, `DEC-29`). Banc : `tests/workflow_restate.rs`,
  `un_step_de_40_minutes_survit_aux_bornes_relevees_du_handler` (`#[ignore]`, vrai `restate-server`, image `latest`, `restate-sdk` 0.12.1) : un step ordinaire
  de 40 minutes dans l'unique `ctx.run` de `execute`, avec `inactivity_timeout` 50 min et `abort_timeout` 55 min posés **sur le handler** (`HandlerOptions`). Résultat :
  statut 200, durée mesurée 2402 s, un seul `start`, un seul `end`, aucun rejeu. **Conclusion :** le réglage par handler tient sur 40 minutes. **Non couvert :** le kill du
  processus service en cours de route (le serveur de test vit dans le processus du test) ; la reprise reste portée par la session déterministe (`AC-10`).
    - **Source:** spike du 2026-11-03, résultat ci-dessus ; user instruction, 2026-11-03
- **OI-12:** **Export TypeScript des événements** (`DEC-30`) : `ts-rs` n'est pas une dépendance de la crate. **TBD :** `ts-rs` derrière la feature `lymaille` (dépendance nouvelle à acter, `CON-4`), ou schémas
  `utoipa` déjà présents pour OpenAPI.
- **OI-10:** **Hooks asynchrones ou contrôle de droits à la sauvegarde REST/MCP** : les hooks de `MiryadResource` sont synchrones, sans connexion ni état admin ; les contrôles `check_grants` (agents, toolsets privés) et le droit d'usage des serveurs MCP (contextes) ne se font qu'à l'assemblage, à l'appel et par les stores. **Soldé le 2026-11-03 (user instruction) :** extension du moteur de ressources par des hooks asynchrones avec connexion, hors lot 2 ; voir `miryad-core/hooks-async`.

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
  step ordinaire bloqué. **Décidé dans `subagent_step.sdd` (en attente d'approbation) :** `recommended_options()` inchangé au lot 1 ; `max_secs` plafonné à 540 s, défaut 480 s. **Amendé le 2026-11-03 : handler dédié, `DEC-29`.**
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
