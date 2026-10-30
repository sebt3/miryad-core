# Solution Design: Support frontend

## Objective

Fournir au générateur de frontend du template `miryad` une représentation intermédiaire (IR) de chaque entité, et servir
une SPA statique sans collision avec l'API.

## Scope and Non-goals

La crate reste strictement backend : la génération du frontend (composants Vue, écrans CRUD, générateur TypeScript) vit
dans le template `miryad` (décision du 2026-08-23, item 8 livré). Deux briques seulement : l'IR (sous `db`) et le
service statique (feature `static-frontend`, **activée par défaut**).

## Applicable Domain Requirements

`miryad-core#CON-1` (bibliothèque), `miryad-core#CON-4` (gating de features).

## Functional Requirements

- **FR-1 - MUST:** `resource_ir::<E>()` rend un `EntityIr` : nom de ressource, champs, politiques de lecture et
  d'écriture, colonnes propriétaire, filtre et libellé. Chaque `FieldIr` porte `type`, `format`, `nullable`,
  `is_primary_key` et `references`.
    - **Source:** `arch@c62a184`, « Support frontend »
    - **Realized by:** SpecDD /src/ir.sdd
    - **Verified by:** `AC-1`

- **FR-2 - MUST:** Le type et le format sont dérivés de `ColumnDef::get_column_type()` : aucune annotation de l'app.
    - **Source:** `arch@c62a184`, « Support frontend »
    - **Realized by:** SpecDD /src/ir.sdd
    - **Verified by:** `AC-1`

- **FR-3 - MUST:** `FieldIr::references` ne retient que les relations `belongs_to` scalaires (colonne simple non
  composite). `IrRegistry` résout le nom de table SQL brut de la cible en `resource_name` à `write_to_file`, une fois
  toutes les entités enregistrées ; `None` si la cible n'est pas dans le même registre. Une FK composite n'est pas
  supportée (`None`).
    - **Source:** `arch@c62a184`, « FieldIr::references (relations, #19) » ; issue #19
    - **Realized by:** SpecDD /src/ir.sdd
    - **Verified by:** `AC-2`

- **FR-4 - MUST:** La production du fichier IR est à la charge de l'app (binaire dédié ou sous-commande), non un binaire
  de la crate. `IrRegistry` accumule l'IR de plusieurs entités et l'écrit en JSON.
    - **Source:** `arch@c62a184`, « Support frontend »
    - **Realized by:** SpecDD /src/ir.sdd
    - **Verified by:** `AC-1`

- **FR-5 - MUST:** `static_frontend_router(assets_dir)` sert le répertoire par `ServeDir` avec repli vers
  `assets_dir/index.html` pour toute route non capturée par l'API (routing SPA côté client). Les assets sont un
  répertoire externe, jamais embarqués dans le binaire.
    - **Source:** `arch@c62a184`, « Support frontend »
    - **Realized by:** SpecDD /src/frontend.sdd
    - **Verified by:** `AC-3`

## Constraints

- **CON-1 - MUST:** L'IR emploie le vocabulaire `type`/`format` d'OpenAPI, pas un enum maison.
    - **Source:** `arch@c62a184`, « Support frontend »
    - **Rationale:** ces chaînes sont stables et comprises par tout l'outillage JS/TS qui consomme de l'OpenAPI.

- **CON-2 - MUST NOT:** L'IR n'est jamais fusionnée avec `openapi.json`.
    - **Source:** `arch@c62a184`, « Support frontend »
    - **Rationale:** deux publics différents (consommateurs externes de l'API REST, outillage interne de scaffolding),
      donc deux contrats qui évoluent indépendamment.
    - **Realized by:** SpecDD /src/ir.sdd

- **CON-3 - MUST:** La clé primaire est détectée par `E::PrimaryKey::iter()` et `PrimaryKeyToColumn::into_column()`,
  comparée par nom (`Iden::to_string()`), puisque `Column` n'implémente pas `PartialEq`.
    - **Source:** `arch@c62a184`, « Support frontend »
    - **Realized by:** SpecDD /src/ir.sdd

- **CON-4 - MUST:** Pour lire `RelationDef::is_owner`, on suit la sémantique **réelle** : `belongs_to()` (où `Self` porte
  la FK) construit avec `is_owner: false` ; `has_one()` et `has_many()` avec `is_owner: true`.
    - **Source:** `arch@c62a184`, « Support frontend », point d'attention (vérifié dans le source de `sea-orm 2.0.2`)
    - **Rationale:** le doc-comment amont dit le contraire ; une lecture littérale ferait remonter la clé primaire comme
      `references` sur les relations `has_one` inversées.
    - **Realized by:** SpecDD /src/ir.sdd

## Design Decisions

- **DEC-1 - `static-frontend` activée par défaut**
    - **Context:** contrairement à `mcp` et `swagger-ui` (dépendances lourdes, opt-in), cette feature ne tire que
      `tower-http` et est attendue par la quasi-totalité des apps miryad.
    - **Decision:** `default = ["static-frontend"]`, désactivable par `default-features = false` pour un backend pur API.
    - **Source:** `arch@c62a184`, « Support frontend »

## SpecDD Realization

`SpecDD /src/ir.sdd`, `SpecDD /src/frontend.sdd`.

## Acceptance Criteria

**ASSUMPTION :** correspondance exacte avec les `Scenario` des specs non vérifiée dans la migration DSD.

- **AC-1:** L'IR d'une entité sans annotation supplémentaire donne les types OpenAPI de chaque colonne, et son fichier
  est écrit par l'app.
- **AC-2:** Une relation `belongs_to` vers une entité enregistrée donne son `resource_name` ; vers une entité absente,
  `None`.
- **AC-3:** Une route inconnue de l'API sert `index.html` ; une route d'API n'est jamais capturée.
