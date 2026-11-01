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
- **Préalable :** une première release publiée de `lymaille` (crates.io) ; à ce jour la crate est en `0.0.0` avec
  `publish = false` (Observed in: `lymaille/Cargo.toml`, 2026-11-01). Aucun lot de spec n'ouvre tant que la release
  n'existe pas (user instruction, 2026-11-01).

## Current State and Target State

- **Actuel :** rien dans la crate ; aucune spec. La feature était réservée par `miryad-core/workflow#OI-8`.
- **Cible :** feature `lymaille`, modules sous `src/lymaille/` (stores SeaORM des traits hôte, kinds de step
  sous-agent et pad, glue éventuel vers `matrix`), migrations internes `miryad_*`.

## Faits vérifiés (exploration d'intégrabilité, 2026-11-01)

- Le graphe combiné miryad-core (toutes features) + lymaille **se compile** : rhai s'unit en **1.27.0 en un seul
  exemplaire** (`vynil-core` 0.9 exige `^1.23.3` et absorbe l'épinglage de lymaille), tokio 1.54.1 et rustls 0.23.46
  unifiés, `sea-orm` 2.0.4 seul. **Verified by:** `AC-1`.
- lymaille **ne porte aucun stockage SQL** : aucun `sqlx` ni `libsqlite3-sys` dans son graphe, une quinzaine de traits
  de store laissés à l'hôte (`SessionStore`, `ToolsetStore`, `MemoryStore`, `ProfileStore`, `ContextStore`, `AgentStore`,
  etc.). L'hypothèse DEC-1 est donc exacte. **Verified by:** `AC-1`.
- lymaille **avorte si le hôte n'a pas installé un crypto provider rustls** avant le premier appel rig ou MCP client
  (`reqwest-tls-no-provider` ; Observed in: `lymaille/src/agents/rig.rs`, `lymaille/src/modules/mcp_client.rs`). C'est
  un piège d'intégration, pas un détail — voir OI-2.
- `SessionKind = Work | Config | Sub` et la règle `agent-sessions#BR-8` (l'agent de configuration ne délègue pas,
  n'appelle aucun MCP, active rien) existent nativement chez lymaille : les deux axes de DEC-2 s'y appuient sans
  adaptation.
- La combinaison `matrix` + `lymaille` est déjà prouvée compilable dans le même graphe ; la jugée amont est le handler
  public `MatrixEventHandler::on_message` (harnais chatbot de `src/matrix/sync.sdd`) et `MatrixClient::send`. Le DSD de
  lymaille cite une « application de réception d'alertes avec agent Matrix » comme hôte type.

## Applicable Domain Requirements

`miryad-core#FR-1` (parité des surfaces — le canal agent devient un consommateur des politiques), `miryad-core#CON-3`
(codes `MRD-*` — domaine nommé OI-6), `miryad-core#CON-4` (feature et batterie), `miryad-core#CON-6` (préfixe
`miryad_` — OI-3).

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
    - **Verified by:** à qualifier avec les specs (aucune `AC` n'est posée avant l'arbitrage du modèle d'identité, OI-4).

## Design Decisions

- **DEC-1 - Le stockage des objets de lymaille est porté par la BDD de miryad-core**
    - **Context:** lymaille laisse ses stores à l'hôte ; l'alternative (stores mémoire de la librairie) fragmenterait
      persistance et sauvegarde.
    - **Decision:** miryad-core implémente les traits de store sur SeaORM ; activer `lymaille` active `db`.
    - **Consequences:** migrations internes dédiées (préfixe à trancher, OI-3) ; les stores mémoire de lymaille restent
      cantonnés aux tests.
    - **Source:** user instruction, 2026-11-01

- **DEC-2 - Deux axes de sessions : configuration et production**
    - **Context:** lymaille distingue déjà sessions `Config` et `Work` ; les ressources de la crate ont un gradé
      admin/user.
    - **Decision:** par défaut, les ressources réservées à l'admin alimentent les sessions de configuration ; les
      ressources `user` alimentent les modules des sessions productives.
    - **Consequences:** il manque la liaison session ↔ principal (qui parle, en tant que qui) — OI-4 ; le bridge Matrix
      bot en dépend (OI-5).
    - **Source:** user instruction, 2026-11-01

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

Aucune pour l'instant — la Solution est ouverte, les specs viennent après la première release de lymaille. Entrées
futures probables : `SpecDD /Cargo.toml`, `SpecDD /src/lymaille/mod.sdd` (à créer), `SpecDD /src/workflow/pad_step.sdd`
et `SpecDD /src/workflow/subagent_step.sdd` (noms provisoires).

## Acceptance Criteria

- **AC-1:** Un graphe combinant miryad-core (toutes features) et lymaille (path) compile, avec un seul rhai, un seul
  tokio et un seul rustls en `cargo tree`. Vérifié le 2026-11-01 (crate tampon, `cargo check` vert).
- **AC-2:** `cargo tree --no-default-features --features lymaille -e normal -i sea-orm` imprime une ligne (la feature
  implique `db`), et `lymaille` absent du graphe par défaut. À poser avec la première spec.

## Open Issues

- **OI-1:** **Relais de features** : garder les défauts de lymaille (`mcp-http`, `rig`) ou les relaguer en sous-features
  miryad ? `fs`/`shell` : jamais (Constraints ci-dessus, à confirmer). **TBD.**
- **OI-2:** **Provider rustls** : lymaille avorte sans installation préalable ; matrix-sdk et l'OIDC le réclament aussi.
  Qui installe — miryad-core au montage de l'état (install idempotent) ou le template `miryad` au démarrage ? **TBD.**
- **OI-3:** **Préfixe des tables** de lymaille sous la promesse `miryad_*` de `miryad-core#CON-6` : `miryad_ll_*` ?
  **TBD.**
- **OI-4:** **Chaîne d'identité** : liaison session ↔ `AuthPrincipal` persistée par le store miryad ; une session
  productive agit sous le principal de son utilisateur, une session de configuration sous un principal admin. Sans cet
  arbitrage, FR-2 ne peut pas avoir de test. **TBD.**
- **OI-5:** **Glue Matrix** (`matrix` + `lymaille`) : mapping salle ↔ session, dédoublonnage des messages remis deux
  fois après bascule de bail, mise en forme des sorties longues, annuaire mxid → principal (inexistant :
  `miryad_matrix_tracked_users` est le store crypto E2EE du bot, pas un annuaire). **TBD** — à ouvrir quand la feature
  existera.
- **OI-6:** **Domaine des codes `MRD-*`** de la surface agent : `MRD-LLM-NNN` ? `MRD-LYMAILLE-NNN` ? **TBD.**

## References

- Dépôt `lymaille` (`sebt3/lymaille`) et sa bibliothèque DSD `agent-sessions` (domaine de l'agent LLM lui-même).
- `miryad-core/workflow#OI-8` (pad de workflow réservé) et l'« Extension réservée : pad de workflow » de
  `lymaille/dsd/agent-sessions/scratchpad.md` (PW-1 à PW-3).
- Exploration d'intégrabilité du 2026-11-01 (graphes combinés, harness lymaille vert : 1327 tests, clippy propre).
