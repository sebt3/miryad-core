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
feature 3 (Utilisateurs & Groupes) pour l'évaluation RBAC réelle et la vraie FK `subject → User`.
