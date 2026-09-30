//! Endpoint self-service pour gérer ses propres tokens API (issue #5) — page "mon compte", pas
//! admin : chaque utilisateur ne voit et ne révoque que ses propres tokens. `issue_token`/
//! `revoke_token`/`validate_token` (`auth::token`) existaient déjà comme fonctions Rust mais
//! n'étaient montées derrière aucune route HTTP.

use axum::extract::{FromRef, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use sea_orm::entity::prelude::*;
use sea_orm::{ColumnTrait, PaginatorTrait, QueryFilter};
use serde::{Deserialize, Serialize};

use crate::auth::token::{ApiToken, Column};
use crate::auth::{AuthError, AuthPrincipal, MiryadAuthState, issue_token, revoke_token};
use crate::query::{PagedResult, Pagination};
use crate::rest::error::RestError;

/// `issue_token`/`revoke_token` ne produisent en pratique que `AuthError::Database` — les autres
/// variantes appartiennent au flow OIDC/session, jamais atteintes ici. Conversion explicite
/// plutôt qu'un `From<AuthError>` générique qui laisserait croire à une correspondance 1:1.
fn to_rest_error(err: AuthError) -> RestError {
    match err {
        AuthError::Database(db_err) => RestError::Database(db_err),
        other => RestError::Internal(other.to_string()),
    }
}

/// Jamais la valeur en clair — seul `token::IssuedToken` (retourné une fois, à l'émission) la
/// porte.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TokenSummary {
    /// Clé primaire de la ligne `miryad_api_tokens`.
    pub id: i32,
    /// Nom donné au token à l'émission, stocké verbatim par le moteur.
    pub name: String,
    /// Date de création, sérialisée en RFC 3339 (suffixe `Z`).
    pub created_at: DateTimeUtc,
    /// Date d'expiration — sérialisée `null` quand le token n'a pas d'expiration.
    pub expires_at: Option<DateTimeUtc>,
    /// Date de dernière authentification par ce token — sérialisée `null` quand le
    /// token n'a jamais servi.
    pub last_used_at: Option<DateTimeUtc>,
}

impl From<crate::auth::token::Model> for TokenSummary {
    fn from(model: crate::auth::token::Model) -> Self {
        Self {
            id: model.id,
            name: model.name,
            created_at: model.created_at,
            expires_at: model.expires_at,
            last_used_at: model.last_used_at,
        }
    }
}

#[derive(Deserialize)]
struct ListParams {
    page: Option<u64>,
    per_page: Option<u64>,
}

#[derive(Deserialize)]
struct CreateTokenBody {
    name: String,
    expires_at: Option<DateTimeUtc>,
}

#[derive(Serialize)]
struct CreatedToken {
    id: i32,
    token: String,
}

/// Monte `GET/POST /api/v1/tokens` et `DELETE /api/v1/tokens/{id}` — n'importe quel principal
/// authentifié (dual-auth), toujours restreint au `subject` courant, jamais les tokens d'un autre
/// utilisateur. Réutilise `MiryadAuthState` comme les autres routeurs. Préfixe `/api/v1` figé,
/// cohérent avec `resource_router` (feature 6).
pub fn tokens_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>,
{
    Router::new().nest(
        "/api/v1",
        Router::new()
            .route("/tokens", get(list_tokens_handler).post(create_token_handler))
            .route("/tokens/{id}", axum::routing::delete(delete_token_handler)),
    )
}

async fn list_tokens_handler(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Query(params): Query<ListParams>,
) -> Result<Json<PagedResult<TokenSummary>>, RestError> {
    let pagination = Pagination::from_raw(params.page, params.per_page);
    let paginator = ApiToken::find()
        .filter(Column::Subject.eq(&principal.subject))
        // `GET` ordonné par `id` ascendant (tokens.sdd `Handles`, arbitré 2026-09-29) :
        // pagination déterministe, reflet de l'index posé par m20260930_000001.
        .order_by_id_asc()
        .paginate(&auth.db, pagination.per_page);
    let totals = paginator.num_items_and_pages().await?;
    // `query.sdd` borne `page >= 1` : `saturating_sub(1)` ne sature jamais, l'index rendu est
    // exactement `page - 1` (purge `arithmetic_side_effects` de `tooling.sdd`).
    let items = paginator
        .fetch_page(pagination.page.saturating_sub(1))
        .await?
        .into_iter()
        .map(TokenSummary::from)
        .collect();

    Ok(Json(PagedResult {
        items,
        page: pagination.page,
        per_page: pagination.per_page,
        total_items: totals.number_of_items,
        total_pages: totals.number_of_pages,
    }))
}

async fn create_token_handler(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Json(body): Json<CreateTokenBody>,
) -> Result<(StatusCode, Json<CreatedToken>), RestError> {
    // Rejet `422` de l'expiration déjà passée (tokens.sdd `Accepts`, arbitré 2026-09-29) :
    // premier émetteur de `RestError::InvalidInput` (`MRD-REST-005`) de la crate. Le moteur
    // Rust reste en pass-through (auth/token.sdd) — la garde est propre à cette surface.
    if body.expires_at.is_some_and(|expires_at| expires_at <= Utc::now()) {
        return Err(RestError::InvalidInput(
            "expires_at must be in the future".to_string(),
        ));
    }
    let issued = issue_token(
        &auth.db,
        &principal.subject,
        &body.name,
        body.expires_at,
        &auth.token_pepper,
    )
    .await
    .map_err(to_rest_error)?;
    // `201` Created, corps exactement `id` et `token`, sans en-tête `Location`
    // (tokens.sdd `Returns`, arbitré 2026-09-29).
    Ok((
        StatusCode::CREATED,
        Json(CreatedToken {
            id: issued.id,
            token: issued.token,
        }),
    ))
}

