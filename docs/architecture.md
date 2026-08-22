# Architecture — miryad-core

Ce fichier accumule, feature après feature, les décisions et contrats qui structurent réellement
le code (par opposition à `AGENTS.md`, qui décrit la cible visée avant implémentation, et à
`docs/roadmap.md`, qui découpe le travail restant).

## Fondations (feature 1, 2026-08-22)

### Trait `MiryadResource`

Contrat central sur lequel REST, GraphQL, MCP et le frontend générique se greffent — une seule
implémentation par entité SeaORM, lue telle quelle par les trois couches API (pas de déclaration
de politique dupliquée entre elles).

```rust
pub trait MiryadResource: EntityTrait {
    fn resource_name() -> &'static str;
    fn read_policy() -> AccessPolicy;
    fn write_policy() -> AccessPolicy;
    fn owner_column() -> Option<<Self as EntityTrait>::Column>;
}
```

`AccessPolicy` (`Public` / `OwnerOnly` / `Group(&'static str)` / `AdminOnly`) est évaluée
séparément en lecture et en écriture — une entité peut être publique en lecture et restreinte en
écriture (cas "recettes partagées, modifiables par leur auteur uniquement"). `owner_column()`
retourne `None` pour les entités sans notion de propriétaire (référentiel partagé) ; ce contrat
n'est pas encore vérifié par le compilateur quand `OwnerOnly` est utilisé sans colonne — à
surveiller quand l'évaluation RBAC réelle arrivera (feature "Utilisateurs & Groupes").

Source : `src/resource.rs`. Non branché à quoi que ce soit encore (pas de RBAC réel, pas de
REST/GraphQL/MCP) — le trait est prouvé compilable et testable sur deux entités d'exemple
(`tests/resource.rs`), rien de plus.

### Découverte : `Column` généré par SeaORM 2.0 n'implémente pas `PartialEq`

`DeriveEntityModel` dérive `Column` avec `Copy, Clone, Debug, EnumIter, DeriveColumn` — pas
`PartialEq`/`Eq`. Comparer une valeur de `Column` (ex. dans un test, ou plus tard dans l'évaluation
RBAC pour vérifier qu'une colonne correspond à `owner_column()`) doit passer par `matches!(...)`,
pas par `==`. À garder en tête pour la feature "Utilisateurs & Groupes", qui devra comparer des
`Column` pour l'évaluation RBAC réelle.

### CI

`.github/workflows/ci.yml` (test/fmt/clippy) et `publish.yml` (publish crates.io sur tag `v*`)
repris de `vynil-core`, adaptés : pas de matrice de features (un seul jeu pour l'instant, à
réintroduire si des features Cargo apparaissent — ex. `postgres` vs `sqlite`).

## Auth — OIDC + session cookie (feature 2a, 2026-08-22)

Flow OIDC navigateur (Authorization Code), porté depuis `vanyline/app/src/auth/` et généralisé :
pas d'`AppState` concret imposé, `MiryadAuthState` (état minimal : client OIDC, clé de cookie,
redirections post-login/logout) est composé dans l'`AppState` de l'app consommatrice via le
pattern `FromRef` standard d'axum. `auth_router<S>()` et l'extracteur `AuthUser` sont génériques
sur `S` tant que `MiryadAuthState: FromRef<S>` — aucune fonction manuelle générique nécessaire
côté handlers, le mécanisme d'extraction d'axum (`State<T>: FromRequestParts<S> where T:
FromRef<S>`) suffit.

```rust
pub struct OidcIdentity {
    pub id_token: String,
    pub subject: String,       // claim `sub` — ce sur quoi la feature 3 liera un `User`
    pub email: Option<String>, // pas garanti par tous les IdP/scopes
}

pub struct MiryadAuthState {
    pub oidc_client: Arc<dyn OidcClientTrait>,
    pub cookie_key: cookie::Key,
    pub post_login_redirect: String,
    pub post_logout_redirect: String,
    pub db: DatabaseConnection,   // ajouté en feature 2b, requis pour valider un token API
}

pub fn auth_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>;

pub struct AuthUser { pub subject: String, pub email: Option<String>, pub id_token: String }
// impl<S> FromRequestParts<S> for AuthUser where MiryadAuthState: FromRef<S>
```

