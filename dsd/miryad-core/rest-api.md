# Solution Design: API REST et OpenAPI

## Objective

Exposer toute entité `MiryadResource` en REST CRUD sans route écrite à la main, et publier le contrat OpenAPI de ces
routes, avec ou sans base de données.

## Scope and Non-goals

La Solution couvre `resource_router`, la logique métier partagée (`rest::core`), la pagination et le filtre
(`src/query.rs`, réutilisés par MCP), `openapi_router` et `swagger_ui_router`. `rest::openapi` reste disponible sans
`db` ; le reste est sous `db`. Les routes de compte (`tokens`, `users`, `me`) sont décrites dans `auth` et
`access-control`.

Hors Solution pour l'instant : pagination par curseur, tri, filtre multi-champs, recherche texte, masquage de champ.

## Applicable Domain Requirements

`miryad-core#FR-1` (parité REST/MCP), `miryad-core#CON-3` (codes `MRD-REST-NNN`).

## Functional Requirements

- **FR-1 - MUST:** `resource_router::<E>()` monte `GET/POST /api/v1/{resource_name}` et
  `GET/PUT/DELETE /api/v1/{resource_name}/{id}` pour toute entité `RestEntity`, en réutilisant `MiryadAuthState`.
    - **Source:** `arch@c62a184`, « API REST générique »
    - **Realized by:** SpecDD /src/rest/mod.sdd
    - **Verified by:** `AC-1`

- **FR-2 - MUST:** Le préfixe `/api/v1` est figé dans la crate.
    - **Source:** `arch@c62a184`, « API REST générique » (feature 6)
    - **Rationale:** il élimine par construction la collision avec une route SPA du frontend dont le nom correspondrait
      à un `resource_name` ; l'app n'a plus à nester ses routeurs sous des préfixes ad hoc.
    - **Realized by:** SpecDD /src/rest/mod.sdd
    - **Verified by:** `AC-1`

- **FR-3 - MUST:** Le corps de requête et de réponse réutilise `E::Model` directement, sans DTO create/update par
  entité.
    - **Source:** `arch@c62a184`, « API REST générique »
    - **Realized by:** SpecDD /src/rest/mod.sdd
    - **Verified by:** `AC-1`

- **FR-4 - MUST:** La logique des cinq opérations (`list`, `get`, `create`, `update`, `delete` : RBAC, pagination,
  injection du propriétaire, `mark_all_set`) vit dans `rest::core`, indépendamment d'axum, et est réutilisée telle
  quelle par MCP.
    - **Source:** `arch@c62a184`, « API REST générique »
    - **Realized by:** SpecDD /src/rest/core.sdd
    - **Verified by:** `AC-2`

- **FR-5 - MUST:** À la création, la colonne `owner_column()` du corps client est ignorée et écrasée par l'id de
  l'utilisateur authentifié : jamais de création au nom d'un autre.
    - **Source:** `arch@c62a184`, « API REST générique »
    - **Realized by:** SpecDD /src/rest/core.sdd
    - **Verified by:** `AC-2`

- **FR-6 - MUST:** Un enregistrement existant mais non autorisé rend 403, pas 404.
    - **Source:** `arch@c62a184`, « API REST générique »
    - **Realized by:** SpecDD /src/rest/core.sdd, SpecDD /src/rest/error.sdd
    - **Verified by:** `AC-2`

- **FR-7 - MUST:** La pagination (`Pagination` : page 1-indexée, `per_page` défaut 100, plafond 1000 ; `PagedResult`) et
  le filtre de liste (`?filter=valeur`, égalité exacte sur `filter_column()`) sont combinés en `AND` avec la condition
  RBAC de `list_access`.
    - **Source:** `arch@c62a184`, « API REST générique »
    - **Realized by:** SpecDD /src/query.sdd, SpecDD /src/rest/core.sdd
    - **Verified by:** `AC-3`

- **FR-8 - MUST:** `resource_openapi::<E>()` produit un fragment OpenAPI pour les cinq routes d'une entité, avec les
  chemins du préfixe figé, et déclare un `SecurityScheme` HTTP Bearer `bearer_auth` (pas de schéma pour le cookie,
  qui est `HttpOnly` et chiffré). `OpenApi::merge` ne laisse qu'un seul `bearer_auth` quel que soit le nombre
  d'entités.
    - **Source:** `arch@c62a184`, « OpenAPI + Swagger UI » (roadmap 4b)
    - **Realized by:** SpecDD /src/rest/openapi.sdd
    - **Verified by:** `AC-4`

