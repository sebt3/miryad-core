# Architecture — miryad-core

Référence du système tel qu'il existe aujourd'hui, organisée par thème (pas par ordre
d'implémentation — pour ça, `git log`). Complète `AGENTS.md` (qui donne la vue d'ensemble et la
cible) en documentant les contrats réels et les points d'attention qui ne se voient pas à la
lecture rapide du code. Mise à jour au fil des features : chaque section reflète l'état courant,
pas l'historique de comment on y est arrivé.

## Modèle de ressources — `MiryadResource`

Contrat central (`src/resource.rs`) sur lequel REST, GraphQL, MCP et le frontend générique se
greffent — une seule implémentation par entité SeaORM, lue telle quelle par les trois couches API
(pas de déclaration de politique dupliquée entre elles).

```rust
pub trait MiryadResource: EntityTrait {
    fn resource_name() -> &'static str;
    fn read_policy() -> AccessPolicy;
    fn write_policy() -> AccessPolicy;
    fn owner_column() -> Option<Self::Column>;
    fn filter_column() -> Option<Self::Column> { None }  // défaut : pas de filtre de liste
}

pub enum AccessPolicy { Public, OwnerOnly, Group(&'static str), AdminOnly }
```

- `read_policy`/`write_policy` sont évaluées séparément — une entité peut être publique en lecture
  et restreinte en écriture (cas "recettes partagées, modifiables par leur auteur uniquement").
- `owner_column()` retourne `None` pour les entités sans notion de propriétaire (référentiel
  partagé). Doit être `Some` si une des deux politiques est `OwnerOnly` — non vérifié par le
  compilateur, seulement par test (cf. section RBAC, "fail-closed").
- `filter_column()` (défaut `None`) déclare la colonne texte filtrable en liste par l'API REST
  (`?filter=valeur`, égalité exacte) — cf. section "API REST générique".

**Point d'attention** : le `Column` généré par `DeriveEntityModel` (SeaORM 2.0) n'implémente pas
`PartialEq`/`Eq` (seulement `Copy, Clone, Debug, EnumIter, DeriveColumn`). Comparer une valeur de
`Column` (tests, ou code métier) passe par `matches!(...)`, pas par `==`.

## Migrations

Schéma interne géré par un `Migrator` (`sea-orm-migration`) embarqué dans le crate (`src/
migration/`), séparé des migrations métier de l'app consommatrice — elle l'appelle explicitement
au démarrage : `miryad_core::migration::Migrator::up(&db, None).await`. Toutes les tables internes
sont préfixées `miryad_` (`miryad_api_tokens`, `miryad_users`, `miryad_groups`,
`miryad_group_memberships`) pour ne jamais collisionner avec le schéma applicatif.

Ajouter une table interne = ajouter un fichier `m<date>_<numéro>_<nom>.rs` et l'enregistrer dans
`Migrator::migrations()` — rien d'autre à changer côté app (elle rejoue simplement `Migrator::up`).

## Authentification — OIDC + session cookie

Flow OIDC navigateur (Authorization Code), généralisé depuis le pattern de `vanyline` : pas
d'`AppState` concret imposé. `MiryadAuthState` (état minimal requis par l'auth) est composé dans
l'`AppState` de l'app consommatrice via le pattern `FromRef` standard d'axum ; `auth_router<S>()`
et les extracteurs (`AuthUser`, `AuthPrincipal`) sont génériques sur `S` tant que
`MiryadAuthState: FromRef<S>` — le mécanisme d'extraction natif d'axum
(`State<T>: FromRequestParts<S> where T: FromRef<S>`) suffit, aucune fonction manuelle générique
n'est nécessaire côté handlers.

```rust
pub struct MiryadAuthState {
    pub oidc_client: Arc<dyn OidcClientTrait>,
    pub cookie_key: cookie::Key,
    pub post_login_redirect: String,
    pub post_logout_redirect: String,
    pub db: DatabaseConnection,   // requis pour valider un token API et résoudre un User
}

pub fn auth_router<S>() -> Router<S>
where S: Clone + Send + Sync + 'static, MiryadAuthState: FromRef<S>;
```

