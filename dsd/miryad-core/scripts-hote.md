# Solution Design: Modules d'hôte et scripts sans LLM

## Objective

Permettre à l'application hôte d'accorder à une session `lymaille` ses propres modules Rhai (par exemple « présenter un tableau de bord »,
« naviguer vers une page ») sous les règles d'autorité de la crate, et d'exécuter un script Rhai à droits bornés, sans boucle d'inférence, sous
l'identité de celui qui le regarde.

## Scope and Non-goals

- **Ouverte le 2026-11-03**, rien n'est implémenté ni spécifié ; lot non fixé. Sous la feature `lymaille` (donc `db`).
- Couvre le registre des modules d'hôte, leur contrôle d'autorité, l'exécuteur de script.
- **Non-goal :** les widgets, tableaux de bord et pages de l'application `vanyline` ; le rendu de l'interface ; toute interprétation de HTML ou de code
  exécutable fourni par un LLM (l'application ne reçoit que des descriptions déclaratives).
- **Non-goal :** les `fs` et `shell` de `lymaille`, jamais activés (`miryad-core/lymaille#OI-1`).

## Applicable Domain Requirements

`miryad-core#CON-2`, `miryad-core#CON-3`, `miryad-core#CON-4`. `miryad-core/lymaille#FR-4` (principal relu à chaque appel), `FR-9` (une définition ne
peut accorder que ce que son propriétaire peut utiliser), `FR-10` (identité du déclencheur) et `DEC-27` (`LymailleHost`) s'appliquent.

## Functional Requirements

- **FR-1 - MUST:** L'hôte enregistre ses modules par un registre de `HostModule` : nom, politique d'accès, fabrique du module
  pour un `subject`. Un nom réservé (`resources`, `result`, …) ou déjà pris fait échouer l'enregistrement au démarrage.
    - **Source:** user instruction, 2026-11-03 ; précédent : `miryad-core/lymaille#FR-2` (`ResourceReaders`)
    - **Verified by:** `AC-1`

- **FR-2 - MUST:** La politique d'accès d'un `HostModule` est évaluée par un enrobage central **avant chaque appel** de fonction, principal relu en base
  (`miryad-core/lymaille#FR-4`) ; un module n'a pas à la réimplémenter. Le contrôle de droits d'une définition (`miryad-core/lymaille#FR-9`) lit la
  même politique.
    - **Source:** user instruction, 2026-11-03
    - **Verified by:** `AC-2`

- **FR-3 - MUST:** Les modules d'hôte sont disponibles en session interactive et dans les steps batch, par le même registre ; en batch, le `subject` est celui du
  déclencheur (`miryad-core/lymaille#FR-10`).
    - **Source:** user instruction, 2026-11-03
    - **Verified by:** `AC-2`

- **FR-4 - MUST:** Un `ScriptRunner` exécute un script Rhai avec les `Grants` d'un contexte, **sans boucle d'inférence**, sous le principal donné. Il réutilise
  l'assemblage de session : modules d'hôte, `resources`, toolsets accordés et leurs rôles MCP (`miryad-core/lymaille#DEC-27`). Il rend une valeur sérialisée. Un script peut agir (écrire une ressource, appeler un rôle MCP ou un module d'hôte) : un bouton de widget qui approuve une MR est un cas visé ; ce qu'un script rejoue ou non est l'affaire de l'hôte, qui décide quand il l'exécute.
    - **Source:** user instruction, 2026-11-03
    - **Verified by:** `AC-3`

- **FR-5 - MUST:** Les droits d'un script viennent du type du contexte et du principal qui l'exécute, jamais du contenu du script ni de son auteur. Chaque
  appel est tranché sous le principal courant.
    - **Source:** `NOTE-vanyline-orientation-lymaille.md` §4 ; user instruction, 2026-11-03
    - **Verified by:** `AC-3`

- **FR-6 - Retiré le 2026-11-11 (user instruction)** : la lecture seule imposée aux scripts était une erreur de relecture ; un script agit sous les droits de son spectateur, sans mode d'effets propre (`DEC-3`).

- **FR-7 - MUST:** Les `ScriptLimits` (durée, opérations Rhai, taille du résultat) sont fournies par l'application avec des défauts (5 s, 1 000 000 opérations, 256 KiB, arbitrage du 2026-11-11) ; un dépassement est une erreur
  typée `MRD-LYMAILLE-*`, jamais une troncature silencieuse. Cache et concurrence restent à l'hôte.
    - **Source:** user instruction, 2026-11-03
    - **Verified by:** `AC-5`

## Design Decisions

- **DEC-1 - Le `ScriptRunner` vit dans miryad-core** (2026-11-03)
    - **Context:** l'hôte pourrait l'écrire avec les briques de `lymaille`, mais il devrait réimplémenter la résolution des grants, l'autorité par appel et
      les rôles MCP que `miryad-core/lymaille#DEC-27` centralise.
    - **Decision:** l'exécuteur est livré par la crate.
    - **Alternatives:** exécuteur chez l'hôte (écarté : duplication de la chaîne d'autorité).
    - **Source:** user instruction, 2026-11-03

- **DEC-2 - Les grants d'un script sont ceux du type de contexte** (2026-11-03)
    - **Decision:** le type de contexte déclare les toolsets et modules accordables aux scripts ; le principal du spectateur est intersecté à chaque appel.
    - **Alternatives:** une famille `ScriptProfile` dédiée (écarté : une troisième définition à stocker, valider et exposer).
    - **Précision du 2026-11-11 :** `script_grants` = `{"toolsets":[…],"modules":[…]}` (toolsets du catalogue `G_…` et noms de `HostModule`), validé en forme par le hook du type de contexte (toolsets existants relus par la connexion) ; les noms de module sont vérifiés à l'exécution, un hook étant une fonction statique sans accès au registre `HostModules`.
    - **Consequences:** le type de contexte (`miryad-core/lymaille#DEC-28`) porte un champ de plus. **Arbitré le 2026-11-05 (user instruction)** : colonne JSON `script_grants` (toolsets et modules accordables aux scripts) sur la table des types de contexte, à côté de `allowed_agents`, ni réutilisation des toolsets de la session LLM ni champ amont ; coûte une retouche de migration en place de plus, à tracer dans `AGENTS.md` à l'ouverture du lot.
    - **Source:** user instruction, 2026-11-03

- **DEC-3 - Révisé le 2026-11-11 : pas de lecture seule imposée** (décidé le 2026-11-03, renversé)
    - **Decision:** le `ScriptRunner` ne restreint pas les effets : `resources` avec écritures, rôles MCP et modules d'hôte sans déclaration `read_only`. Un widget qui agit (approuver une MR par un bouton) est voulu ; les droits restent ceux du spectateur.
    - **Source:** user instruction, 2026-11-11 (renverse l'instruction du 2026-11-03)

- **DEC-4 - Registre des `HostModule` : valeur dédiée** (2026-11-11)
    - **Decision:** builder `HostModules::new().register(module)?` passé au montage, distinct de `LymailleHost` : la collision échoue au démarrage (`FR-1`).
    - **Source:** user instruction, 2026-11-11

- **DEC-5 - Le texte du script est fourni par l'hôte** (2026-11-11)
    - **Decision:** `ScriptRunner::run` reçoit le script en `&str` ; miryad-core n'en stocke aucun (`OI-3`).
    - **Source:** user instruction, 2026-11-11

- **DEC-6 - Le `ScriptRunner` tourne sur un `rhai::Engine` dédié** (2026-11-11)
    - **Decision:** un `Engine` propre à la crate monte les mêmes `rhai::Module` que l'assemblage ; opérations, durée et taille sont des limites Rhai natives, le résultat est un `Dynamic` converti en `serde_json::Value`.
      Le pad de `lymaille` rend un texte masqué et plafonné, sans plafond d'opérations : écarté.
    - **Source:** user instruction, 2026-11-11

## SpecDD Realization

Aucune.

## Acceptance Criteria

- **AC-1:** L'enregistrement d'un nom réservé ou en double échoue au démarrage.
- **AC-2:** Un `HostModule` réservé à l'admin est refusé à l'appel suivant le retrait du groupe, en interactif comme en batch.
- **AC-3:** Un script ne lit ni n'écrit rien de plus que ce que son spectateur peut ; un autre spectateur obtient un autre résultat.
- **AC-4:** Retiré avec `FR-6` (2026-11-11).
- **AC-5:** Un script qui dépasse la durée, les opérations ou la taille de résultat échoue en erreur typée.

## Open Issues

- **OI-1:** **Soldé le 2026-11-05 (user instruction)** : la politique d'accès d'un `HostModule` est une **énumération fermée** (`Authenticated`, `AdminOnly`), lisible par `check_grants`
  (`FR-9`) et évaluée par l'enrobage central (`FR-2`) ; pas de décision de l'hôte par appel.
- **OI-2:** **Soldé le 2026-11-05 (user instruction)** : lot 3 de `miryad-core`, **après** le lot 2 de `lymaille` **et** après `hooks-async` (arbitrage ci-dessous) ; les specs s'écrivent à
  l'ouverture du lot, pas avant.
- **OI-3:** **Soldé le 2026-11-05 (user instruction)** : fonction de bibliothèque seule (`ScriptRunner::run`) ; aucune route REST ni outil MCP dans la crate, donc pas de question de parité
  (`miryad-core#FR-1`). L'hôte l'expose à sa guise.
- **OI-4:** **Soldé le 2026-11-05 (user instruction)** : aucun cache dans la crate (`FR-7`). Le résultat dépend du principal et du contenu : une clé de cache correcte est celle de l'hôte.
- **OI-5:** **Soldé le 2026-11-11 (user instruction)** : les rôles MCP et toolsets sont montés dans un script comme dans une session, sans filtre d'outils ni lecture seule (`FR-6` retiré). Les modules natifs de `lymaille` (mémoire, todo) ne sont pas montés.
