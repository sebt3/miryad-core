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

Pas de tokens API, pas de dual-auth, pas de persistance — cf. feature 2b (`docs/roadmap.md`).