`OidcClientTrait::exchange_code` retourne `OidcLoginResult { identity: OidcIdentity, groups:
Vec<String> }` : `identity` (`id_token`/`subject`/`email`) est ce qui finit dans le cookie de
session ; `groups` (claim `groups`, spécifique à Authentik — pas standard OIDC, extrait à la main
du payload JWT déjà vérifié plutôt que de reconfigurer `CoreClient` avec des `AdditionalClaims`
génériques pour un seul champ) est éphémère, consommé une seule fois par la synchronisation de
groupes (cf. section RBAC), jamais persisté dans le cookie.

Le cookie de session (`miryad_session`) porte un payload JSON chiffré (`id_token`/`subject`/
`email`) — pas de délimiteur `|` façon vanyline, pour ne dépendre d'aucun caractère absent des
claims. Le cookie transitoire CSRF/nonce (`miryad_oidc_pending`) suit le même principe (secret
chiffré, `Max-Age=300`).

`AuthUser { subject, email, id_token }` (extracteur cookie-only) reste spécifique au flow
navigateur — cf. section suivante pour `AuthPrincipal`, le type réellement consommé par les API.

Erreurs : `AuthError` (`src/auth/error.rs`), préfixe `MRD-AUTH-XXX`, implémente `IntoResponse`
(401 non-authentifié/session invalide, 502 erreur OIDC amont, 401 token invalide/expiré, 500
erreur base).

## Tokens API + dual-auth

```rust
// src/auth/token.rs
pub type ApiToken = Entity;
pub struct Model {
    pub id: i32,
    pub subject: String,           // pas de FK vers User — résolu par requête, cf. section RBAC
    pub name: String,
    pub token_hash: String,        // SHA-256 hex — jamais le token en clair
    pub created_at: DateTimeUtc,
    pub expires_at: Option<DateTimeUtc>,
    pub last_used_at: Option<DateTimeUtc>,
}

pub async fn issue_token(db, subject, name, expires_at) -> Result<IssuedToken, AuthError>;
pub async fn validate_token(db, token) -> Result<AuthPrincipal, AuthError>;
pub async fn revoke_token(db, id) -> Result<(), AuthError>;
pub async fn ensure_token(db, subject, name, token, expires_at) -> Result<(), AuthError>;
```

Format du token émis par `issue_token` : `mrd_<43 car. base64url>` (32 octets aléatoires, préfixe
façon GitHub/Stripe), haché en SHA-256 avant stockage (pas un KDF lent : le secret est déjà
haute-entropie, pas un mot de passe humain).