Le cookie de session (`miryad_session`) porte un payload JSON chiffré (`id_token`/`subject`/
`email`) plutôt que le format `id_token|email` délimité par `|` de vanyline — évite toute
dépendance à un caractère de séparation absent des claims. Le cookie transitoire CSRF/nonce
(`miryad_oidc_pending`) suit le même principe que vanyline (secret chiffré, `Max-Age=300`).

Erreurs : `AuthError` (`src/auth/error.rs`), préfixe `MRD-AUTH-XXX`, implémente `IntoResponse`
(401 pour non-authentifié/session invalide, 502 pour une erreur OIDC amont).

Pas de tokens API, pas de dual-auth, pas de persistance en 2a — cf. section suivante (2b).

## Auth — tokens API + dual-auth (feature 2b, 2026-08-22)

Première table interne à miryad-core (`miryad_api_tokens`, préfixe `miryad_` pour éviter toute
collision avec le schéma de l'app consommatrice). Le schéma est géré par un `Migrator`
(`sea-orm-migration`) embarqué dans le crate — séparé des migrations métier de l'app, appelé
explicitement par elle au démarrage (`miryad_core::migration::Migrator::up(&db, None).await`).
Toute future table interne (User/Group en feature 3) s'ajoute à ce même `Migrator`.

```rust
// src/auth/token.rs
pub type ApiToken = Entity;   // alias du Entity généré par DeriveEntityModel
pub struct Model {
    pub id: i32,
    pub subject: String,           // pas de FK vers User — n'existe pas encore (feature 3)
    pub name: String,
    pub token_hash: String,        // SHA-256 hex — jamais le token en clair
    pub created_at: DateTimeUtc,
    pub expires_at: Option<DateTimeUtc>,
    pub last_used_at: Option<DateTimeUtc>,
}

pub struct IssuedToken { pub id: i32, pub token: String }  // le token en clair, retourné une seule fois

pub async fn issue_token(db, subject, name, expires_at) -> Result<IssuedToken, AuthError>;
pub async fn validate_token(db, token) -> Result<AuthPrincipal, AuthError>;
pub async fn revoke_token(db, id) -> Result<(), AuthError>;
```

Format du token : `mrd_<43 car. base64url>` (32 octets aléatoires, préfixe façon GitHub/Stripe).
Haché en SHA-256 (pas un KDF lent : le secret est déjà haute-entropie, pas un mot de passe humain).

`AuthPrincipal` (`src/auth/principal.rs`) est le type unifié que REST/GraphQL/MCP consommeront
(feature 4+) — distinct d'`AuthUser` (2a, resté spécifique au cookie navigateur) :

```rust
pub struct AuthPrincipal { pub subject: String, pub email: Option<String>, pub source: PrincipalSource }
pub enum PrincipalSource { Session { id_token: String }, ApiToken { token_id: i32 } }
```

L'extracteur dual-auth (`src/auth/dual.rs`, `impl FromRequestParts<S> for AuthPrincipal`) résout
soit via `Authorization: Bearer <token>`, soit via le cookie de session — dans cet ordre. Un
header `Authorization: Bearer` présent mais invalide ne retombe **pas** sur le cookie : c'est un
choix explicite du client, son échec est final (pas de repli silencieux vers un mode plus faible).

`MiryadAuthState` (2a) s'est étendu d'un champ `db: DatabaseConnection`, sans rupture — 2a n'était
pas encore publié.

Pas de RBAC réel, pas de `MiryadResource` sur `ApiToken` (pas de CRUD générique dessus) — cf.
feature 3 (Utilisateurs & Groupes) pour l'évaluation RBAC réelle.

## Utilisateurs & Groupes (feature 3, 2026-08-22)

**Décision structurante : synchronisation, pas gestion.** Authentik est la seule source de vérité
pour l'appartenance aux groupes (`admin` compris) — miryad-core ne fournit **aucune API
d'assignation manuelle**. À chaque login OIDC réussi, `handler_callback` (`src/auth/mod.rs`)
enchaîne `resolve_user` (get-or-create par `subject`, sans FK depuis `ApiToken` — la résolution se
fait par requête sur `subject`, pas par contrainte de schéma) puis `sync_groups_from_oidc`, qui
réconcilie **entièrement** les `GroupMembership` locales depuis le claim `groups` du token : ajout
des groupes présents (création à la volée d'un `Group` jamais vu — pas de registre préalable),
retrait de ceux qui ne le sont plus. Le groupe `admin` est *seedé* par migration (toujours présent,
vide au départ), sans rien de spécial au niveau schéma — juste une convention lue par
`rbac::is_admin`.

**Limite acceptée** : un principal résolu depuis un token API (2b) n'a pas de session OIDC vivante
par requête — son état de groupe reflète le *dernier login navigateur* de son `subject`, pas l'état
Authentik en temps réel. Une révocation de groupe ne prend effet sur les tokens existants qu'après
la prochaine connexion navigateur de la personne concernée.

Le claim `groups` (spécifique à Authentik, pas standard OIDC) est extrait à la main du payload de
l'`id_token` déjà vérifié (`auth::oidc::extract_groups_claim`, même technique que
`cookie::extract_exp_claim`), plutôt que de reconfigurer `CoreClient` avec des `AdditionalClaims`
génériques pour un seul champ. Ça a changé la signature d'`OidcClientTrait::exchange_code` (2a,
pas encore publié — pas de rupture) : elle retourne désormais `OidcLoginResult { identity,
groups }` au lieu de `OidcIdentity` seule.

