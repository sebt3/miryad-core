# Solution Design: Modules d'hôte et scripts sans LLM

## Objective

Permettre à l'application hôte d'accorder à une session `lymaille` ses propres modules Rhai (par exemple « présenter un tableau de bord »,
« naviguer vers une page ») sous les règles d'autorité de la crate, et d'exécuter un script Rhai à droits bornés, sans boucle d'inférence, sous
l'identité de celui qui le regarde.

## Scope and Non-goals

- **Ouverte le 2026-11-03**, rien n'est implémenté ni spécifié ; lot non fixé. Sous la feature `lymaille` (donc `db`).
- Couvre le registre des modules d'hôte, leur contrôle d'autorité, l'exécuteur de script et sa politique d'effets.
- **Non-goal :** les widgets, tableaux de bord et pages de l'application `vanyline` ; le rendu de l'interface ; toute interprétation de HTML ou de code
  exécutable fourni par un LLM (l'application ne reçoit que des descriptions déclaratives).
- **Non-goal :** les `fs` et `shell` de `lymaille`, jamais activés (`miryad-core/lymaille#OI-1`).

## Applicable Domain Requirements

`miryad-core#CON-2`, `miryad-core#CON-3`, `miryad-core#CON-4`. `miryad-core/lymaille#FR-4` (principal relu à chaque appel), `FR-9` (une définition ne
peut accorder que ce que son propriétaire peut utiliser), `FR-10` (identité du déclencheur) et `DEC-27` (`LymailleHost`) s'appliquent.

## Functional Requirements

- **FR-1 - MUST:** L'hôte enregistre ses modules par un registre de `HostModule` : nom, politique d'accès, déclaration `read_only`, fabrique du module
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
  l'assemblage de session : modules d'hôte, `resources`, rôles MCP (`miryad-core/lymaille#DEC-27`). Il rend une valeur sérialisée.
    - **Source:** user instruction, 2026-11-03
    - **Verified by:** `AC-3`

- **FR-5 - MUST:** Les droits d'un script viennent du type du contexte et du principal qui l'exécute, jamais du contenu du script ni de son auteur. Chaque
  appel est tranché sous le principal courant.
    - **Source:** `NOTE-vanyline-orientation-lymaille.md` §4 ; user instruction, 2026-11-03
    - **Verified by:** `AC-3`

- **FR-6 - MUST:** Le `ScriptRunner` impose la **lecture seule** : le module `resources` est monté sans écritures, un module d'hôte ou un rôle MCP n'est
  monté que s'il se déclare `read_only`.
    - **Source:** user instruction, 2026-11-03
    - **Verified by:** `AC-4`

- **FR-7 - MUST:** Les `ScriptLimits` (durée, opérations Rhai, taille du résultat) sont fournies par l'application avec des défauts serrés ; un dépassement est une erreur
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
    - **Consequences:** le type de contexte (`miryad-core/lymaille#DEC-28`) porte un champ de plus ; **ASSUMPTION :** à confirmer à la spec.
    - **Source:** user instruction, 2026-11-03

- **DEC-3 - Lecture seule imposée par la crate** (2026-11-03)
    - **Context:** un script de widget s'exécute à chaque affichage, par un spectateur qui n'a rien demandé d'écrire, et un rechargement le rejoue.
    - **Decision:** mode `ReadOnly` unique du `ScriptRunner` (`FR-6`) ; la déclaration `read_only` d'un `HostModule` engage l'hôte, pas la crate.
    - **Alternatives:** aucune politique d'effets (écarté : un affichage déclencherait des écritures ou des actions externes).
    - **Source:** user instruction, 2026-11-03

## SpecDD Realization

Aucune.

## Acceptance Criteria

- **AC-1:** L'enregistrement d'un nom réservé ou en double échoue au démarrage.
- **AC-2:** Un `HostModule` réservé à l'admin est refusé à l'appel suivant le retrait du groupe, en interactif comme en batch.
- **AC-3:** Un script d'un contexte de moindre confiance ne lit rien de plus que ce que son spectateur peut lire ; un autre spectateur obtient un autre résultat.
- **AC-4:** Un script qui appelle une écriture, un module ou un rôle non `read_only` est refusé.
- **AC-5:** Un script qui dépasse la durée, les opérations ou la taille de résultat échoue en erreur typée.

## Open Issues

- **OI-1:** **Signature de `HostModule` et de la politique d'accès** : énumération fermée (`Authenticated`, `AdminOnly`) ou décision de l'hôte par un appel. **TBD.**
- **OI-2:** **Lot** : avant ou après le lot 2 de `miryad-core/lymaille` ; dépendance aux champs du type de contexte (`DEC-28`). **TBD.**
- **OI-3:** **Surface de l'exécuteur** : fonction de bibliothèque seule, ou aussi route REST ? Une route serait une surface de plus à placer vis-à-vis de la parité
  (`miryad-core#FR-1`). **TBD.**
- **OI-4:** **Cache des résultats** : à l'hôte (`FR-7`), mais un script lent rejoué par plusieurs spectateurs pose la question d'un point d'ancrage dans la crate. **TBD.**