`ensure_token` diffère d'`issue_token` : la valeur du token est **fournie par l'appelant** (pas
générée), et l'opération est idempotente (aucun doublon si un token avec ce hash existe déjà pour
ce `subject`). C'est le socle de `users::ensure_service_account` (`src/users/service_account.rs`) :
compose `resolve_user` + `sync_group_memberships` + `ensure_token` pour garantir l'existence d'un
compte "machine" (jamais de login OIDC — pensé pour l'automatisation de déploiement, ex. kuberest),
membre de groupes donnés, authentifiable par un secret connu à l'avance (typiquement une variable
d'environnement lue par l'app cible à son démarrage, après ses migrations — l'app décide
elle-même si et quand appeler cette fonction, miryad-core n'automatise rien). Si le secret change
côté app, l'appel suivant **ajoute** un nouveau token sans supprimer l'ancien, qui reste valide
jusqu'à révocation explicite (`revoke_token`).

`AuthPrincipal` (`src/auth/principal.rs`) est le type unifié que REST/GraphQL/MCP consomment,
produit soit par le cookie de session soit par un token API :

```rust
pub struct AuthPrincipal { pub subject: String, pub email: Option<String>, pub source: PrincipalSource }
pub enum PrincipalSource { Session { id_token: String }, ApiToken { token_id: i32 } }
```

L'extracteur dual-auth (`src/auth/dual.rs`) résout dans cet ordre : `Authorization: Bearer
<token>` d'abord, cookie de session en repli. Un header `Bearer` présent mais invalide ne retombe
**pas** sur le cookie — c'est le choix explicite du client, son échec est final (pas de repli
silencieux vers un mode plus faible).

## Utilisateurs & Groupes / RBAC

**Décision structurante : synchronisation, pas gestion.** Authentik est la seule source de vérité
pour l'appartenance aux groupes (`admin` compris) — miryad-core ne fournit **aucune API
d'assignation manuelle**. À chaque login OIDC réussi, `handler_callback` enchaîne `resolve_user`
(get-or-create par `subject`) puis `sync_groups_from_oidc`, qui réconcilie **entièrement** les
`GroupMembership` locales depuis le claim `groups` : ajout des groupes présents (création à la
volée d'un `Group` jamais vu — pas de registre préalable), retrait de ceux qui ne le sont plus. Le
groupe `admin` est *seedé* par migration (toujours présent, vide au départ), sans rien de spécial
au niveau schéma — juste une convention lue par `rbac::is_admin`.

**Limite acceptée** : un principal résolu depuis un token API n'a pas de session OIDC vivante par
requête — son état de groupe reflète le *dernier login navigateur* de son `subject`, pas l'état
Authentik en temps réel. Une révocation de groupe ne prend effet sur les tokens existants qu'après
la prochaine connexion navigateur de la personne concernée.

```rust
// src/users/{user,group,membership}.rs — génériques sur C: ConnectionTrait (pas &DatabaseConnection
// en dur : le seed de migration les appelle via manager.get_connection(), qui satisfait le même trait)
pub async fn resolve_user<C>(db: &C, subject: &str, email: Option<&str>) -> Result<user::Model, DbErr>;
pub async fn sync_groups_from_oidc<C>(db: &C, user_id: i32, groups: &[String]) -> Result<(), DbErr>;
pub async fn is_admin<C>(db: &C, user_id: i32) -> Result<bool, DbErr>;
pub async fn is_member<C>(db: &C, user_id: i32, group_name: &str) -> Result<bool, DbErr>;

// src/rbac.rs
pub async fn can_read<E: MiryadResource>(db, user: &user::Model, record: &E::Model) -> Result<bool, DbErr>;
pub async fn can_write<E: MiryadResource>(...) -> Result<bool, DbErr>;   // même signature, write_policy()
pub async fn can_create<E: MiryadResource>(db, user: &user::Model) -> Result<bool, DbErr>;
pub async fn list_access<E: MiryadResource>(db, user: &user::Model) -> Result<ListAccess, DbErr>;

pub enum ListAccess { Unrestricted, FilterByOwner(Condition), Forbidden }
```

- `can_read`/`can_write` évaluent un enregistrement déjà chargé. `OwnerOnly` compare via
  `record.get(owner_column)` — réflexion générique `sea_orm::ModelTrait`, aucune entité n'écrit
  son propre code de comparaison. Admin gagne toujours, sur toutes les politiques sauf `Public`.
  `OwnerOnly` sans `owner_column` → refuse (fail-closed), couvert par un test explicite.
- `can_create` n'a pas de record à comparer : `Public`/`OwnerOnly` autorisent toujours (le
  créateur devient propriétaire), `Group`/`AdminOnly` vérifient l'appartenance.
- `list_access` construit la condition de filtrage d'une liste plutôt qu'un booléen : pas de
  restriction, une condition `WHERE owner_column = user.id`, ou un refus complet avant même de
  construire la requête (`Group`/`AdminOnly` sans appartenance).

## API REST générique

```rust
// src/rest/mod.rs
pub trait RestEntity:
    MiryadResource<
        Model: Serialize + DeserializeOwned + IntoActiveModel<Self::ActiveModel> + Sync,
        ActiveModel: ActiveModelTrait<Entity = Self> + Send,
        PrimaryKey: PrimaryKeyTrait<ValueType = i32> + PrimaryKeyToColumn<Column = Self::Column>,
    >
{
}

pub fn resource_router<E: RestEntity, S>() -> Router<S>
where S: Clone + Send + Sync + 'static, MiryadAuthState: FromRef<S>;
```

Monte `GET/POST /{resource_name}` et `GET/PUT/DELETE /{resource_name}/{id}` pour toute entité
`RestEntity` — aucune route à écrire à la main par entité, aucun nouvel état à composer (réutilise
`MiryadAuthState`). Le corps de requête/réponse réutilise `E::Model` directement (pas de DTO
Create/Update par entité), grâce à `IntoActiveModel<E::ActiveModel>` généré par
`DeriveEntityModel`. **Contrainte assumée** : une seule colonne de clé primaire, de type `i32`
(vrai pour toutes les entités du crate à ce jour) — encodée dans les bornes associées de
`RestEntity` (`Model:`/`ActiveModel:`/`PrimaryKey:`), avec un blanket impl : aucune méthode
supplémentaire à implémenter par entité.

**Point d'attention** : `Model::into_active_model()` (généré par `DeriveEntityModel`) marque
**tous** les champs `Unchanged`, jamais `Set` — y compris hors primary key.
`ActiveModelTrait::insert()` traite `Unchanged` comme une valeur à écrire, mais
`ActiveModelTrait::update()` n'inclut que les champs `Set` dans la clause `SET` : un `Model` reçu
tel quel et directement passé à `.update()` n'écrirait donc silencieusement aucune colonne.
`mark_all_set::<E>()` (`src/rest/core.rs`) corrige ça par réflexion générique
(`ActiveModelTrait::get`/`set`, itération sur `E::Column`) avant tout `insert`/`update` — aucune
entité n'a besoin d'en tenir compte.

La logique métier des 5 opérations (`list`/`get`/`create`/`update`/`delete` : résolution RBAC,
pagination, injection du propriétaire, `mark_all_set`) vit dans `src/rest/core.rs`,
indépendamment d'axum — les handlers de `rest/mod.rs` ne font qu'extraire les paramètres et
envelopper le résultat en `Json<...>`. Pensé pour être réutilisé tel quel par d'autres surfaces
d'API sur les mêmes entités (ex. MCP), sans dupliquer les règles RBAC/pagination.

Pagination et filtre de liste (`src/query.rs`, partagé — pas spécifique REST, réutilisable par
GraphQL/MCP) :

```rust
pub struct Pagination { pub page: u64, pub per_page: u64 }  // page 1-indexée, per_page défaut 100, plafond 1000
pub struct PagedResult<M> { pub items: Vec<M>, pub page: u64, pub per_page: u64, pub total_items: u64, pub total_pages: u64 }
```

via le `Paginator` déjà intégré à SeaORM (`Select::paginate` + `num_items_and_pages`). Le filtre
(`?filter=valeur`, sur `MiryadResource::filter_column()` si l'entité en déclare un) est combiné en
`AND` avec la condition RBAC de `list_access` — un non-admin filtrant par catégorie ne voit jamais
les enregistrements d'autrui, même si la catégorie correspond.

À la création, la colonne `owner_column()` du corps client est ignorée et écrasée par l'id de
l'utilisateur authentifié — jamais de création au nom de quelqu'un d'autre.

Pas de pagination par curseur, pas de tri, pas de filtre multi-champs, pas de masquage de champ.
403 (pas 404) pour un enregistrement existant mais non autorisé.

### OpenAPI + Swagger UI

`utoipa` est une dépendance normale (pas optionnelle) — la génération `/openapi.json` est
toujours disponible. Seule la UI Swagger (`utoipa-swagger-ui`) est optionnelle, derrière la
feature Cargo `swagger-ui`.

```rust
// src/rest/openapi.rs
pub trait OpenApiEntity: RestEntity<Model: utoipa::ToSchema> {}

/// Fragment pour les 5 routes CRUD d'une entité — à fusionner (OpenApi::merge) avec celui des
/// autres entités montées avant publication. Ne fixe pas `info` (titre/version) : l'app les
/// renseigne sur le document final après fusion.
pub fn resource_openapi<E: OpenApiEntity>() -> utoipa::openapi::OpenApi;

/// Sert GET /openapi.json — toujours disponible.
pub fn openapi_router<S>(spec: OpenApi) -> axum::Router<S>;

/// Sert Swagger UI sur /swagger-ui, qui sert aussi /openapi.json lui-même (mécanisme natif
/// d'utoipa-swagger-ui) — derrière la feature "swagger-ui". Ne pas fusionner avec
/// openapi_router : les deux enregistreraient une route pour /openapi.json.
#[cfg(feature = "swagger-ui")]
pub fn swagger_ui_router<S>(spec: OpenApi) -> axum::Router<S>;
```

Construit via l'API bas niveau d'`utoipa` (`OperationBuilder`, `ObjectBuilder`, `Paths::
add_path_operation`...), pas la macro `#[utoipa::path]` (qui exige une fonction concrète par
route, incompatible avec des handlers génériques par entité) ni `utoipa-axum`/`OpenApiRouter`
(même contrainte). L'enveloppe de pagination (`PagedResult<M>`, `src/query.rs`) n'a pas de
`ToSchema` propre : son schéma est construit à la main (`items`/`page`/`per_page`/`total_items`/
`total_pages`) pour éviter la friction des génériques avec `ToSchema` côté utoipa.