```rust
// src/users/{user,group,membership}.rs
pub struct User { pub id: i32, pub subject: String, pub email: Option<String>, pub display_name: Option<String>, pub created_at: DateTimeUtc }
pub struct Group { pub id: i32, pub name: String, pub created_at: DateTimeUtc }
pub struct GroupMembership { pub id: i32, pub user_id: i32, pub group_id: i32 }  // unique(user_id, group_id)

pub async fn resolve_user<C: ConnectionTrait>(db: &C, subject: &str, email: Option<&str>) -> Result<user::Model, DbErr>;
pub async fn sync_groups_from_oidc<C: ConnectionTrait>(db: &C, user_id: i32, groups: &[String]) -> Result<(), DbErr>;
pub async fn is_admin<C: ConnectionTrait>(db: &C, user_id: i32) -> Result<bool, DbErr>;
pub async fn is_member<C: ConnectionTrait>(db: &C, user_id: i32, group_name: &str) -> Result<bool, DbErr>;
```

Ces fonctions sont génériques sur `C: ConnectionTrait` (pas `&DatabaseConnection` en dur) : le seed
de migration (`m20260822_000003_seed_admin_group`) les appelle directement via
`manager.get_connection()`, qui n'est pas un `DatabaseConnection` mais satisfait le même trait —
évite de dupliquer la logique d'insertion en SQL brut dans la migration.

```rust
// src/rbac.rs
pub async fn can_read<E>(db, user: &user::Model, record: &E::Model) -> Result<bool, DbErr>
where E: MiryadResource, E::Model: ModelTrait<Entity = E>;
pub async fn can_write<E>(...) -> Result<bool, DbErr>;  // même signature, write_policy()
```

`OwnerOnly` compare via `record.get(owner_column)` — réflexion générique `sea_orm::ModelTrait`,
aucune entité n'écrit son propre code de comparaison (l'intention du trait `MiryadResource` de la
feature 1 tenue jusqu'au bout). Admin gagne toujours, sur toutes les politiques sauf `Public`
(inutile). `OwnerOnly` sans `owner_column` → refuse (fail-closed, comportement non défini par le
contrat de la feature 1, désormais couvert par un test explicite).

Pas de filtrage de liste en feature 3 (`can_read`/`can_write` évaluent un enregistrement déjà
chargé, un à la fois) — cf. section suivante (feature 4) pour `list_access`, qui construit la
clause de requête.