- **FR-9 - MUST:** `openapi_router` sert `GET /api/openapi.json` et reste compilé sans `db`. `swagger_ui_router`, derrière
  la feature `swagger-ui`, sert l'UI sur `/api/swagger-ui` et le document lui-même ; il ne se fusionne pas avec
  `openapi_router`.
    - **Source:** `arch@c62a184`, « OpenAPI + Swagger UI »
    - **Realized by:** SpecDD /src/rest/openapi.sdd
    - **Verified by:** `AC-4`

## Constraints

- **CON-1 - MUST:** `RestEntity` impose une unique colonne de clé primaire de type `i32`, encodée dans ses bornes
  associées, avec un blanket impl.
    - **Source:** `arch@c62a184`, « API REST générique » (contrainte assumée)
    - **Realized by:** SpecDD /src/rest/mod.sdd

- **CON-2 - MUST:** `mark_all_set::<E>()` est appelé avant tout `insert` ou `update`.
    - **Source:** `arch@c62a184`, « API REST générique », point d'attention
    - **Rationale:** `Model::into_active_model()` marque tous les champs `Unchanged` ; `update()` n'inclut que les
      champs `Set`, donc un `Model` reçu tel quel n'écrirait silencieusement aucune colonne.
    - **Realized by:** SpecDD /src/rest/core.sdd

- **CON-3 - MUST:** Toute entité qui dérive `ToSchema` porte aussi `#[schema(as = NomUnique)]`.
    - **Source:** `arch@c62a184`, « OpenAPI + Swagger UI », point d'attention
    - **Rationale:** toute entité SeaORM s'appelle `Model` ; deux entités sans renommage produisent un même nom de
      schéma et `OpenApi::merge` garde silencieusement le premier.
    - **Realized by:** SpecDD /src/rest/openapi.sdd

- **CON-4 - MUST:** Le document OpenAPI est construit par l'API bas niveau d'`utoipa` (`OperationBuilder`,
  `ObjectBuilder`, `Paths::add_path_operation`), pas par la macro `#[utoipa::path]` ni par `utoipa-axum`.
    - **Source:** `arch@c62a184`, « OpenAPI + Swagger UI »
    - **Rationale:** ils exigent une fonction concrète par route, incompatible avec des handlers génériques par
      entité. `PagedResult<M>` n'a pas de `ToSchema` : son schéma est écrit à la main.
    - **Realized by:** SpecDD /src/rest/openapi.sdd

- **CON-5 - MUST:** `swagger_ui_router` emploie des chemins absolus plutôt qu'un `.nest("/api", ...)` externe.
    - **Source:** `arch@c62a184`, « OpenAPI + Swagger UI »
    - **Rationale:** `.url(...)` est aussi l'URL que le JS de Swagger UI interroge ; un nest désynchroniserait la route
      montée de celle interrogée.
    - **Realized by:** SpecDD /src/rest/openapi.sdd

## Design Decisions

- **DEC-1 - Pas de curseur, de tri ni de filtre multi-champs au départ**
    - **Context:** gap réel mais pas MVP.
    - **Decision:** le filtre reste limité à une colonne en égalité exacte, jusqu'à une première application miryad
      réelle (décidé le 2026-08-23).
    - **Consequences:** voir `OI-2`.
    - **Source:** `roadmap@c62a184`, item 10

## SpecDD Realization

`SpecDD /src/rest/mod.sdd`, `SpecDD /src/rest/core.sdd`, `SpecDD /src/rest/error.sdd`, `SpecDD /src/rest/openapi.sdd`,
`SpecDD /src/query.sdd`.

## Acceptance Criteria

**ASSUMPTION :** correspondance exacte avec les `Scenario` des specs non vérifiée dans la migration DSD.

- **AC-1:** Une entité `RestEntity` répond sur les cinq routes sans route écrite à la main.
- **AC-2:** Un non-admin ne crée jamais au nom d'autrui ; un enregistrement non autorisé rend 403.
- **AC-3:** La page 2 d'une liste filtrée respecte à la fois le filtre et le RBAC.
- **AC-4:** Le document fusionné de deux entités contient deux schémas distincts et un seul `bearer_auth`.

## Open Issues

- **OI-1:** **Fragment OpenAPI des routes de compte** (arbitré le 2026-09-29) : `GET /api/v1/me`, `GET /api/v1/users`
  et `/api/v1/tokens` sont absentes du document OpenAPI, dont le template `miryad` aura besoin pour son client. Ces
  routes restent REST-only par nature (exemption de parité) ; `/auth/*` reste hors document. Non MVP, à planifier.
    - **Source:** `roadmap@c62a184`, « À planifier, non MVP »
- **OI-2:** **Filtrage et tri étendus** (item 10, décidé le 2026-08-23) : filtre multi-critères, tri et recherche texte,
  à reprendre une fois une première application miryad réelle construite dessus, pour caler le besoin sur un usage
  concret. **TBD.**
    - **Source:** `roadmap@c62a184`, item 10