**Point d'attention** : `ToSchema::name()` retourne par défaut le nom nu du type — et toute
entité SeaORM s'appelle `Model` (convention `DeriveEntityModel`). Sans renommage, deux entités
montées dans la même app produiraient donc un **même** nom de schéma (`Model`), et `OpenApi::merge`
garderait silencieusement le premier en ignorant le second. Toute entité qui dérive `ToSchema` doit
donc aussi porter `#[schema(as = NomUnique)]` (ex. `#[schema(as = Recipe)]`) — pas optionnel,
contrairement à ce qu'un `#[derive(ToSchema)]` nu laisserait penser.

## API GraphQL

Schéma généré dynamiquement par Seaography 2.0 (pas de codegen) depuis les entités `MiryadResource`
montées par l'app, RBAC réellement appliqué via un pont maison vers `rbac.rs` — pas le RBAC natif
de Seaography/SeaORM (`db.load_rbac()`/`RestrictedConnection`). Deux features Cargo distinctes :
`graphql` (le cœur) et `graphiql` (le client interactif, dépend de `graphql`).

**Décision : pas le RBAC natif de Seaography/SeaORM.** Il est table-level (grants
`select`/`insert`/`update`/`delete` par rôle par table entière, pas de notion de ligne) et
suppose un seul rôle par utilisateur (`RbacUserId` → un rôle, avec hiérarchie DAG) — deux
incompatibilités de fond avec ce qu'on a déjà : `AccessPolicy::OwnerOnly` (row-level) et le
multi-groupe synchronisé depuis Authentik (feature 3). L'adopter aurait fait vivre deux modèles
RBAC parallèles, incohérents entre REST et GraphQL sur la même entité. Vérifié en lisant le
billet SeaORM 2.0 RBAC (pas seulement le nom de la feature Cargo `rbac`).