async fn delete_token_handler(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Path(id): Path<i32>,
) -> Result<StatusCode, RestError> {
    // DELETE filtré par propriétaire (tokens.sdd `Handles`, arbitré 2026-09-29) : une seule
    // requête `find_by_id` restreinte à `subject` — token d'autrui et token inexistant rendent
    // le même `404`, plus d'oracle d'existence, plus de `403` sur cette route.
    let owned = ApiToken::find_by_id(id)
        .filter(Column::Subject.eq(&principal.subject))
        .one(&auth.db)
        .await?
        .is_some();
    if !owned {
        return Err(RestError::NotFound);
    }
    revoke_token(&auth.db, id).await.map_err(to_rest_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::cookie::build_set_cookie;
    use crate::auth::oidc::{MockOidcClient, OidcIdentity};
    use crate::migration::Migrator;
    use axum::body::Body;
    use axum::http::Request;
    use base64::Engine as _;
    use chrono::Utc;
    use sea_orm::{ConnectionTrait, Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;
    use tower::ServiceExt;

    /// Motif partagé avec le module de tests du module : `sqlite::memory:` + `Migrator::up`,
    /// sans base externe (/tooling.sdd `Depends on`).
    async fn test_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    fn test_state(db: DatabaseConnection) -> MiryadAuthState {
        MiryadAuthState {
            oidc_client: std::sync::Arc::new(MockOidcClient::default()),
            cookie_key: ::cookie::Key::from(&[0u8; 64]),
            post_login_redirect: "/".to_string(),
            post_logout_redirect: "/".to_string(),
            db,
            secure_cookies: false,
            token_pepper: "test-pepper".to_string(),
        }
    }

    fn app(state: MiryadAuthState) -> Router {
        Router::new()
            .merge(tokens_router::<MiryadAuthState>())
            .with_state(state)
    }

    async fn json_body(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        serde_json::from_slice(&bytes).expect("valid JSON body")
    }

    /// Corps texte brut (rejets `MRD-*`, erreurs axum) — les rejets sont affirmés par chaîne
    /// exacte, jamais par le seul statut.
    async fn body_text(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn request(method: &str, uri: &str, token: &str, body: Option<serde_json::Value>) -> Request<Body> {
        let builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json");
        match body {
            Some(value) => builder
                .body(Body::from(value.to_string()))
                .expect("valid request"),
            None => builder.body(Body::empty()).expect("valid request"),
        }
    }

    /// Requête à en-têtes et corps entièrement contrôlés : aucune credential (401, 405, 404 de
    /// routage), cookie de session, `Content-Type` absent (415) ou corps brut invalide (400).
    fn raw_request(method: &str, uri: &str, headers: &[(&str, &str)], body: &[u8]) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::from(body.to_vec())).expect("valid request")
    }

    /// Lignes `miryad_api_tokens` courantes — sonde « aucun token créé » des rejets.
    async fn token_count(db: &DatabaseConnection) -> usize {
        ApiToken::find().all(db).await.expect("query succeeds").len()
    }

    /// Clés d'un objet JSON triées — assertions « exactement ces clés » (`Accepts`/`Returns`).
    fn sorted_keys(value: &serde_json::Value) -> Vec<String> {
        let mut keys: Vec<String> = value
            .as_object()
            .expect("JSON object body")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    // ——— Fixture cookie de session (patron src/auth/dual.rs, même clé [0u8; 64]) ———

    /// JWT tri-segment dont la claim `exp` vaut `exp` (base64 url-safe sans bourrage).
    fn make_jwt(exp: u64) -> String {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("header.{payload}.sig")
    }

    fn future_exp() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_secs()
            + 3600
    }

    /// Paire `nom=valeur` isolée d'un en-tête `Set-Cookie` (premier segment avant `;`).
    fn cookie_pair(set_cookie: &str) -> String {
        set_cookie
            .split(';')
            .next()
            .expect("cookie pair present")
            .to_string()
    }

    /// Cookie de session valide signé de la clé de l'état, pour le `subject` demandé.
    fn session_cookie(state: &MiryadAuthState, subject: &str) -> String {
        let identity = OidcIdentity {
            id_token: make_jwt(future_exp()),
            subject: subject.to_string(),
            email: Some(format!("{subject}@example.com")),
            preferred_username: Some(subject.to_string()),
        };
        cookie_pair(&build_set_cookie(
            &identity,
            &state.cookie_key,
            state.secure_cookies,
        ))
    }

    // ——— AuthN sur les trois routes ———

    /// `Scenario` : « les trois routes sans identifiant rendent `401` ».
    #[tokio::test]
    async fn unauthenticated_requests_are_rejected_on_the_three_routes() {
        let db = test_db().await;
        let db_check = db.clone();
        let app = app(test_state(db));

        let get = app
            .clone()
            .oneshot(raw_request("GET", "/api/v1/tokens", &[], &[]))
            .await
            .expect("router does not fail");
        assert_eq!(get.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            body_text(get).await,
            "MRD-AUTH-001: not authenticated (no session cookie)"
        );

        let post = app
            .clone()
            .oneshot(raw_request(
                "POST",
                "/api/v1/tokens",
                &[("Content-Type", "application/json")],
                br#"{"name":"x"}"#,
            ))
            .await
            .expect("router does not fail");
        assert_eq!(post.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            body_text(post).await,
            "MRD-AUTH-001: not authenticated (no session cookie)"
        );

        let delete = app
            .oneshot(raw_request("DELETE", "/api/v1/tokens/1", &[], &[]))
            .await
            .expect("router does not fail");
        assert_eq!(delete.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            body_text(delete).await,
            "MRD-AUTH-001: not authenticated (no session cookie)"
        );

        assert_eq!(
            token_count(&db_check).await,
            0,
            "aucun token n'a été créé en base"
        );
    }

    /// `Scenario` : « Bearer invalide ou expiré rend `401` sans repli sur le cookie de session ».
    #[tokio::test]
    async fn invalid_or_expired_bearer_is_rejected_without_cookie_fallback() {
        let db = test_db().await;
        // Le moteur Rust reste en pass-through (../auth/token.sdd) : l'émission d'un
        // expires_at passé se fait ici par le moteur, hors surface REST (gardée 422).
        let past = DateTimeUtc::from_timestamp(Utc::now().timestamp() - 60, 0).expect("valid timestamp");
        let expired = issue_token(&db, "bob", "doomed", Some(past), "test-pepper")
            .await
            .expect("the Rust engine still accepts a past expiry")
            .token;
        let state = test_state(db);
        let cookie = session_cookie(&state, "bob");
        let app = app(state);

        // Bearer inconnu + cookie valide du même subject : 014, le cookie n'est jamais réessayé.
        let resp = app
            .clone()
            .oneshot(raw_request(
                "GET",
                "/api/v1/tokens",
                &[
                    ("Authorization", "Bearer mrd_nexiste-pas"),
                    ("Cookie", cookie.as_str()),
                ],
                &[],
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            body_text(resp).await,
            "MRD-AUTH-014: invalid or unknown API token"
        );

        // Bearer expiré à la place, même cookie valide : 015, sans repli non plus.
        let bearer = format!("Bearer {expired}");
        let resp = app
            .oneshot(raw_request(
                "GET",
                "/api/v1/tokens",
                &[("Authorization", bearer.as_str()), ("Cookie", cookie.as_str())],
                &[],
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body_text(resp).await, "MRD-AUTH-015: expired API token");
    }

    // ——— Cookie de session : liste puis émission pour soi ———

    /// `Scenario` : « cookie de session liste vide puis émission pour soi ».
    #[tokio::test]
    async fn session_cookie_lists_empty_then_self_issues() {
        let db = test_db().await;
        issue_token(&db, "bob", "bob-visible", None, "test-pepper")
            .await
            .expect("issuing succeeds");
        let state = test_state(db);
        let cookie = session_cookie(&state, "session-user");
        let app = app(state);

        let listed = app
            .clone()
            .oneshot(raw_request(
                "GET",
                "/api/v1/tokens",
                &[("Cookie", cookie.as_str())],
                &[],
            ))
            .await
            .expect("router does not fail");
        assert_eq!(listed.status(), StatusCode::OK);
        let body = json_body(listed).await;
        assert_eq!(
            body["total_items"], 0,
            "le principal de session n'a encore aucun token"
        );
        assert!(body["items"].as_array().expect("items array").is_empty());

        let created = app
            .clone()
            .oneshot(raw_request(
                "POST",
                "/api/v1/tokens",
                &[("Cookie", cookie.as_str()), ("Content-Type", "application/json")],
                br#"{"name":"mon CLI"}"#,
            ))
            .await
            .expect("router does not fail");
        assert_eq!(created.status(), StatusCode::CREATED);
        let issued = json_body(created).await;
        let secret = issued["token"].as_str().expect("token present").to_string();
        assert!(secret.starts_with("mrd_"));

        let listed = app
            .oneshot(request("GET", "/api/v1/tokens", &secret, None))
            .await
            .expect("router does not fail");
        let body = json_body(listed).await;
        assert_eq!(body["total_items"], 1);
        assert_eq!(
            body["items"][0]["name"], "mon CLI",
            "le token de bob reste invisible"
        );
    }

    // ——— Isolation stricte par subject ———

    /// `Scenario` : « liste strictement au subject du principal, sans passe-droit admin ».
    /// Écart Consigné (voir rapport) : le `Then` « une page vide est rendue » du `Scenario` est
    /// faux pour un appelant Bearer — sa propre ligne de token est listée (contrat `Returns` du
    /// `GET`, prouvé par « projection a exactement cinq champs » et le test sœur). Le verrou
    /// tenu ici est le `But` : l'admin ne voit que sa propre ligne, jamais le bien d'autrui.
    #[tokio::test]
    async fn list_only_returns_the_caller_own_tokens() {
        let db = test_db().await;
        let alice_token = issue_token(&db, "alice", "alice's token", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        issue_token(&db, "bob", "bob's token", None, "test-pepper")
            .await
            .expect("issuing succeeds");
        // Appartenance admin posée en SQL brut (le fichier n'atteint pas @crate::users —
        // `Forbids`) : utilisateur `admin` membre du groupe `admin` seedé par la migration 003.
        db.execute_unprepared(
            "INSERT INTO miryad_users (subject, created_at) \
             VALUES ('admin', '2026-09-30T00:00:00Z')",
        )
        .await
        .expect("admin user row inserts");
        db.execute_unprepared(
            "INSERT INTO miryad_group_memberships (user_id, group_id) \
             SELECT u.id, g.id FROM miryad_users u \
             JOIN miryad_groups g ON g.name = 'admin' WHERE u.subject = 'admin'",
        )
        .await
        .expect("admin membership on the seeded admin group inserts");
        let admin_token = issue_token(&db, "admin", "admin's token", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        let resp = app
            .clone()
            .oneshot(request("GET", "/api/v1/tokens", &alice_token, None))
            .await
            .expect("router does not fail");
        let body = json_body(resp).await;
        assert_eq!(body["total_items"], 1);
        assert_eq!(body["items"][0]["name"], "alice's token");

        // Le Bearer d'admin : aucune voie d'à-côté, l'admin ne voit que sa propre ligne.
        let resp = app
            .oneshot(request("GET", "/api/v1/tokens", &admin_token, None))
            .await
            .expect("router does not fail");
        let body = json_body(resp).await;
        let names: Vec<&str> = body["items"]
            .as_array()
            .expect("items array")
            .iter()
            .map(|item| item["name"].as_str().expect("item name"))
            .collect();
        assert_eq!(
            names,
            vec!["admin's token"],
            "l'admin ne liste que son propre token — ni alice ni bob ne traversent le filtre"
        );
    }

    // ——— Projection fermée à cinq champs ———

    /// `Scenario` : « projection a exactement cinq champs, jamais secret, hash ni subject » —
    /// et `POST` réussi en `201` (`Returns`, arbitré 2026-09-29, sans en-tête `Location`).
    #[tokio::test]
    async fn create_then_list_returns_the_token_without_the_cleartext_value() {
        let db = test_db().await;
        let bootstrap = issue_token(&db, "alice", "bootstrap", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        let create_body = serde_json::json!({ "name": "cli laptop" });
        let created = app
            .clone()
            .oneshot(request("POST", "/api/v1/tokens", &bootstrap, Some(create_body)))
            .await
            .expect("router does not fail");
        assert_eq!(created.status(), StatusCode::CREATED);
        assert!(
            created.headers().get("location").is_none(),
            "pas d'en-tête Location sur le 201 (Returns, arbitré 2026-09-29)"
        );
        let issued_json = json_body(created).await;
        let cleartext = issued_json["token"].as_str().expect("token present").to_string();
        assert!(cleartext.starts_with("mrd_"));

        let listed = app
            .oneshot(request("GET", "/api/v1/tokens", &bootstrap, None))
            .await
            .expect("router does not fail");
        assert_eq!(listed.status(), StatusCode::OK);
        let raw = body_text(listed).await;
        let listed_body: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON body");

        // Enveloppe @PagedResult : exactement les cinq clés du wire (../query.sdd).
        assert_eq!(
            sorted_keys(&listed_body),
            vec![
                "items".to_string(),
                "page".to_string(),
                "per_page".to_string(),
                "total_items".to_string(),
                "total_pages".to_string(),
            ]
        );
        assert_eq!(listed_body["total_items"], 2);
        let items = listed_body["items"].as_array().expect("items array");
        for item in items {
            assert_eq!(
                sorted_keys(item),
                vec![
                    "created_at".to_string(),
                    "expires_at".to_string(),
                    "id".to_string(),
                    "last_used_at".to_string(),
                    "name".to_string(),
                ],
                "l'item a exactement les cinq clés de @TokenSummary"
            );
            assert_eq!(
                item["expires_at"],
                serde_json::Value::Null,
                "sans expiration = null"
            );
            assert!(
                item["created_at"]
                    .as_str()
                    .expect("created_at rendered")
                    .ends_with('Z'),
                "date RFC 3339 à suffixe Z"
            );
        }

        let new_entry = items
            .iter()
            .find(|t| t["name"] == "cli laptop")
            .expect("new token present in the list");
        assert!(
            new_entry.get("token").is_none(),
            "cleartext value must never be listed"
        );
        assert!(
            new_entry["last_used_at"].is_null(),
            "le token jamais authentifié porte last_used_at null"
        );
        assert_eq!(new_entry["id"], issued_json["id"]);

        // Nulle part : le secret en clair, une sous-chaîne de token_hash ou le mot subject.
        assert!(!raw.contains(&cleartext), "le secret en clair ne revient jamais");
        assert!(!raw.contains("token_hash"), "l'empreinte n'est jamais projetée");
        assert!(!raw.contains("subject"), "le subject n'est jamais sérialisé");
    }

    // ——— last_used_at de la requête qui authentifie ———

    /// `Scenario` : « le `last_used_at` du token porte la trace de la requete qui vient de
    /// l'authentifier ».
    #[tokio::test]
    async fn caller_last_used_at_is_visible_on_the_request_that_authenticates_it() {
        let db = test_db().await;
        let issued = issue_token(&db, "alice", "fresh", None, "test-pepper")
            .await
            .expect("issuing succeeds");
        let row = ApiToken::find_by_id(issued.id)
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("issued row exists");
        assert_eq!(row.last_used_at, None, "aucun usage avant la requête");
        let app = app(test_state(db));

        let resp = app
            .oneshot(request("GET", "/api/v1/tokens", &issued.token, None))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["total_items"], 1);
        assert_eq!(body["items"][0]["id"], issued.id);
        assert_ne!(
            body["items"][0]["last_used_at"],
            serde_json::Value::Null,
            "@validate_token de l'extracteur a écrit l'horodatage avant la lecture de la page"
        );
    }

    // ——— Pagination déterministe ———

    /// `Scenario` : « page demandee est echoee et tranchee » — offset amont
    /// `per_page * (page - 1)`, et `Pagination::from_raw` (`page=0` → `1`, `per_page` absent →
    /// `100`).
    #[tokio::test]
    async fn requested_page_is_echoed_and_sliced() {
        let db = test_db().await;
        for n in 0..4 {
            issue_token(&db, "alice", &format!("moteur {n}"), None, "test-pepper")
                .await
                .expect("issuing succeeds");
        }
        let state = test_state(db);
        let cookie = session_cookie(&state, "alice");
        let app = app(state);

        // Le cinquième token vient de l'émetteur de session lui-même (cookie).
        let created = app
            .clone()
            .oneshot(raw_request(
                "POST",
                "/api/v1/tokens",
                &[("Cookie", cookie.as_str()), ("Content-Type", "application/json")],
                br#"{"name":"session emetteur"}"#,
            ))
            .await
            .expect("router does not fail");
        assert_eq!(created.status(), StatusCode::CREATED);

        let listed = app
            .clone()
            .oneshot(raw_request(
                "GET",
                "/api/v1/tokens?page=2&per_page=2",
                &[("Cookie", cookie.as_str())],
                &[],
            ))
            .await
            .expect("router does not fail");
        assert_eq!(listed.status(), StatusCode::OK);
        let body = json_body(listed).await;
        assert_eq!(body["page"], 2);
        assert_eq!(body["per_page"], 2);
        assert_eq!(body["total_items"], 5);
        assert_eq!(body["total_pages"], 3);
        assert_eq!(
            body["items"].as_array().expect("items array").len(),
            2,
            "tranche exactement per_page = 2 à l'offset per_page * (page - 1)"
        );

        let zero = json_body(
            app.clone()
                .oneshot(raw_request(
                    "GET",
                    "/api/v1/tokens?page=0",
                    &[("Cookie", cookie.as_str())],
                    &[],
                ))
                .await
                .expect("router does not fail"),
        )
        .await;
        assert_eq!(zero["page"], 1, "page 0 ramenée à 1 (@Pagination::from_raw)");

        let echo = json_body(
            app.oneshot(raw_request(
                "GET",
                "/api/v1/tokens",
                &[("Cookie", cookie.as_str())],
                &[],
            ))
            .await
            .expect("router does not fail"),
        )
        .await;
        assert_eq!(echo["per_page"], 100, "per_page absent rendu 100");
    }

    /// `Scenario` : « page hors limite rend items vides et totaux preservés ».
    #[tokio::test]
    async fn out_of_range_page_keeps_empty_items_and_totals() {
        let db = test_db().await;
        let alice = issue_token(&db, "alice", "unique", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        let resp = app
            .oneshot(request("GET", "/api/v1/tokens?page=99&per_page=10", &alice, None))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(
            body["page"], 99,
            "@page est echoé tel quel, pas ramené à la dernière page"
        );
        assert_eq!(body["per_page"], 10);
        assert!(body["items"].as_array().expect("items array").is_empty());
        assert_eq!(body["total_items"], 1);
        assert_eq!(body["total_pages"], 1);
    }

    /// `Scenario` : « page non numerique rejete avant toute lecture » — rejet `Query` d'axum,
    /// la base reste inchangée (aucune voie d'écriture sur ce chemin).
    #[tokio::test]
    async fn non_numeric_page_query_is_rejected_with_bad_request() {
        let db = test_db().await;
        let db_check = db.clone();
        let alice = issue_token(&db, "alice", "alice", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        let resp = app
            .oneshot(request("GET", "/api/v1/tokens?page=abc", &alice, None))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(token_count(&db_check).await, 1, "la base reste inchangée");
    }

    /// `Scenario` verrou de `Handles` (arbitré 2026-09-29) : « Le `GET` est ordonné par `id`
    /// ascendant » — pagination déterministe, y compris de page en page.
    #[tokio::test]
    async fn list_items_are_ordered_by_id_ascending() {
        let db = test_db().await;
        let bootstrap = issue_token(&db, "alice", "bootstrap", None, "test-pepper")
            .await
            .expect("issuing succeeds");
        let mut expected = vec![i64::from(bootstrap.id)];
        for n in 0..4 {
            let issued = issue_token(&db, "alice", &format!("tri {n}"), None, "test-pepper")
                .await
                .expect("issuing succeeds");
            expected.push(i64::from(issued.id));
        }
        expected.sort_unstable();
        assert!(
            expected.windows(2).all(|w| w[0] < w[1]),
            "les ids attendus sont strictement croissants"
        );
        let app = app(test_state(db));

        let full = json_body(
            app.clone()
                .oneshot(request(
                    "GET",
                    "/api/v1/tokens?per_page=10",
                    &bootstrap.token,
                    None,
                ))
                .await
                .expect("router does not fail"),
        )
        .await;
        let listed: Vec<i64> = full["items"]
            .as_array()
            .expect("items array")
            .iter()
            .map(|item| item["id"].as_i64().expect("item id"))
            .collect();
        assert_eq!(listed, expected, "items rendus en ordre id ascendant");

        // Tranche de page en page : page 1 puis page 2 recollent l'ordre complet.
        let first = json_body(
            app.clone()
                .oneshot(request(
                    "GET",
                    "/api/v1/tokens?page=1&per_page=2",
                    &bootstrap.token,
                    None,
                ))
                .await
                .expect("router does not fail"),
        )
        .await;
        let second = json_body(
            app.oneshot(request(
                "GET",
                "/api/v1/tokens?page=2&per_page=2",
                &bootstrap.token,
                None,
            ))
            .await
            .expect("router does not fail"),
        )
        .await;
        let mut stitched: Vec<i64> = first["items"]
            .as_array()
            .expect("page 1 items")
            .iter()
            .map(|item| item["id"].as_i64().expect("item id"))
            .collect();
        stitched.extend(
            second["items"]
                .as_array()
                .expect("page 2 items")
                .iter()
                .map(|item| item["id"].as_i64().expect("item id")),
        );
        assert_eq!(
            &stitched[..4],
            &expected[..4],
            "la fenêtre LIMIT/OFFSET suit le même tri"
        );
    }

    // ——— POST : émission ———

    /// `Scenario` : « emission rend `201` avec seulement l'`id` et le secret `mrd_` ».
    #[tokio::test]
    async fn issued_response_is_created_with_only_id_and_secret() {
        let db = test_db().await;
        let alice = issue_token(&db, "alice", "bootstrap", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        let created = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/v1/tokens",
                &alice,
                Some(serde_json::json!({"name":"cli"})),
            ))
            .await
            .expect("router does not fail");
        assert_eq!(created.status(), StatusCode::CREATED);
        assert!(
            created.headers().get("location").is_none(),
            "pas d'en-tête Location (Returns, arbitré 2026-09-29)"
        );
        let issued = json_body(created).await;
        assert_eq!(
            sorted_keys(&issued),
            vec!["id".to_string(), "token".to_string()],
            "exactement deux clés : id et token"
        );
        let secret = issued["token"].as_str().expect("token present");
        assert!(secret.starts_with("mrd_"));
        let body_part = secret.strip_prefix("mrd_").expect("mrd_ prefix present");
        assert_eq!(body_part.len(), 43, "43 caractères après le préfixe");
        assert!(
            body_part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "alphabet base64url uniquement : {secret}"
        );

        let listed = json_body(
            app.clone()
                .oneshot(request("GET", "/api/v1/tokens", &alice, None))
                .await
                .expect("router does not fail"),
        )
        .await;
        let item = listed["items"]
            .as_array()
            .expect("items array")
            .iter()
            .find(|t| t["name"] == "cli")
            .expect("the issued token is listed under its name");
        assert_eq!(item["id"], issued["id"], "id rendu est le PK de la ligne insérée");
    }

    /// `Scenario` : « aucun corps ne peut cibler un autre subject, champs inconnus ignores ».
    #[tokio::test]
    async fn body_cannot_claim_another_subject_unknown_fields_ignored() {
        let db = test_db().await;
        let alice = issue_token(&db, "alice", "alice bootstrap", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let bob = issue_token(&db, "bob", "bob bootstrap", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        let created = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/v1/tokens",
                &alice,
                Some(serde_json::json!({"name":"x","subject":"bob","owner_id":9})),
            ))
            .await
            .expect("router does not fail");
        assert_eq!(created.status(), StatusCode::CREATED);

        let alice_list = json_body(
            app.clone()
                .oneshot(request("GET", "/api/v1/tokens", &alice, None))
                .await
                .expect("router does not fail"),
        )
        .await;
        assert!(
            alice_list["items"]
                .as_array()
                .expect("items array")
                .iter()
                .any(|t| t["name"] == "x"),
            "le subject émis vient du principal : x appartient à alice"
        );
        let bob_list = json_body(
            app.oneshot(request("GET", "/api/v1/tokens", &bob, None))
                .await
                .expect("router does not fail"),
        )
        .await;
        assert!(
            !bob_list["items"]
                .as_array()
                .expect("items array")
                .iter()
                .any(|t| t["name"] == "x"),
            "le champ subject du corps est ignoré, bob ne reçoit rien"
        );
    }

    /// `Scenario` : « name manquant en `422`, name vide accepté en `201` ».
    #[tokio::test]
    async fn missing_name_is_unprocessable_and_empty_name_accepted() {
        let db = test_db().await;
        let alice = issue_token(&db, "alice", "bootstrap", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        let missing = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/v1/tokens",
                &alice,
                Some(serde_json::json!({"expires_at":null})),
            ))
            .await
            .expect("router does not fail");
        assert_eq!(missing.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body_text(missing)
                .await
                .starts_with("Failed to deserialize the JSON body into the target type"),
            "@JsonDataError d'axum en texte, jamais une enveloppe JSON de hook"
        );

        let empty = app
            .oneshot(request(
                "POST",
                "/api/v1/tokens",
                &alice,
                Some(serde_json::json!({"name":""})),
            ))
            .await
            .expect("router does not fail");
        assert_eq!(
            empty.status(),
            StatusCode::CREATED,
            "le nom vide est stocké verbatim"
        );
    }

    /// `Scenario` : « corps JSON syntaxiquement invalide rend `400` » — `JsonSyntaxError`,
    /// distinct du `422` de désaccord de type, aucun token créé.
    #[tokio::test]
    async fn invalid_json_body_is_bad_request() {
        let db = test_db().await;
        let db_check = db.clone();
        let alice = issue_token(&db, "alice", "bootstrap", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let before = token_count(&db_check).await;
        let app = app(test_state(db));
        let bearer = format!("Bearer {alice}");

        let resp = app
            .oneshot(raw_request(
                "POST",
                "/api/v1/tokens",
                &[
                    ("Authorization", bearer.as_str()),
                    ("Content-Type", "application/json"),
                ],
                b"{name:",
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(token_count(&db_check).await, before, "aucun token n'est créé");
    }

    /// `Scenario` : « POST sans Content-Type application/json rend `415` » — l'auth a passé
    /// avant, le corps n'est jamais lu, la base est intacte.
    #[tokio::test]
    async fn post_without_json_content_type_is_unsupported_media_type() {
        let db = test_db().await;
        let db_check = db.clone();
        let alice = issue_token(&db, "alice", "bootstrap", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let before = token_count(&db_check).await;
        let app = app(test_state(db));
        let bearer = format!("Bearer {alice}");

        let resp = app
            .oneshot(raw_request(
                "POST",
                "/api/v1/tokens",
                &[("Authorization", bearer.as_str())],
                br#"{"name":"x"}"#,
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let text = body_text(resp).await;
        assert!(
            text.contains("Expected request with `Content-Type: application/json`"),
            "texte vérifié au source axum : {text}"
        );
        assert_eq!(
            token_count(&db_check).await,
            before,
            "le corps n'est jamais lu, la base est intacte"
        );
    }

    // ——— expires_at : futur stocké, malformé 422 (axum), passé 422 (crate) ———

    /// `Scenario` : « `expires_at` futur est stocke puis rendu par la liste » — sans écho
    /// d'`expires_at` dans la réponse d'émission.
    #[tokio::test]
    async fn future_expiry_is_stored_then_listed() {
        let db = test_db().await;
        let alice = issue_token(&db, "alice", "bootstrap", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        let created = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/v1/tokens",
                &alice,
                Some(serde_json::json!({"name":"temporel","expires_at":"2030-09-20T00:00:00Z"})),
            ))
            .await
            .expect("router does not fail");
        assert_eq!(created.status(), StatusCode::CREATED);
        let issued = json_body(created).await;
        assert_eq!(
            sorted_keys(&issued),
            vec!["id".to_string(), "token".to_string()],
            "expires_at n'est pas échoé à l'émission"
        );
        let secret = issued["token"].as_str().expect("token present").to_string();

        let listed = json_body(
            app.clone()
                .oneshot(request("GET", "/api/v1/tokens", &secret, None))
                .await
                .expect("router does not fail"),
        )
        .await;
        let item = listed["items"]
            .as_array()
            .expect("items array")
            .iter()
            .find(|t| t["name"] == "temporel")
            .expect("the temporel token is listed");
        let rendered = item["expires_at"].as_str().expect("expires_at rendered");
        let parsed = chrono::DateTime::parse_from_rfc3339(rendered).expect("expires_at is RFC 3339");
        let expected = chrono::DateTime::parse_from_rfc3339("2030-09-20T00:00:00Z").expect("literal");
        assert_eq!(parsed, expected, "l'expiration est rendue à la seconde près");
    }

    /// `Scenario` : « `expires_at` hors RFC 3339 rend `422` » — `JsonDataError` d'axum sur les
    /// deux formes (mot libre, naïf sans fuseau), aucun token créé par ces tentatives.
    #[tokio::test]
    async fn malformed_expiry_string_is_unprocessable() {
        let db = test_db().await;
        let db_check = db.clone();
        let alice = issue_token(&db, "alice", "bootstrap", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let before = token_count(&db_check).await;
        let app = app(test_state(db));

        let wordy = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/v1/tokens",
                &alice,
                Some(serde_json::json!({"name":"x","expires_at":"demain"})),
            ))
            .await
            .expect("router does not fail");
        assert_eq!(wordy.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let naive = app
            .oneshot(request(
                "POST",
                "/api/v1/tokens",
                &alice,
                Some(serde_json::json!({"name":"x","expires_at":"2030-09-20 00:00:00"})),
            ))
            .await
            .expect("router does not fail");
        assert_eq!(naive.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            token_count(&db_check).await,
            before,
            "aucun token créé par ces tentatives"
        );
    }

    /// `Scenario` : « `expires_at` deja passe rejeté `422`, aucun token créé » — premier émetteur
    /// de `RestError::InvalidInput` (`MRD-REST-005`, arbitré 2026-09-29), corps texte et non le
    /// JSON du hook.
    #[tokio::test]
    async fn past_expiry_is_unprocessable() {
        let db = test_db().await;
        let db_check = db.clone();
        let alice = issue_token(&db, "alice", "bootstrap", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let before = token_count(&db_check).await;
        let app = app(test_state(db));

        let resp = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/v1/tokens",
                &alice,
                Some(serde_json::json!({"name":"zombie","expires_at":"2000-01-01T00:00:00Z"})),
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let text = body_text(resp).await;
        assert_eq!(
            text, "MRD-REST-005: invalid input: expires_at must be in the future",
            "corps texte porteur du code, jamais le JSON du hook"
        );
        assert!(!text.contains("mrd_"), "aucun secret n'est rendu");
        assert_eq!(token_count(&db_check).await, before, "aucun token créé");

        let listed = json_body(
            app.oneshot(request("GET", "/api/v1/tokens", &alice, None))
                .await
                .expect("router does not fail"),
        )
        .await;
        assert!(
            !listed["items"]
                .as_array()
                .expect("items array")
                .iter()
                .any(|t| t["name"] == "zombie"),
            "le GET suivant ne montre aucun item zombie"
        );
    }

    // ——— DELETE : révocation ———

    /// `Scenario` : « revoke du proprietaire rend `204` corps vide puis disparition » — ligne
    /// physiquement absente après coup (suppression du moteur, pas un flag).
    #[tokio::test]
    async fn owner_can_revoke_their_own_token() {
        let db = test_db().await;
        let alice_token = issue_token(&db, "alice", "alice's token", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let to_revoke = issue_token(&db, "alice", "to revoke", None, "test-pepper")
            .await
            .expect("issuing succeeds");
        let db_check = db.clone();
        let app = app(test_state(db));

        let resp = app
            .clone()
            .oneshot(request(
                "DELETE",
                &format!("/api/v1/tokens/{}", to_revoke.id),
                &alice_token,
                None,
            ))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(
            axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .expect("readable body")
                .is_empty(),
            "204 sans corps"
        );

        let listed = app
            .oneshot(request("GET", "/api/v1/tokens", &alice_token, None))
            .await
            .expect("router does not fail");
        let body = json_body(listed).await;
        assert_eq!(body["total_items"], 1, "total diminué");
        assert!(
            !body["items"]
                .as_array()
                .expect("items array")
                .iter()
                .any(|item| item["id"] == to_revoke.id),
            "aucun item d'id T ne subsiste"
        );

        let row = ApiToken::find_by_id(to_revoke.id)
            .one(&db_check)
            .await
            .expect("query succeeds");
        assert_eq!(row, None, "la ligne est physiquement absente");
    }

    /// `Scenario` : « revoke d'un id d'autrui rend `404` et laisse la victime vivante » —
    /// DELETE filtré sur `subject`, plus de `403`, plus d'oracle d'existence (arbitré
    /// 2026-09-29) : corps identique à un id inexistant.
    #[tokio::test]
    async fn cannot_revoke_someone_else_token() {
        let db = test_db().await;
        let alice_token = issue_token(&db, "alice", "alice's token", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let bobs_token = issue_token(&db, "bob", "bob's token", None, "test-pepper")
            .await
            .expect("issuing succeeds");
        let db_check = db.clone();
        let app = app(test_state(db));

        let resp = app
            .oneshot(request(
                "DELETE",
                &format!("/api/v1/tokens/{}", bobs_token.id),
                &alice_token,
                None,
            ))
            .await
            .expect("router does not fail");
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "plus de 403 : le 404 est l'unique rendu"
        );
        assert_eq!(
            body_text(resp).await,
            "MRD-REST-001: resource not found",
            "corps identique à un id inexistant — alice n'apprend rien du titulaire"
        );

        // victime vivante, jamais révoquée par l'appelante non-propriétaire.
        let principal = crate::auth::validate_token(&db_check, &bobs_token.token, "test-pepper")
            .await
            .expect("bob's token remains valid");
        assert_eq!(principal.subject, "bob");
    }

    /// `Scenario` : « id inconnu ou negatif rend `404` » — `id` négatif est un `i32` recevable
    /// parsé par `Path` : c'est la requête filtrée qui tranche, pas le routeur.
    #[tokio::test]
    async fn deleting_an_unknown_token_returns_not_found() {
        let db = test_db().await;
        let alice_token = issue_token(&db, "alice", "alice's token", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        for uri in ["/api/v1/tokens/999999", "/api/v1/tokens/-1"] {
            let resp = app
                .clone()
                .oneshot(request("DELETE", uri, &alice_token, None))
                .await
                .expect("router does not fail");
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
            assert_eq!(body_text(resp).await, "MRD-REST-001: resource not found", "{uri}");
        }
    }

    /// `Scenario` : « id de chemin non numerique ou trop grand rend `400` » — rejet `Path`
    /// d'axum, le handler n'est pas atteint, aucune requête SQL.
    #[tokio::test]
    async fn non_numeric_or_overflowing_path_id_is_bad_request() {
        let db = test_db().await;
        let alice = issue_token(&db, "alice", "alice's token", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        for uri in ["/api/v1/tokens/abc", "/api/v1/tokens/2147483648"] {
            let resp = app
                .clone()
                .oneshot(request("DELETE", uri, &alice, None))
                .await
                .expect("router does not fail");
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "{uri} doit être rejeté par @Path"
            );
        }
    }

    /// `Scenario` : « methodes non enregistrees en `405`, chemin sans prefixe en `404` » —
    /// en-tête `Allow` posé par axum, rien hors du nest `/api/v1`.
    #[tokio::test]
    async fn other_methods_are_method_not_allowed_and_unprefixed_paths_are_not_found() {
        let db = test_db().await;
        let app = app(test_state(db));

        for (method, uri) in [
            ("PUT", "/api/v1/tokens"),
            ("PATCH", "/api/v1/tokens"),
            ("POST", "/api/v1/tokens/1"),
        ] {
            let resp = app
                .clone()
                .oneshot(raw_request(
                    method,
                    uri,
                    &[("Content-Type", "application/json")],
                    b"{}",
                ))
                .await
                .expect("router does not fail");
            assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED, "{method} {uri}");
            assert!(
                resp.headers().get("allow").is_some(),
                "l'en-tête Allow est posé par axum sur {method} {uri}"
            );
        }

        let resp = app
            .oneshot(raw_request("GET", "/tokens", &[], &[]))
            .await
            .expect("router does not fail");
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "rien n'existe hors du nest /api/v1"
        );
    }
}
