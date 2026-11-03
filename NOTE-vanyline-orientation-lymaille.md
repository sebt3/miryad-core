# Note de cadrage : besoins de vanyline sur l'intégration lymaille de miryad-core

*Statut : note de travail (2026-11-03), à reprendre en session dans ce dépôt pour amender
`dsd/miryad-core/lymaille.md` avant que le lot 2 ne soit codé. Ce n'est ni un DSD ni une spec.*

## 1. Pourquoi cette note

vanyline (environnement de développement cloud-native piloté par LLM, consommateur de miryad-core)
réoriente son harnais LLM : **lymaille devient le cœur générique**, hébergé par l'application via
miryad-core, au lieu du tooling maison (`vanyline-lib`, agents système, `llm-config`). Le plan
« extraire le tooling vanyline vers miryad-core » est abandonné.

Le DSD d'intégration (`dsd/miryad-core/lymaille.md`) convient sur l'essentiel : stockage SeaORM,
`fs`/`shell` jamais activés, principal relu à chaque appel, `Config` sans client MCP. Cette note
liste ce qui, en l'état, **obligerait vanyline à contourner le DSD**, tant qu'il est encore temps
de l'amender.

## 2. La thèse de vanyline (ce qui guide les besoins)

1. **Le contexte de travail est le seul sélecteur de session.** L'utilisateur choisit un contexte
   (un projet, une sandbox, la configuration), pas un agent parmi N. L'agent principal et la
   nature de la session (`Work` ou `Config`) en découlent.
2. **Pas d'approbation en session.** Les limites sont posées *avant*, par le contexte : modules et
   toolsets accordés, serveurs MCP liés. Une approbation humaine par commande ne protège pas mieux.
3. **Séparation stricte des rôles.** En contexte productif, le LLM peut modifier les fichiers du
   workspace (`.skills/`, `AGENTS.md`) mais ne remonte dans la configuration que via une session en
   contexte de configuration ; la promotion est une action humaine hors session. Les *droits* ne
   viennent jamais du workspace.
4. **L'interaction avec le LLM est le cœur de l'UX** : « j'ai besoin d'une MR qui fait X », puis
   relecture sur la forge. Le LLM doit aussi pouvoir *montrer* : naviguer dans l'interface pour
   l'utilisateur et lui présenter un tableau de bord éphémère (voir § 4).

## 3. Écarts à traiter (par ordre d'importance)

### B1. Serveurs MCP fournis par l'hôte (bloquant)

`DEC-20` ne connaît que deux modes d'auth : `static` (référence de secret) et `forward` (jeton de
l'utilisateur, interactif seulement). Le MCP d'une sandbox vanyline ne rentre dans aucun :

- l'URL est propre à chaque sandbox, créée et détruite dynamiquement ; ce n'est pas une ligne
  `mcp_servers` administrée ;
- le jeton est celui du ServiceAccount de l'Owner (API `TokenRequest` Kubernetes), **minté à la
  demande**, de courte durée ;
- l'autorité (« cet utilisateur a-t-il le droit d'agir sur cet Owner ? ») est un contrôle de l'hôte
  (lien utilisateur↔Owner), pas une liste de groupes `allowed_groups`.

**Besoin :** un trait de l'hôte, par exemple `McpServerProvider`, appelé à l'assemblage de la
session et à chaque appel : pour un contexte donné, il rend l'URL, un jeton frais et rend sa
décision d'autorité. Valable en session interactive **et en batch** (le jeton ne vient pas de la
requête). Dans `DEC-20`, ce serait un troisième mode d'auth `host` ; `FR-9` (« une définition ne
peut accorder que des modules que son propriétaire pourrait utiliser ») s'appuie alors sur la
décision de l'hôte.

### B2. Le contexte pilote la session ; la nature ne se choisit pas

`FR-11` / `DEC-24` ouvrent une session de nature `Work` ou `Config` au choix de l'utilisateur, et
le pont Matrix expose `!agent`. C'est l'ancien modèle (« l'utilisateur choisit l'agent »).