**Pont via `LifecycleHooksInterface`** (`seaography`, trait synchrone sauf `entity_watch`) :

```rust
pub trait LifecycleHooksInterface: Send + Sync {
    fn entity_guard(&self, ctx: &ResolverContext, entity: &str, action: OperationType) -> GuardAction;
    fn entity_filter(&self, ctx: &ResolverContext, entity: &str, action: OperationType) -> Option<Condition>;
    // field_guard / entity_watch / before_active_model_save : défauts (Allow / no-op), non utilisés
}
// OperationType: Read | Create | Update | Delete — GuardAction: Allow | Block(Option<String>)
```

`entity_guard` bloque un accès entier (`AdminOnly`/`Group` sans appartenance) ; `entity_filter`
ajoute une `Condition` de requête pour `OwnerOnly` — l'équivalent GraphQL de
`rbac::ListAccess::FilterByOwner`, construite par **nom de colonne** (`sea_query::Expr::col(Alias::
new(nom)).eq(user_id)`, via le trait `ExprTrait`) plutôt que par `Column` typé : Seaography
identifie une entité par son nom (`&str`) à l'exécution, pas par son type.

**Contrainte structurante : hooks synchrones, précalcul obligatoire.** `entity_guard`/
`entity_filter` ne peuvent pas faire de requête DB (`is_admin`/`is_member` sont `async`). Le
principal (statut admin + ensemble des groupes) est donc résolu **une fois par requête HTTP**,
avant `schema.execute(...)`, puis injecté comme donnée de requête (`req.data(...)`, mécanisme
standard async-graphql) :