## API REST générique (feature 4, 2026-08-22)

Routeur CRUD monté automatiquement pour toute entité qui implémente `MiryadResource` —
`resource_router::<E, S>()` génère `GET/POST /{resource_name}` et
`GET/PUT/DELETE /{resource_name}/{id}`, réutilisant `MiryadAuthState` (2b) comme état, aucun
nouvel état à composer côté app.

**Le corps de requête/réponse réutilise `E::Model`** — pas de DTO Create/Update par entité,
grâce à `IntoActiveModel<E::ActiveModel>` généré par `DeriveEntityModel`. **Contrainte assumée** :
une seule colonne de clé primaire, de type `i32` (vrai pour toutes les entités du crate à ce
jour). Le trait `RestEntity` (`src/rest/mod.rs`) encode ces contraintes comme des bornes sur
`MiryadResource` (via les bornes associées `Model:`/`ActiveModel:`/`PrimaryKey:` de Rust 2024),
avec un blanket impl — pas de méthode supplémentaire à implémenter par entité.

```rust
pub trait RestEntity:
    MiryadResource<
        Model: Serialize + DeserializeOwned + IntoActiveModel<Self::ActiveModel> + Sync,
        ActiveModel: ActiveModelTrait<Entity = Self> + Send,
        PrimaryKey: PrimaryKeyTrait<ValueType = i32> + PrimaryKeyToColumn<Column = Self::Column>,
    >
{
}

pub fn resource_router<E: RestEntity, S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>;
```

### Découverte : `Model::into_active_model()` marque tout `Unchanged`, pas `Set`

`DeriveEntityModel` génère `impl From<Model> for ActiveModel` en mettant **tous** les champs à
`ActiveValue::Unchanged`, y compris hors primary key. `ActiveModelTrait::insert()` traite
`Unchanged` comme une valeur à écrire (donc `create` marchait tel quel), mais
`ActiveModelTrait::update()` n'inclut que les champs `Set` dans la clause `SET` — un premier essai
naïf de l'endpoint `PUT` n'écrivait donc silencieusement aucune colonne. Corrigé par
`mark_all_set::<E>()` (`src/rest/mod.rs`), qui repasse tous les champs de `Unchanged` à `Set` par
réflexion générique (`ActiveModelTrait::get`/`set`, itération sur `E::Column`) avant `insert`/
`update` — aucune entité n'a besoin d'en tenir compte.

### Pagination et filtre de liste

`src/query.rs` (partagé, pas spécifique REST) : `Pagination` (page 1-indexée, `per_page` par
défaut 100, plafonné à 1000 — pas une pagination fine, juste une garde-fou contre une liste de
milliers de lignes) et `PagedResult<M> { items, page, per_page, total_items, total_pages }` via le
`Paginator` déjà intégré à SeaORM (`Select::paginate` + `num_items_and_pages`).

`MiryadResource::filter_column() -> Option<Self::Column>` (défaut `None`, amendement feature 1)
déclare une colonne texte filtrable par `?filter=valeur` (égalité exacte). Combiné en `AND` avec
la condition RBAC de `rbac::list_access::<E>()` (`Unrestricted` / `FilterByOwner(Condition)` /
`Forbidden`) — un non-admin filtrant par catégorie ne voit jamais les enregistrements d'autrui,
même si la catégorie correspond.

### RBAC de création

`rbac::can_create::<E>()` (nouveau, pas de record à comparer) : `Public`/`OwnerOnly` autorisent
toujours (le créateur devient propriétaire), `Group`/`AdminOnly` vérifient l'appartenance. À la
création, la colonne `owner_column()` du corps client est ignorée et écrasée par l'id de
l'utilisateur authentifié — jamais de création au nom de quelqu'un d'autre.

Pas de pagination par curseur, pas de tri, pas de filtre multi-champs, pas de masquage de champ —
hors-scope MVP. 403 (pas 404) pour un enregistrement existant mais non autorisé.
