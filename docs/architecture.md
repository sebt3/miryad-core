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
`mark_all_set::<E>()` (`src/rest/mod.rs`) corrige ça par réflexion générique
(`ActiveModelTrait::get`/`set`, itération sur `E::Column`) avant tout `insert`/`update` — aucune
entité n'a besoin d'en tenir compte.

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

## Conventions transverses

- Identifiants d'erreur uniques, préfixés par domaine : `MRD-AUTH-XXX` (`src/auth/error.rs`),
  `MRD-REST-XXX` (`src/rest/error.rs`). Un nouveau domaine (GraphQL, MCP, workflow) suit le même
  schéma `MRD-<DOMAINE>-XXX`.
- Tables internes préfixées `miryad_` (cf. section Migrations).
- Toute fonction qui doit pouvoir être appelée depuis une migration (donc via
  `SchemaManagerConnection`, pas directement `DatabaseConnection`) est générique sur
  `C: ConnectionTrait` plutôt que de prendre `&DatabaseConnection` en dur.