```rust
// src/graphql/{registry,principal,hooks,handler}.rs
pub struct EntityPolicy { pub read: AccessPolicy, pub write: AccessPolicy, pub owner_column: Option<String> }
pub struct PolicyRegistry { .. }
impl PolicyRegistry { pub fn register<E: MiryadResource>(&mut self) -> &mut Self; }

pub struct GraphQlPrincipal { pub user_id: i32, pub is_admin: bool, pub groups: HashSet<String> }
pub async fn load_principal(db: &DatabaseConnection, principal: &AuthPrincipal) -> Result<GraphQlPrincipal, DbErr>;

pub struct MiryadHooks(/* PolicyRegistry */);   // impl LifecycleHooksInterface

pub fn graphql_router<S>(schema: Schema) -> axum::Router<S>
where S: Clone + Send + Sync + 'static, MiryadAuthState: FromRef<S>;
// monte POST /graphql, + GET /graphiql sous la feature "graphiql"
```

L'app construit son `BuilderContext` avec `hooks: LifecycleHooks::new(MiryadHooks::new(registry))`,
appelle `register_entity::<E>()` (Seaography) et `registry.register::<E>()` (le nôtre) pour chaque
entité — deux registres en parallèle, même geste répétitif qu'un `resource_router::<E>()` par
entité en REST.

**Point d'attention — versions** : `seaography 2.0.0-rc.9` dépend d'`async-graphql 7.0.19` en
interne. `async-graphql-axum` a une branche `8.x` sur crates.io, mais l'utiliser casserait la
compatibilité de types avec le `Schema` de Seaography — épingler `async-graphql`/
`async-graphql-axum` à `"7"`, jamais `"8"`. `seaography` est encore une release candidate ; une
2.0 finale pourrait faire bouger `LifecycleHooksInterface`/`BuilderContext`.

Pas de subscriptions — Seaography ne fournit aucun mécanisme de détection de changement
(`register_entity` ne peuple que `Query`/`Mutation`, `Subscription` reste un root vide à peupler
soi-même). Le faire correctement demanderait une détection de changement cohérente avec tous les
chemins d'écriture (REST compris), pas seulement les mutations GraphQL — hors-scope pour
l'instant.

## Serveur MCP — design validé, implémentation bloquée en amont (2026-08-22)

Tools CRUD générés par entité (`list`/`get`/`create`/`update`/`delete`), exposés en JSON-RPC 2.0
sur un unique `POST /mcp` (mêmes patterns que `kydah-mcp-template/src/mcp.rs`). Dual-auth et RBAC
entièrement réutilisés (`rest/core.rs`, cf. section REST ci-dessus) — aucune règle réécrite pour
MCP. Design validé, mais **implémentation non committée** : bloquée par un bug en amont, cf.
plus bas.

**Décision : un seul mécanisme de rendu**, pas quatre chemins de code séparés :

```rust
pub enum OutputFormat {
    Json,
    Yaml,
    Markdown,
    /// Template Handlebars fourni par l'app, remplace le défaut.
    Custom(String),
}
```

