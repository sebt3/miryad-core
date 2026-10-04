//! Endpoint admin listant les utilisateurs et leurs groupes (issue #4). Ne modifie aucune
//! donnée métier — la première vue d'un sujet crée toutefois sa ligne `miryad_users`
//! (`resolve_user` get-or-create s'exécute avant le garde, contrat de toute route authentifiée,
//! cf. `users::user.sdd`). Pas un `MiryadResource` : `User` n'a pas la sémantique CRUD (pas
//! d'owner, jamais de write — Authentik reste la seule source de vérité pour l'appartenance aux
//! groupes, cf. `users::sync_group_memberships`). Un routeur dédié, dans l'esprit
//! d'`auth::auth_router`.

use std::collections::HashMap;

use axum::extract::{FromRef, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder};
use serde::{Deserialize, Serialize};

use crate::auth::{AuthPrincipal, MiryadAuthState};
use crate::query::{PagedResult, Pagination};
use crate::rest::error::RestError;
use crate::users::{group, is_admin, membership, resolve_user, user};

/// Ligne de la liste paginée `GET /api/v1/users` — exactement les clés `id`,
/// `subject`, `email`, `groups` ; jamais `display_name`, `created_at` ni une colonne
/// d'une autre table interne (tokens compris).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UserSummary {
    /// Clé primaire interne de la ligne `miryad_users`.
    pub id: i32,
    /// Sujet OIDC persisté sur la ligne locale.
    pub subject: String,
    /// Colonne `email` de la ligne, pas le credential de la requête (le bearer API
    /// porte un email `None`) ; sérialisé `null` quand absent.
    pub email: Option<String>,
    /// Noms des groupes de l'utilisateur, lus par `groups_by_user` — tableau vide
    /// sans appartenance, ordre non contractuel.
    pub groups: Vec<String>,
}

#[derive(Deserialize)]
struct ListParams {
    page: Option<u64>,
    per_page: Option<u64>,
}

/// Monte `GET /api/v1/users` — liste paginée `{ id, subject, email, groups }`, réservée aux
/// membres du groupe admin (`AdminOnly`, cf. `docs/architecture.md` section RBAC). Réutilise
/// `MiryadAuthState` comme les autres routeurs — rien de nouveau à composer côté app. Préfixe
/// `/api/v1` figé, cohérent avec `resource_router` (feature 6).
pub fn users_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>,
{
    Router::new().nest("/api/v1", Router::new().route("/users", get(list_users_handler)))
}

async fn list_users_handler(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Query(params): Query<ListParams>,
) -> Result<Json<PagedResult<UserSummary>>, RestError> {
    let caller = resolve_user(&auth.db, &principal.subject, principal.email.as_deref()).await?;
    if !is_admin(&auth.db, caller.id).await? {
        // `rest/admin.sdd` `Must` (arbitré 2026-09-29) : la tentative d'accès admin est la trace
        // la plus utile — `warn!` avec le subject, jamais le token ni le cookie.
        tracing::warn!(subject = %principal.subject, "admin access denied: GET /api/v1/users");
        return Err(RestError::Forbidden);
    }

    let pagination = Pagination::from_raw(params.page, params.per_page);
    // `rest/admin.sdd` `Must` (arbitré 2026-09-29) : liste ordonnée par id croissant — pagination
    // déterministe, même contrat que `rest::core::list`.
    let paginator = user::Entity::find()
        .order_by_asc(user::Column::Id)
        .paginate(&auth.db, pagination.per_page);
    let totals = paginator.num_items_and_pages().await?;
    // `query.sdd` borne `page >= 1` : `saturating_sub(1)` ne sature jamais, l'index rendu est
    // exactement `page - 1` (purge `arithmetic_side_effects` de `tooling.sdd`).
    let users = paginator.fetch_page(pagination.page.saturating_sub(1)).await?;

    let mut groups_by_user = groups_by_user(&auth.db, users.iter().map(|u| u.id)).await?;

    let items = users
        .into_iter()
        .map(|u| UserSummary {
            groups: groups_by_user.remove(&u.id).unwrap_or_default(),
            id: u.id,
            subject: u.subject,
            email: u.email,
        })
        .collect();

    Ok(Json(PagedResult {
        items,
        page: pagination.page,
        per_page: pagination.per_page,
        total_items: totals.number_of_items,
        total_pages: totals.number_of_pages,
    }))
}