**Besoin :** l'ouverture prend `(contexte, agent optionnel)`. La nature et l'agent par défaut
découlent du **type de contexte** (`ContextType` existe déjà). Le contexte est persisté dans la
session et immuable. Un choix d'agent explicite reste possible *dans l'ensemble que le type de
contexte autorise*. (Côté lymaille, l'issue sebt3/lymaille#4 reformule BO-3 dans ce sens.)

### B3. Contexte de niveau `Project` fourni par l'hôte

`DEC-20` : « niveau `Project` = partagé par toute l'instance : miryad-core n'a pas d'entité
projet ». Chez vanyline, un Project est une entité réelle (dépôt git, CRD, base). Deux demandes :

- permettre à l'hôte de **résoudre** un contexte de niveau projet depuis sa propre entité, pas
  seulement depuis le catalogue `AdminOnly` ;
- lever l'ambiguïté de nom (« Project » ne désigne pas la même chose des deux côtés).

### B4. Plafond de durée d'un run de sous-agent (`DEC-10`, `max_secs` ≤ 540 s)

Un agent de code qui compile et teste dure typiquement de 30 à 45 minutes. Le spike `OI-4` a montré
que `inactivity_timeout`/`abort_timeout` se règlent par service **ou par handler**.

**Proposition :** faire dispatcher le step `subagent` par un handler Restate dédié, aux bornes
relevées, pour ne pas retarder la détection des steps ordinaires bloqués. Le step reste
`retryable: false` ; la reprise sur session déterministe est inchangée.

### B5. Coordination entre processus (`DEC-24`)

« Un tour actif par session et par processus » ne protège pas contre deux réplicas de
l'application. Soit un verrou en base, soit une limite assumée par écrit (« une seule réplique »).
Le DSD évoque déjà une « bascule de bail » côté Matrix : le sujet existe.

### B6. `OI-10`, hooks de ressource synchrones

Si les Project et Sandbox de vanyline deviennent des `MiryadResource`, créer une ressource devrait
créer une CR Kubernetes, donc un effet asynchrone. Deux issues : hooks asynchrones avec connexion
(changement transversal) ou module hôte dédié aux actions Kubernetes, le module générique restant
pour les lectures. À arbitrer ; ne bloque pas si l'on choisit la seconde.

### B7. Flux d'événements : typés, extensibles, relisibles

- les événements SSE doivent être **typés et exportables en TypeScript** (ts-rs), pour remplacer le
  `ChatEvent` actuel du frontend ;
- une route de **lecture de l'historique** doit exister (aucun rejeu `Last-Event-ID`, `DEC-24`) :
  un rechargement de page doit retrouver la conversation ;
- les événements doivent être **extensibles par l'hôte** (voir § 4 : actions d'interface).

### B8. Hygiène : prénom dans un fichier commité

`dsd/miryad-core/lymaille.md` (`FR-10`, « Realized by ») contient un prénom. La règle du
développeur : aucun prénom dans les fichiers commités, utiliser « le développeur principal ». Un
`grep` sur les `.sdd` et `.md` en trouvera probablement d'autres.

## 4. Besoins qui découlent de la vision UX (à qualifier avec miryad-core)

La vision de vanyline est détaillée dans son propre dépôt. Seuls les besoins qui touchent
miryad-core sont repris ici.

- **Modules et toolsets fournis par l'hôte.** L'hôte doit pouvoir accorder à une session des modules
  Rhai qui lui sont propres (ex. : « présenter un tableau de bord à l'utilisateur »,
  « naviguer vers une page »). À confirmer : le montage des `Grants` accepte-t-il des modules
  d'hôte arbitraires, sous les mêmes règles d'autorité (`FR-4`) ?
- **Actions d'interface comme événements.** Quand le LLM « montre une page » ou « ouvre un tableau
  de bord éphémère », cela doit sortir comme un événement typé du flux de session (B7), que le
  frontend interprète. Aucun HTML ni code exécutable fourni par le LLM n'est interprété par
  l'application : seulement des descriptions déclaratives (type de widget + données).
- **Exécuter un script sans tour de LLM.** Des widgets de tableau de bord sont alimentés par un
  script (Rhai) qui appelle des modules (MCP de sandbox, toolsets). C'est le même moteur
  `ScratchpadEngine` avec ses `Grants`, mais **sans boucle d'inférence** et **sous l'identité de
  l'utilisateur qui regarde**. À qualifier : miryad-core expose-t-il (ou doit-il exposer) un
  « exécuteur de script à grants » réutilisable, distinct du step `rhai` du workflow, ou est-ce
  du ressort de l'hôte avec les briques de lymaille ?
- **Politique de sécurité de ces scripts.** Un script de widget est du code écrit par un projet
  (donc potentiellement par un contexte de moindre confiance, cf. thèse 3). Ses droits viennent du
  principal qui affiche le widget et des grants du contexte, jamais du contenu du script.