`Json`/`Yaml`/`Markdown` sont des templates Handlebars **fournis en dur par miryad-core**
(`{{json_to_str this format="json_pretty"}}`, `format="yaml"`, un gabarit `{{#each}}` générique
pour markdown) ; `Custom` est le même mécanisme de rendu, juste avec le template de l'app à la
place du défaut. Le format est fixé une fois par l'app, au montage du serveur MCP — pas
reconfigurable par appel (cohérent avec REST/GraphQL). Un enregistrement seul (`get`/`create`/
`update`) et une page de résultats (`list`, forme `PagedResult`) n'ont pas la même forme JSON :
deux templates par défaut internes selon l'opération, pas exposé comme complexité côté app.

Dispatch par nom d'entité (comme `graphql::PolicyRegistry`) plutôt que par type, puisque
`tools/call` arrive avec un nom de méthode en chaîne, pas un type Rust :

```rust
// registre : mêmes contraintes que RestEntity (feature 4), rien de nouveau à implémenter
pub struct McpToolRegistry { /* format + un trait object par entité montée */ }
impl McpToolRegistry {
    pub fn new(format: OutputFormat) -> Self;
    /// Enregistre {resource_name}_list / _get / _create / _update / _delete.
    pub fn register<E: RestEntity>(&mut self) -> &mut Self;
}

pub fn mcp_router<S>(registry: McpToolRegistry) -> axum::Router<S>
where S: Clone + Send + Sync + 'static, MiryadAuthState: FromRef<S>;
// monte POST /mcp — dispatch JSON-RPC 2.0 (initialize, tools/list, tools/call)
```

En interne, chaque tool appelle directement `rest::core::{list,get,create,update,delete}::<E>`
(mêmes fonctions que REST) puis rend le résultat via `OutputFormat`. Codes d'erreur JSON-RPC :
`-32001` (refusé), `-32002` (non trouvé), `-32601` (tool inconnu, standard "Method not found"),
`-32602` (params invalides, standard "Invalid params"), `-32603` (erreur interne/DB, standard
"Internal error") — `-32000`..`-32099` est la plage libre pour l'application selon la spec
JSON-RPC 2.0, `-32001`/`-32002` sont nos choix dans cette plage.

**Bloqué : `vynil-core` (moteur `vynil_core::hbs::HandleBars`, retenu pour réutiliser ses helpers
Handlebars json/yaml/toml déjà écrits) ne compile pas actuellement.** `handlebars_misc_helpers`
(dépendance de `vynil-core`, feature `json` activée sans condition) tire `jmespath 0.3.0`, qui
échoue à la compilation (`dyn Function` sans borne `Send`/`Sync`, utilisé dans un
`lazy_static!` qui exige `Sync`) — reproduit à l'identique sur `rustc` 1.94.0 et 1.97.1, donc pas
une histoire de toolchain trop récente : `jmespath 0.3.0` ne compile simplement plus avec du Rust
courant. Remonté en amont :
[sebt3/vynil-core#7](https://github.com/sebt3/vynil-core/issues/7) (poids des dépendances non
conditionnelles) et
[sebt3/vynil-core#8](https://github.com/sebt3/vynil-core/issues/8) (le blocage de compilation
lui-même). La feature `mcp` n'est donc **pas déclarée** dans `Cargo.toml` pour l'instant — à
reprendre une fois l'un des deux tickets résolu en amont. La feature 7 (workflow, moteur Rhai de
`vynil-core`) sera bloquée par le même problème, `vynil-core` étant une dépendance monolithique
(pas de moyen de n'obtenir que Rhai sans Handlebars/`handlebars_misc_helpers`/jmespath).

## Conventions transverses

- Identifiants d'erreur uniques, préfixés par domaine : `MRD-AUTH-XXX` (`src/auth/error.rs`),
  `MRD-REST-XXX` (`src/rest/error.rs`). Un nouveau domaine (GraphQL, MCP, workflow) suit le même
  schéma `MRD-<DOMAINE>-XXX`.
- Tables internes préfixées `miryad_` (cf. section Migrations).
- Toute fonction qui doit pouvoir être appelée depuis une migration (donc via
  `SchemaManagerConnection`, pas directement `DatabaseConnection`) est générique sur
  `C: ConnectionTrait` plutôt que de prendre `&DatabaseConnection` en dur.
