# Solution Design: Modèle de ressources

## Objective

Donner à une application consommatrice **un seul contrat par entité** (`MiryadResource`) que REST, MCP et le frontend
lisent tel quel, avec les migrations internes de la crate et des points d'extension métier (hooks CRUD).

## Scope and Non-goals

Sous la feature `db` (défaut ; absente du profil sans base). La Solution définit le trait, les politiques d'accès
déclaratives, les hooks et le `Migrator` interne. L'évaluation des politiques est dans `miryad-core/access-control`, les
surfaces dans `rest-api` et `mcp`.

## Applicable Domain Requirements

`miryad-core#CON-5` (migrations immuables), `miryad-core#CON-6` (préfixe `miryad_`), `miryad-core#CON-7` (fonctions
génériques sur `ConnectionTrait`), `miryad-core#FR-1` (parité REST/MCP).

## Functional Requirements

- **FR-1 - MUST:** Chaque entité est décrite par une unique implémentation de `MiryadResource`, lue telle quelle par
  les deux couches API, sans déclaration de politique dupliquée entre elles.
    - **Source:** `arch@c62a184`, « Modèle de ressources »
    - **Realized by:** SpecDD /src/resource.sdd
    - **Verified by:** `AC-1`

- **FR-2 - MUST:** `read_policy` et `write_policy` sont évaluées séparément (une entité peut être publique en lecture et
  restreinte en écriture). Les politiques sont `Public`, `OwnerOnly`, `Group(nom)` et `AdminOnly`.
    - **Source:** `arch@c62a184`, « Modèle de ressources »
    - **Realized by:** SpecDD /src/resource.sdd
    - **Verified by:** `AC-1`

- **FR-3 - MUST:** `owner_column()` est `Some` dès qu'une des deux politiques est `OwnerOnly` ; `None` désigne une
  entité sans propriétaire (référentiel partagé).
    - **Source:** `arch@c62a184`, « Modèle de ressources »
    - **Realized by:** SpecDD /src/resource.sdd, SpecDD /src/rbac.sdd
    - **Verified by:** `AC-2`

- **FR-4 - MAY:** `filter_column()` et `label_column()` déclarent respectivement la colonne texte filtrable en liste et
  le champ affiché comme libellé humain ; sans déclaration, pas de filtre et repli sur la clé primaire.
    - **Source:** `arch@c62a184`, « Modèle de ressources »
    - **Realized by:** SpecDD /src/resource.sdd

- **FR-5 - MAY:** Une entité peut intervenir sur `create`, `update` et `delete` par `before_create`, `before_update` et
  `before_delete`. `before_update` et `before_delete` reçoivent le `Model` relu avant la modification (celui sur lequel
  le RBAC a statué), en lecture seule. Par défaut les hooks ne changent rien.
    - **Source:** `arch@c62a184`, « Hooks métier CRUD »
    - **Realized by:** SpecDD /src/resource.sdd, SpecDD /src/rest/core.sdd
    - **Verified by:** `AC-3`

- **FR-6 - MUST:** Les hooks s'exécutent après la décision RBAC et **avant** les invariants de sécurité (suppression de
  la clé primaire du corps, injection du propriétaire), qui restent les derniers mots : un hook ne peut pas les
  contourner en mutant l'`ActiveModel`. Il n'existe ni hook de lecture ni hook « after ».
    - **Source:** `arch@c62a184`, « Hooks métier CRUD »
    - **Realized by:** SpecDD /src/rest/core.sdd
    - **Verified by:** `AC-3`

- **FR-7 - MUST:** Un hook s'applique à REST **et** à MCP par la même fonction. REST restitue un `HookError` en
  `422 Unprocessable Entity` (JSON `{code, message}`), MCP sur le code JSON-RPC `-32000` (le `code` de l'app dans
  `data`).
    - **Source:** `arch@c62a184`, « Hooks métier CRUD »
    - **Realized by:** SpecDD /src/rest/core.sdd, SpecDD /src/rest/error.sdd, SpecDD /src/mcp/mod.sdd
    - **Verified by:** `AC-3`

- **FR-8 - MUST:** Le schéma interne est géré par un `Migrator` embarqué, séparé des migrations métier de l'app, que
  l'app appelle explicitement au démarrage (`Migrator::up`). Ajouter une table interne revient à ajouter une migration
  et à l'enregistrer dans `Migrator::migrations()`, sans autre changement côté app.
    - **Source:** `arch@c62a184`, « Migrations »
    - **Realized by:** SpecDD /src/migration/mod.sdd
    - **Verified by:** `AC-4`

## Constraints

- **CON-1 - MUST NOT:** `HookError` n'est jamais une erreur de la crate : il ne porte pas de code `MRD-*`. Son `code`
  est libre, à la charge de l'app.
    - **Source:** `arch@c62a184`, « Hooks métier CRUD »
    - **Rationale:** la convention `MRD-*` identifie un problème dans le framework, pas une règle métier qui rejette
      une requête.
    - **Realized by:** SpecDD /src/resource.sdd

- **CON-2 - MUST:** Comparer deux valeurs de `Column` (tests, code métier) passe par `matches!(...)` ou par le nom
  (`Iden::to_string()`), jamais par `==`.
    - **Source:** `arch@c62a184`, « Modèle de ressources », point d'attention
    - **Rationale:** le `Column` généré par `DeriveEntityModel` (SeaORM 2.0) n'implémente pas `PartialEq` ni `Eq`.

## Design Decisions

- **DEC-1 - Migrateur interne séparé de celui de l'app**
    - **Decision:** le schéma interne est un `Migrator` de la crate (`sea-orm-migration`), que l'app rejoue ; les
      features `workflow` et `matrix` suivent la même convention de préfixe et impliquent `db`.
    - **Consequences:** aucune collision avec le schéma applicatif (CON-6 du Domain).
    - **Source:** `arch@c62a184`, « Migrations »

## Acceptance Criteria

**ASSUMPTION :** correspondance exacte avec les `Scenario` des specs non vérifiée dans la migration DSD.

- **AC-1:** Une entité qui implémente `MiryadResource` est exposée par REST et par MCP avec les mêmes politiques,
  sans déclaration supplémentaire.
- **AC-2:** Une entité en `OwnerOnly` sans `owner_column` est refusée (fail-closed), couvert par un test explicite.
- **AC-3:** Un hook qui refuse produit un `422` en REST et un code `-32000` en MCP ; un hook qui mute l'`ActiveModel` ne
  peut pas changer le propriétaire ni la clé primaire.
- **AC-4:** `Migrator::up` rejoué sur une base déjà migrée ne change rien.

## SpecDD Realization

- `SpecDD /src/resource.sdd` : le trait, `AccessPolicy`, `HookError`.
- `SpecDD /src/migration/mod.sdd` et ses migrations : le schéma interne.
- `SpecDD /src/rest/core.sdd` : la séquence d'appel des hooks.

## Open Issues

- **OI-1:** Les migrations `m20260923_000001` et `m20261006_000001` ont été modifiées en place par exception (voir
  `miryad-core#CON-5`). **TBD :** à la première mise en production, la règle reprend ; aucun mécanisme n'en alerte
  aujourd'hui.