## 5. Hors périmètre de miryad-core (côté vanyline seulement)

Pour mémoire, afin de ne pas les rediscuter ici : tableaux de bord de projet et de sandbox
personnalisables, tableaux de bord complémentaires et leur widget de liste, widgets standard
(markdown, bar-chart, pie-graph, workflows), éventuelle extension aux pages de configuration,
modèle *projet → groupes d'artefacts → artefacts* (pages HTML de planification, à afficher dans un
iframe sandboxé), manipulation visuelle partagée d'un schéma, et intégration de la méthode
SpecDD + DSD à la boîte à outils JS de la sandbox.

## 6. Ordre de discussion proposé

1. **B1, B2, B3, B4** : conditions d'une intégration sans contournement.
2. **B5, B6, B7** : à trancher avant le code du lot 2 (le lot 2 est « posé, non spécifié »).
3. **§ 4** : à qualifier (modules d'hôte, exécuteur de script sans LLM).
4. **B8** : hygiène, quand on touche au fichier.

Le lot 1 est déjà largement codé et le lot 2 démarre : les amendements `DEC-20` (mode d'auth `host`)
et `DEC-24` (ouverture par contexte) sont ceux qui coûtent le moins s'ils sont faits avant les
specs `interactive/*` et `mcp_access.sdd`.

---

## 7. Décisions de séance (2026-11-03) — à reporter dans `dsd/miryad-core/lymaille.md`

*État : arbitrées en questions/réponses avec le développeur principal ; **pas encore écrites dans le DSD**.
Le code du lot 2 est à l'arrêt (travail non commité : `definitions/catalog_context.rs`,
`store/write_journal.rs`, `definitions/mod.rs`, `store/mod.rs`) jusqu'à la fin de l'amendement.*

### B1 — liaison MCP fournie par l'hôte (amende `DEC-20`)
- Aucune ligne `mcp_servers` : liaison **purement hôte**, pas de 3e mode d'auth dans la table.
- Un trait de l'hôte (`LymailleHost`, voir B3) porte `resolve_mcp(contexte, rôle, principal)` →
  `Refusé | Accordé{url, jeton, expiration}`, appelé à l'assemblage **et** à chaque appel d'outil.
  Un rôle non pourvu par l'hôte retombe sur la liaison de contexte classique (`mcp_servers`).
- Cache du jeton : à l'hôte seul ; miryad-core ne garde ni cache ni persistance du jeton.
- Batch : l'autorité est tranchée sous le `subject` du déclencheur (`FR-10`) ; subject absent = échec.
- `FR-9` : pas de contrôle à la sauvegarde (hooks synchrones) ; l'accord excessif échoue à l'usage via `resolve_mcp`.
- La session `Config` voit les liaisons hôte en lecture seule, informatif ; ne les édite pas.

### B2 — le contexte pilote la session (amende `FR-11`, `DEC-24`, `DEC-26`)
- L'ouverture prend `(contexte, agent optionnel)`. Nature (`Work`/`Config`), agent par défaut et ensemble
  d'agents autorisés sont **portés par le `ContextType`** du catalogue (3 champs, `AdminOnly` en écriture).
- Le contexte est persisté dans la session, immuable.
- Ouvrir une session `Config` : droit de l'utilisateur sur ce contexte (pas de statut admin implicite).
- Agent explicite hors ensemble autorisé : refus, erreur typée `MRD-LYMAILLE-*`.
- Pont Matrix : `!agent` remplacé par `!new <contexte> [agent]`.
- Impact code déjà fait : entité `context_types` (commit e556862) et migration lot 2 à compléter de 3 colonnes
  (clause « migration modifiable en place » à ré-acter, ou nouvelle migration).

### B3 — contexte fourni par l'hôte (amende `DEC-9`, `DEC-20`)
- Un seul trait `LymailleHost` : `resolve_mcp` (B1) + `resolve_context(id, principal) -> Option<ToolsetContext>`.
- Nouveau préfixe d'identifiant **`H_`** : résolu par l'hôte, jamais en table ; `classify()` l'aiguille vers le trait.
  Aucune écriture possible (REST, MCP, session Config).
- Niveau de contexte `Project` renommé **`Shared`** (catalogue `G_`) ; « projet » redevient le mot de l'hôte.
  Impacte `lymaille` amont (à signaler) et le code lot 2 (renommage local).

### B4 — durée des sous-agents (amende `DEC-10`)
- Handler Restate dédié au step `subagent`, bornes `inactivity_timeout`/`abort_timeout` relevées, plafond
  `max_secs` configurable par l'application (défaut 540 s inchangé). Step toujours `retryable: false`.
- Pas de nouveau spike (choix assumé du développeur principal ; risque : reprise non testée sur 45 min).

### B5 — réplicas (amende `DEC-24`)
- Limite écrite : sessions interactives **mono-réplique** (hub SSE en mémoire). Verrou en base rouvert au besoin HA.

### B6 — `OI-10`
- Direction retenue : **hooks asynchrones avec connexion** (changement transversal de `MiryadResource`).
  **Hors lot 2** ; à cadrer (voir questions ouvertes ci-dessous).

### B7 — événements (amende `DEC-24`, `DEC-25`)
- `SessionEvent` typé, export ts-rs ; variante `Host{kind: String, payload: JSON}` pour l'extension par l'hôte
  (la crate n'interprète jamais le payload).
- Route de lecture de l'historique : rend l'état persisté de la session (messages, appels d'outils) sous les mêmes
  types ts-rs ; le client lit l'historique puis s'abonne au SSE (pas de `Last-Event-ID`). Événements `Host` non rejoués.

### B8 — hygiène
- Aucun prénom dans `dsd/miryad-core/lymaille.md` (vérifié). Restent des prénoms dans `AGENTS.md`, `tooling.sdd`,
  `src/frontend.sdd`, `.opencode/agents/*` : hors lymaille, à traiter séparément.

### B6 — précision
- Solution dédiée `dsd/miryad-core/hooks-async.md` (hors lot 2) : hooks `MiryadResource` asynchrones avec connexion ;
  `OI-10` soldé en « oui, plus tard » dans `lymaille.md`. Rouvre le contrôle d'autorité à la sauvegarde (`FR-9`, `DEC-20`).

### §4 — modules hôte et scripts sans LLM (Solution `dsd/miryad-core/scripts-hote.md`, même Domain)
- (a) **`HostModule` + registre** (comme `ResourceReaders`) : `{nom, politique d'accès, déclaration read_only, fabrique(subject) -> rhai::Module}`.
  Noms réservés (`resources`, `result`, …) refusés par `assert!` au démarrage. Autorité `FR-4` appliquée par un **enrobage central** avant
  chaque appel de fonction (principal relu en base) ; `check_grants` (`FR-9`) lit la même politique. Mêmes modules en interactif et en batch
  (subject du déclencheur) ; l'absence de spectateur en batch est une erreur du module.
- (b) **`ScriptRunner` dans miryad-core** (feature `lymaille`) : `run(script, contexte, principal, limites)` → valeur sérialisée, sans boucle
  d'inférence, même assemblage que la session (modules hôte, `resources`, rôles MCP via `resolve_mcp`). Limites `ScriptLimits` fournies
  par l'application, défauts serrés, dépassement = erreur typée (pas de troncature silencieuse). Cache et concurrence : à l'hôte.
- (c) **Grants** = ceux du `ContextType` (toolsets/modules « widget »), jamais du contenu du script, et chaque appel est tranché sous le
  principal du spectateur. **Lecture seule imposée** (`ReadOnly`) : `resources` sans écriture ; modules hôte et rôles MCP admis seulement
  s'ils se déclarent `read_only`.

### Reste à faire
- **Fait (2026-11-03, non commité, en attente de relecture) :** `dsd/miryad-core/lymaille.md` amendé (FR-14, FR-15, DEC-27 à DEC-30, AC-18 à AC-20,
  OI-11, OI-12, OI-10 soldé, prénom retiré de FR-10) ; `hooks-async.md` et `scripts-hote.md` créés ; paysage mis à jour dans `dsd/miryad-core.md`.
- Relecture et accord du développeur principal, puis commit du DSD.
- Ré-aligner les specs lot 2 listées dans « Réalignement du 2026-11-03 » (SpecDD Realization de `lymaille.md`), acter l'exception de migration
  (colonnes du `ContextType`) dans `AGENTS.md`, signaler le renommage `Shared` à `lymaille` amont, trancher `OI-11` (spike 45 min) et `OI-12` (ts-rs ou utoipa).
- Relancer l'implémentation (travail non commité : `definitions/catalog_context.rs`, `store/write_journal.rs`, `definitions/mod.rs`, `store/mod.rs`).