/// Deux requêtes (memberships puis groupes), jamais une par utilisateur — évite le N+1 sur une
/// page de résultats. `is_in` sur une liste vide est explicitement court-circuité (cf.
/// `graphql::principal::load_principal`, même précaution) plutôt que délégué au driver.
/// `pub(crate)` : réutilisée par `rest::me` (issue #24), même besoin "groupes d'un utilisateur".
pub(crate) async fn groups_by_user(
    db: &sea_orm::DatabaseConnection,
    user_ids: impl Iterator<Item = i32>,
) -> Result<HashMap<i32, Vec<String>>, sea_orm::DbErr> {
    let user_ids: Vec<i32> = user_ids.collect();
    if user_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let memberships = membership::Entity::find()
        .filter(membership::Column::UserId.is_in(user_ids))
        .all(db)
        .await?;
    if memberships.is_empty() {
        return Ok(HashMap::new());
    }

    let group_ids: Vec<i32> = memberships.iter().map(|m| m.group_id).collect();
    let group_names: HashMap<i32, String> = group::Entity::find()
        .filter(group::Column::Id.is_in(group_ids))
        .all(db)
        .await?
        .into_iter()
        .map(|g| (g.id, g.name))
        .collect();

    let mut result: HashMap<i32, Vec<String>> = HashMap::new();
    for m in memberships {
        if let Some(name) = group_names.get(&m.group_id) {
            result.entry(m.user_id).or_default().push(name.clone());
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    //! Conversion des treize `Scenario` de `./admin.sdd` — un test nommé distinct par scenario
    //! (`Tasks` « Convertir les treize Scenario »), plus le test de pagination déterministe
    //! (`ORDER BY id`, arbitré 2026-09-29) et la capture `warn!` du `403` (arbitré 2026-09-29).

    use super::*;
    use crate::auth::issue_token;
    use crate::auth::oidc::MockOidcClient;
    use crate::migration::Migrator;
    use crate::users::membership::trace_capture::capture_traces;
    use crate::users::{group, membership, sync_group_memberships};
    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode};
    use sea_orm::{Database, DatabaseConnection, DbBackend, MockDatabase, MockExecResult, Transaction};
    use sea_orm_migration::MigratorTrait;
    use std::collections::BTreeMap;
    use tower::ServiceExt;

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
            .merge(users_router::<MiryadAuthState>())
            .with_state(state)
    }

    async fn json_body(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        serde_json::from_slice(&bytes).expect("valid JSON body")
    }

    /// `(statut, en-têtes, corps texte)` — les en-têtes sont capturés avant le corps.
    async fn serve(app: Router, req: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let resp = app.oneshot(req).await.expect("router does not fail");
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    async fn serve_json(app: Router, req: Request<Body>) -> (StatusCode, serde_json::Value) {
        let (status, _headers, body) = serve(app, req).await;
        let json: serde_json::Value = serde_json::from_str(&body).expect("valid JSON body");
        (status, json)
    }

    fn get_request(uri: &str, token: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(uri)
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .expect("valid request")
    }

    async fn admin_token(db: &DatabaseConnection) -> String {
        let admin = resolve_user(db, "admin-sub", None)
            .await
            .expect("resolve succeeds");
        sync_group_memberships(db, admin.id, &["admin".to_string()])
            .await
            .expect("sync succeeds");
        issue_token(db, "admin-sub", "test", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token
    }

    // ——— Scenario 1 : admin obtient la liste paginée avec les groupes de chaque utilisateur ———

    #[tokio::test]
    async fn admin_sees_paginated_users_with_their_groups() {
        let db = test_db().await;
        let alice = resolve_user(&db, "alice-sub", Some("alice@example.com"))
            .await
            .expect("resolve succeeds");
        sync_group_memberships(&db, alice.id, &["editors".to_string(), "viewers".to_string()])
            .await
            .expect("sync succeeds");
        admin_token(&db).await;
        // bob n'a jamais rejoint de groupe — doit apparaître avec groups: [].
        resolve_user(&db, "bob-sub", None)
            .await
            .expect("resolve succeeds");

        let token = issue_token(&db, "admin-sub", "test", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        let (status, body) = serve_json(app, get_request("/api/v1/users", &token)).await;
        assert_eq!(status, StatusCode::OK);

        assert_eq!(body["total_items"], 3);
        assert_eq!(body["total_pages"], 1, "défaut per_page 100 : une seule page");
        assert_eq!(body["per_page"], 100, "l'écho per_page est la valeur normalisée");
        let items = body["items"].as_array().expect("items array");
        assert_eq!(items.len(), 3);
        for item in items {
            let mut keys: Vec<&str> = item
                .as_object()
                .expect("item object")
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                vec!["email", "groups", "id", "subject"],
                "le trigramme est exact, jamais display_name ni created_at"
            );
        }

        let alice_entry = items
            .iter()
            .find(|u| u["subject"] == "alice-sub")
            .expect("alice present");
        assert_eq!(alice_entry["email"], "alice@example.com");
        let mut groups: Vec<&str> = alice_entry["groups"]
            .as_array()
            .expect("groups array")
            .iter()
            .map(|g| g.as_str().expect("group is a string"))
            .collect();
        groups.sort_unstable();
        assert_eq!(groups, vec!["editors", "viewers"]);

        let bob_entry = items
            .iter()
            .find(|u| u["subject"] == "bob-sub")
            .expect("bob present");
        assert_eq!(bob_entry["email"], serde_json::Value::Null);
        assert_eq!(bob_entry["groups"].as_array().expect("groups array").len(), 0);
    }

    // ——— Scenario 2 : le non-admin authentifié est refusé ———

    #[tokio::test]
    async fn non_admin_is_forbidden() {
        let db = test_db().await;
        resolve_user(&db, "alice-sub", None)
            .await
            .expect("resolve succeeds");
        let token = issue_token(&db, "alice-sub", "test", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        let (status, _headers, body) = serve(app, get_request("/api/v1/users", &token)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, "MRD-REST-002: access denied");
        assert!(
            !body.contains("\"items\""),
            "aucune page d'utilisateurs n'est rendue, ni partielle : {body}"
        );
    }

    // ——— Trace `warn!` avec subject sur chaque 403 (arbitré 2026-09-29) ———

    /// `Tasks` « Trace `warn!` avec `@subject` sur chaque `403` de la route » — TEST ROUGE avant
    /// implémentation. Capture via le helper partagé de `users::membership::trace_capture` ;
    /// jamais le token ni le cookie dans la ligne.
    #[tokio::test]
    async fn non_admin_403_emits_warn_trace_with_subject() {
        let db = test_db().await;
        resolve_user(&db, "alice-sub", None)
            .await
            .expect("resolve succeeds");
        let token = issue_token(&db, "alice-sub", "test", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        let (_guard, captured) = capture_traces();
        let (status, _headers, _body) = serve(app, get_request("/api/v1/users", &token)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let lines = captured.lines();
        assert!(
            lines
                .iter()
                .any(|(level, line)| *level == tracing::Level::WARN && line.contains("subject=alice-sub")),
            "le 403 émet une trace warn! portant le subject : {lines:?}"
        );
        for (_level, line) in &lines {
            assert!(
                !line.contains(&token),
                "la trace ne porte jamais le token : {line}"
            );
        }
    }

    // ——— Scenario 3 : requête anonyme refusée, même avec des paramètres invalides ———

    #[tokio::test]
    async fn anonymous_request_is_unauthorized() {
        // MockDatabase non préparée : toute requête SQL échouerait ; le journal vide prouve
        // que seul l'extracteur AuthPrincipal a tranché, avant le Query.
        let db = MockDatabase::new(DbBackend::Sqlite).into_connection();
        let log_db = db.clone();

        let (status, _headers, body) = serve(
            app(test_state(db)),
            Request::builder()
                .method("GET")
                .uri("/api/v1/users?page=abc")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, "MRD-AUTH-001: not authenticated (no session cookie)");
        assert!(
            log_db.into_transaction_log().is_empty(),
            "le 400 de parsing de query ne se déclenche jamais : AuthPrincipal passe avant Query"
        );
    }

    // ——— Scenario 4 : token invalide ou expiré refusé ———

    #[tokio::test]
    async fn invalid_or_expired_token_is_unauthorized() {
        let db = test_db().await;
        let past = chrono::DateTime::from_timestamp(chrono::Utc::now().timestamp() - 3600, 0)
            .expect("valid timestamp");
        let expired = issue_token(&db, "expired-sub", "gone", Some(past), "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;

        let (status, _headers, body) = serve(
            app(test_state(db.clone())),
            get_request("/api/v1/users", "mrd_never_issued"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, "MRD-AUTH-014: invalid or unknown API token");

        let (status, _headers, body) =
            serve(app(test_state(db)), get_request("/api/v1/users", &expired)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, "MRD-AUTH-015: expired API token");
    }

    // ——— Scenario 5 : méthode refusée avant toute authentification ———

    #[tokio::test]
    async fn post_is_method_not_allowed_without_auth() {
        let db = MockDatabase::new(DbBackend::Sqlite).into_connection();
        let log_db = db.clone();

        let (status, headers, body) = serve(
            app(test_state(db)),
            Request::builder()
                .method("POST")
                .uri("/api/v1/users")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert!(body.is_empty(), "corps vide");
        // Écart spec↔code consigné au rapport : admin.sdd attend `Allow: GET` ; axum 0.8.9
        // compose `GET,HEAD` sur un `get()` (source axum, gelé par `rest/mod.sdd`). Verrou réel.
        let allow = headers
            .get("allow")
            .and_then(|v| v.to_str().ok())
            .expect("en-tête Allow présent")
            .to_string();
        let methods: Vec<&str> = allow.split(',').map(str::trim).collect();
        assert!(methods.contains(&"GET"), "Allow contient GET : {allow}");
        assert_eq!(methods.len(), 2, "GET et HEAD seulement : {allow}");
        assert!(
            log_db.into_transaction_log().is_empty(),
            "aucune authentification ni requête SQL : le routeur de méthodes répond d'abord"
        );
    }

    // ——— Scenario 6 : paramètre de pagination invalide ———

    #[tokio::test]
    async fn malformed_pagination_is_bad_request_before_guard() {
        let db = test_db().await;
        resolve_user(&db, "alice-sub", None)
            .await
            .expect("resolve succeeds");
        let token = issue_token(&db, "alice-sub", "test", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;

        let (status, _headers, _body) =
            serve(app(test_state(db)), get_request("/api/v1/users?page=abc", &token)).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "le rejet de l'extracteur Query sur u64 précède le garde admin (pas 403)"
        );
    }

    // ——— Scenario 7 : la page demandée est respectée ———

    #[tokio::test]
    async fn pagination_params_are_respected() {
        let db = test_db().await;
        admin_token(&db).await;
        for n in 0..3 {
            resolve_user(&db, &format!("user-{n}"), None)
                .await
                .expect("resolve succeeds");
        }
        // 4 utilisateurs au total (admin + 3) — page 2 à per_page=3 ne renvoie que le dernier.

        let token = issue_token(&db, "admin-sub", "test", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;
        let app = app(test_state(db));

        let resp = app
            .oneshot(get_request("/api/v1/users?page=2&per_page=3", &token))
            .await
            .expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);

        let body = json_body(resp).await;
        assert_eq!(body["page"], 2);
        assert_eq!(body["per_page"], 3);
        assert_eq!(body["total_items"], 4);
        assert_eq!(body["total_pages"], 2);
        assert_eq!(body["items"].as_array().expect("items array").len(), 1);
    }

    // ——— Scenario 8 : paramètres nuls normalisés ———

    #[tokio::test]
    async fn null_pagination_params_are_normalized() {
        let db = test_db().await;
        admin_token(&db).await;
        for n in 0..3 {
            resolve_user(&db, &format!("user-{n}"), None)
                .await
                .expect("resolve succeeds");
        }
        let token = issue_token(&db, "admin-sub", "test", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;

        let (status, body) = serve_json(
            app(test_state(db)),
            get_request("/api/v1/users?page=0&per_page=0", &token),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["page"], 1);
        assert_eq!(body["per_page"], 1);
        assert_eq!(
            body["items"].as_array().expect("items array").len(),
            1,
            "Pagination::from_raw est passé avant fetch_page"
        );
    }

    // ——— Scenario 9 : per_page au-dessus du plafond ———

    #[tokio::test]
    async fn per_page_above_cap_is_echoed_capped() {
        let db = test_db().await;
        let token = admin_token(&db).await;

        let (status, body) = serve_json(
            app(test_state(db)),
            get_request("/api/v1/users?per_page=50000", &token),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["per_page"], crate::query::MAX_PER_PAGE);
        assert_eq!(body["page"], 1);
    }

    // ——— Scenario 10 : page au-delà de la dernière ———

    #[tokio::test]
    async fn out_of_range_page_keeps_empty_items_and_totals() {
        let db = test_db().await;
        admin_token(&db).await;
        resolve_user(&db, "user-1", None).await.expect("resolve succeeds");
        let token = issue_token(&db, "admin-sub", "test", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token;

        let (status, body) = serve_json(
            app(test_state(db)),
            get_request("/api/v1/users?page=99&per_page=1", &token),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"].as_array().expect("items array").len(), 0);
        assert_eq!(body["page"], 99, "rien n'est ramené à la dernière page existante");
        assert_eq!(body["total_items"], 2);
        assert_eq!(body["total_pages"], 2);
    }

    // ——— ORDER BY id ascendant (arbitré 2026-09-29) ———

    /// `Must` « La liste est ordonnée par @id croissant » — TEST ROUGE avant implémentation :
    /// preuve d'émission par `MockDatabase` piloté depuis la route (sous `SQLite` le balayage
    /// `rowid` rend le contenu indiscernable, seule la clause émise est observable).
    #[tokio::test]
    async fn admin_list_apply_order_by_id_asc() {
        let now = chrono::Utc::now();
        let token_row = crate::auth::token::Model {
            id: 1,
            subject: "admin-sub".to_string(),
            name: "mock".to_string(),
            token_hash: "mock-hash".to_string(),
            created_at: now,
            expires_at: None,
            last_used_at: None,
        };
        let admin_row = user::Model {
            id: 5,
            subject: "admin-sub".to_string(),
            email: None,
            display_name: None,
            created_at: now,
        };
        let group_row = group::Model {
            id: 1,
            name: "admin".to_string(),
            created_at: now,
        };
        let membership_row = membership::Model {
            id: 9,
            user_id: 5,
            group_id: 1,
        };
        let count_row = BTreeMap::from([("num_items".to_string(), sea_orm::Value::BigInt(Some(4)))]);
        // Ordre des requêtes de la route : SELECT token (validate) → UPDATE last_used_at (exec)
        // → relecture token → resolve_user → is_member (groupes puis memberships) → COUNT →
        // SELECT page → memberships de la page (vides : court-circuit de groups_by_user).
        let db = MockDatabase::new(DbBackend::Sqlite)
            .append_query_results([[token_row.clone()]])
            .append_query_results([[token_row]])
            .append_query_results([[admin_row.clone()]])
            .append_query_results([[group_row]])
            .append_query_results([[membership_row]])
            .append_query_results([[count_row]])
            .append_query_results([[admin_row]])
            .append_query_results::<membership::Model, _, _>([[]])
            .append_exec_results([MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .into_connection();

        let (status, _headers, _body) = serve(
            app(test_state(db.clone())),
            get_request("/api/v1/users?page=2&per_page=3", "mrd_whatever"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let page_sql = db
            .into_transaction_log()
            .iter()
            .flat_map(Transaction::statements)
            .map(ToString::to_string)
            // Le SELECT de la page : la table entière, sans WHERE, avec LIMIT (le COUNT
            // englobant ne porte jamais de LIMIT).
            .find(|sql| {
                sql.starts_with("SELECT")
                    && sql.contains(r#"FROM "miryad_users""#)
                    && !sql.contains("WHERE")
                    && !sql.contains("COUNT")
            })
            .expect("le SELECT de la page d'utilisateurs a été émis");
        assert!(
            page_sql.contains(r#"ORDER BY "miryad_users"."id" ASC"#),
            "`ORDER BY` sur l'id est appliqué avant paginate : {page_sql}"
        );
    }

    /// Pagination déterministe sur `SQLite` réel : deux pages consécutives couvrent la table
    /// sans saut ni doublon. Consigné au rapport : vert même sans clause sous `SQLite`
    /// (balayage `rowid`) — la preuve rouge est le test de statement ci-dessus.
    #[tokio::test]
    async fn admin_pagination_deux_pages_sans_saut_ni_doublon() {
        let db = test_db().await;
        let token = admin_token(&db).await;
        for n in 0..4 {
            resolve_user(&db, &format!("user-{n}"), None)
                .await
                .expect("resolve succeeds");
        }

        let mut seen: Vec<i64> = Vec::new();
        for page_index in 1..=2 {
            let (status, body) = serve_json(
                app(test_state(db.clone())),
                get_request(&format!("/api/v1/users?page={page_index}&per_page=3"), &token),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            for item in body["items"].as_array().expect("items array") {
                let id = item["id"].as_i64().expect("id");
                assert!(!seen.contains(&id), "l'utilisateur {id} est dupliqué entre pages");
                seen.push(id);
            }
        }
        assert_eq!(seen.len(), 5, "les cinq lignes sont couvertes sans saut");
        assert!(
            seen.windows(2).all(|w| w[1] > w[0]),
            "le parcours suit l'id croissant : {seen:?}"
        );
    }

    // ——— Scenario 11 : itérateur vide — aucune requête de groupes ———

    #[tokio::test]
    async fn groups_by_user_empty_input_issues_no_query() {
        let db = MockDatabase::new(DbBackend::Sqlite).into_connection();
        let log_db = db.clone();

        let map = groups_by_user(&db, std::iter::empty())
            .await
            .expect("le court-circuit ne peut pas échouer");
        assert!(map.is_empty());
        assert!(
            log_db.into_transaction_log().is_empty(),
            "aucune instruction SQL émise : le court-circuit précède le is_in"
        );
    }

    // ——— Scenario 12 : deux requêtes au plus pour une page ———

    #[tokio::test]
    async fn groups_by_user_caps_at_two_queries() {
        let now = chrono::Utc::now();
        let db = MockDatabase::new(DbBackend::Sqlite)
            .append_query_results([[
                membership::Model {
                    id: 1,
                    user_id: 10,
                    group_id: 20,
                },
                membership::Model {
                    id: 2,
                    user_id: 11,
                    group_id: 21,
                },
            ]])
            .append_query_results([[
                group::Model {
                    id: 20,
                    name: "editors".to_string(),
                    created_at: now,
                },
                group::Model {
                    id: 21,
                    name: "viewers".to_string(),
                    created_at: now,
                },
            ]])
            .into_connection();

        let map = groups_by_user(&db, [10, 11].into_iter())
            .await
            .expect("two scripted queries");
        assert_eq!(
            map.get(&10).map(Vec::as_slice),
            Some(&["editors".to_string()][..])
        );
        assert_eq!(
            map.get(&11).map(Vec::as_slice),
            Some(&["viewers".to_string()][..])
        );

        let log = db.into_transaction_log();
        let stmts: Vec<String> = log
            .iter()
            .flat_map(Transaction::statements)
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            stmts.len(),
            2,
            "exactement deux SELECT, jamais un par utilisateur"
        );
        assert!(
            stmts.iter().all(|sql| sql.starts_with("SELECT"))
                && stmts.iter().any(|sql| sql.contains("miryad_group_memberships"))
                && stmts.iter().any(|sql| sql.contains("miryad_groups")),
            "un SELECT de memberships et un SELECT de groupes au total : {stmts:?}"
        );
    }

    // ——— Scenario 13 : chemin inconnu sous le préfixe monté ———

    #[tokio::test]
    async fn unknown_path_under_mount_is_not_found() {
        let db = MockDatabase::new(DbBackend::Sqlite).into_connection();
        let log_db = db.clone();

        let (status, _headers, _body) = serve(
            app(test_state(db)),
            Request::builder()
                .method("GET")
                .uri("/api/v1/utilisateurs")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(
            log_db.into_transaction_log().is_empty(),
            "ni AuthPrincipal ni ListParams évalués, aucune requête SQL"
        );
    }
}
