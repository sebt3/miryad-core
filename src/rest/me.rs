//! Endpoint self-service exposant l'identité/les groupes du principal courant (issue #24) — un
//! frontend a besoin de savoir "qui je suis" (ex: afficher ou non un lien de nav vers une page
//! admin) sans accès direct aux tables internes de miryad-core, que le projet évite justement de
//! réclamer côté app. Jamais les infos d'un autre utilisateur, dans l'esprit de `tokens_router`.

use axum::extract::{FromRef, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::auth::{AuthPrincipal, MiryadAuthState};
use crate::rest::admin::groups_by_user;
use crate::rest::error::RestError;
use crate::users::resolve_user;

/// Corps JSON de succès de `GET /api/v1/me` — trois clés et trois seulement, émises
/// en ordre de déclaration (`subject`, `email`, `groups`) ; jamais de champ `id`, la
/// clé interne reste hors du frontend.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MeResponse {
    /// `subject` de la ligne `miryad_users` résolue depuis le principal authentifié —
    /// jamais une cible passée par le client.
    pub subject: String,
    /// Colonne `email` de cette même ligne — snapshot de première vue posé par
    /// `resolve_user`, jamais rafraîchi depuis la session ; sérialisé `null` quand absent.
    pub email: Option<String>,
    /// Noms des groupes de l'appelante, lus par `groups_by_user` puis triés par nom
    /// croissant (arbitré 2026-09-29) — tableau vide sans appartenance.
    pub groups: Vec<String>,
}

/// Monte `GET /api/v1/me` — n'importe quel principal authentifié (dual-auth), toujours restreint
/// à son propre compte, jamais `AdminOnly` (contrairement à `users_router`). Réutilise
/// `MiryadAuthState` comme les autres routeurs. Préfixe `/api/v1` figé, cohérent avec
/// `resource_router`/`tokens_router`/`users_router`.
pub fn me_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>,
{
    Router::new().nest("/api/v1", Router::new().route("/me", get(me_handler)))
}

async fn me_handler(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
) -> Result<Json<MeResponse>, RestError> {
    let caller = resolve_user(&auth.db, &principal.subject, principal.email.as_deref()).await?;
    let mut groups = groups_by_user(&auth.db, std::iter::once(caller.id))
        .await?
        .remove(&caller.id)
        .unwrap_or_default();
    // `rest/me.sdd` `Must` (arbitré 2026-09-29) : tableau `groups` trié par nom croissant —
    // ordre stable pour le frontend, miroir du contrat « toute liste est ordonnée » de
    // `rest/core.sdd`. Tri explicite ici et non dans `groups_by_user` : la liste admin garde
    // son propre contrat (`rest/admin.sdd`), et l'ordre de sortie de l'agrégation est celui du
    // parcours des memberships, indépendamment de tout `ORDER BY` SQL sur les noms.
    groups.sort();

    Ok(Json(MeResponse {
        subject: caller.subject,
        email: caller.email,
        groups,
    }))
}

#[cfg(test)]
mod tests {
    //! Conversion des 13 `Scenario` de `./me.sdd` — un test nommé distinct par scenario
    //! (`Tasks` « Convertir les 13 Scenario »), plus le test du tri `groups` (arbitré
    //! 2026-09-29). Les octets bruts des corps sont lus via `axum::body::to_bytes` :
    //! `serde_json::Value` en features par défaut est un `BTreeMap` qui trie les clés et
    //! perd l'ordre du fil.

    use super::*;
    use crate::auth::cookie::build_set_cookie;
    use crate::auth::issue_token;
    use crate::auth::oidc::{MockOidcClient, OidcIdentity};
    use crate::migration::Migrator;
    use crate::users::{sync_group_memberships, user};
    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode};
    use sea_orm::{
        ColumnTrait, ConnectionTrait, Database, DatabaseConnection, DbBackend, EntityTrait, MockDatabase,
        QueryFilter,
    };
    use sea_orm_migration::MigratorTrait;
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
            .merge(me_router::<MiryadAuthState>())
            .with_state(state)
    }

    async fn json_body(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        serde_json::from_slice(&bytes).expect("valid JSON body")
    }

    /// `(statut, en-têtes, octets du corps)` — les octets bruts sont l'autorité pour
    /// l'ordre des clés et le littéral `null` (scenarios 6 et 11).
    async fn serve(app: Router, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let resp = app.oneshot(req).await.expect("router does not fail");
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        (status, headers, body.to_vec())
    }

    fn get_request(token: &str) -> Request<Body> {
        request("GET", "/api/v1/me", token)
    }

    fn request(method: &str, uri: &str, token: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .expect("valid request")
    }

    // ——— Cookie de session valide (helpers des scenarios 1, 3, 8, pattern de `auth/dual.rs`) ———

    /// JWT tri-segment dont la claim `exp` vaut `exp` (base64 url-safe sans bourrage).
    fn make_jwt(exp: u64) -> String {
        use base64::Engine;
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

    /// Cookie `miryad_session` scellé sous la clé de l'état pour `subject`, avec l'`email`
    /// de payload demandé (`exp` dans une heure).
    fn session_cookie(state: &MiryadAuthState, subject: &str, email: Option<&str>) -> String {
        cookie_pair(&build_set_cookie(
            &OidcIdentity {
                id_token: make_jwt(future_exp()),
                subject: subject.to_string(),
                email: email.map(str::to_string),
                preferred_username: None,
            },
            &state.cookie_key,
            state.secure_cookies,
        ))
    }

    fn cookie_request(cookie: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri("/api/v1/me")
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request")
    }

    async fn issue(db: &DatabaseConnection, subject: &str) -> String {
        issue_token(db, subject, "test", None, "test-pepper")
            .await
            .expect("issuing succeeds")
            .token
    }

    fn assert_json_content(headers: &HeaderMap) {
        let content_type = headers
            .get(axum::http::header::CONTENT_TYPE)
            .expect("Content-Type présent")
            .to_str()
            .expect("ASCII")
            .to_string();
        assert!(
            content_type.starts_with("application/json"),
            "succès rendu en JSON : {content_type}"
        );
    }

    // ——— Scenario 1 : callante cookie de session rend son identité et ses groupes ———

    #[tokio::test]
    async fn me_cookie_session_rend_identity_et_groupes() {
        let db = test_db().await;
        let alice = resolve_user(&db, "alice-sub", Some("alice@example.com"))
            .await
            .expect("resolve succeeds");
        sync_group_memberships(&db, alice.id, &["admin".to_string()])
            .await
            .expect("sync succeeds");
        let state = test_state(db);
        let cookie = session_cookie(&state, "alice-sub", None);

        let (status, headers, body) = serve(app(state), cookie_request(&cookie)).await;
        assert_eq!(status, StatusCode::OK);
        assert_json_content(&headers);
        let body: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(body["subject"], "alice-sub");
        assert_eq!(body["email"], "alice@example.com");
        assert_eq!(body["groups"], serde_json::json!(["admin"]));
    }

    // ——— Scenario 2 : aucun groupe d'autrui n'est rendu ———

    #[tokio::test]
    async fn me_never_reports_another_users_groups() {
        let db = test_db().await;
        resolve_user(&db, "bob-sub", None)
            .await
            .expect("resolve succeeds");
        let bob = user::Entity::find()
            .filter(user::Column::Subject.eq("bob-sub"))
            .one(&db)
            .await
            .expect("readable")
            .expect("bob provisioned");
        sync_group_memberships(&db, bob.id, &["admin".to_string()])
            .await
            .expect("sync succeeds");
        resolve_user(&db, "alice-sub", None)
            .await
            .expect("resolve succeeds");
        let token = issue(&db, "alice-sub").await;

        let (status, _headers, body) = serve(app(test_state(db)), get_request(&token)).await;
        assert_eq!(status, StatusCode::OK);
        let raw = String::from_utf8(body).expect("utf-8 body");
        assert!(raw.contains("\"subject\":\"alice-sub\""), "{raw}");
        assert!(raw.contains("\"groups\":[]"), "{raw}");
        assert!(
            !raw.contains("admin"),
            "la chaîne admin n'apparaît nulle part : la lookup est indexée sur l'id de l'appelante : {raw}"
        );
    }

    // ——— Scenario 3 : l'email de la base prime sur l'email de la session ———

    #[tokio::test]
    async fn me_email_de_la_base_prime_sur_celui_de_la_session() {
        let db = test_db().await;
        resolve_user(&db, "carol-sub", Some("old@example.com"))
            .await
            .expect("resolve succeeds");
        let state = test_state(db);
        let cookie = session_cookie(&state, "carol-sub", Some("new@example.com"));

        let (status, _headers, body) = serve(app(state), cookie_request(&cookie)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            String::from_utf8_lossy(&body).contains("old@example.com"),
            "le @email rendu est la colonne de la ligne résolue : {body:?}"
        );
        assert!(
            !String::from_utf8_lossy(&body).contains("new@example.com"),
            "resolve_user n'est pas un upsert : l'email de la session n'est jamais relu"
        );
    }

    // ——— Scenario 4 : token sans email au principal voit l'email en base ———

    #[tokio::test]
    async fn me_reports_own_identity_and_groups() {
        let db = test_db().await;
        let alice = resolve_user(&db, "alice-sub", Some("alice@example.com"))
            .await
            .expect("resolve succeeds");
        sync_group_memberships(&db, alice.id, &["admin".to_string()])
            .await
            .expect("sync succeeds");
        let token = issue(&db, "alice-sub").await;

        let resp = app(test_state(db))
            .oneshot(get_request(&token))
            .await
            .expect("router does not fail");

        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["subject"], "alice-sub");
        assert_eq!(
            body["email"], "alice@example.com",
            "chemin token : principal.email vaut None, la source est la colonne email de la base"
        );
        assert_eq!(body["groups"], serde_json::json!(["admin"]));
    }

    // ——— Scenario 5 : subject inconnu est inséré avant le premier rendu ———

    #[tokio::test]
    async fn me_subject_inconnu_est_insere_avant_le_premier_rendu() {
        let db = test_db().await;
        let token = issue(&db, "fresh-sub").await;

        let (status, _headers, body) = serve(app(test_state(db.clone())), get_request(&token)).await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(body["subject"], "fresh-sub");
        assert_eq!(body["email"], serde_json::Value::Null);
        assert_eq!(body["groups"], serde_json::json!([]));

        let rows = user::Entity::find()
            .filter(user::Column::Subject.eq("fresh-sub"))
            .all(&db)
            .await
            .expect("readable");
        assert_eq!(rows.len(), 1, "get-or-create a posé exactement une ligne");
        assert!(
            rows.first().is_some_and(|r| r.email.is_none()),
            "chemin token : email NULL sur la ligne insérée"
        );
    }

    // ——— Scenario 6 : corps minimal exactement subject email et groups en ordre de déclaration ———

    #[tokio::test]
    async fn me_corps_minimal_octet_pour_octet_en_ordre_de_declaration() {
        let db = test_db().await;
        resolve_user(&db, "nobody-sub", None)
            .await
            .expect("resolve succeeds");
        let token = issue(&db, "nobody-sub").await;

        let (status, headers, body) = serve(app(test_state(db)), get_request(&token)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json"),
            "l'en-tête CONTENT_TYPE est application/json"
        );
        assert_eq!(
            body,
            br#"{"subject":"nobody-sub","email":null,"groups":[]}"#.to_vec(),
            "octets du corps exactement en ordre de déclaration, aucune clé id"
        );
    }

    // ——— Scenario 7 : anonyme est 401 MRD-AUTH-001 ———

    #[tokio::test]
    async fn me_anonyme_est_401_mrd_auth_001_sans_aucune_requete() {
        // Base mock non préparée : toute requête SQL rendrait une erreur de mock ; la
        // capture du journal prouve que me_handler n'a jamais été exécuté.
        let db = MockDatabase::new(DbBackend::Sqlite).into_connection();
        let log_db = db.clone();

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me")
            .body(Body::empty())
            .expect("valid request");
        let (status, headers, body) = serve(app(test_state(db)), req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            String::from_utf8(body).expect("utf-8"),
            "MRD-AUTH-001: not authenticated (no session cookie)"
        );
        assert!(
            headers
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|ct| ct.starts_with("text/plain")),
            "rejet d'extraction rendu en text/plain par auth/error.rs"
        );
        assert!(
            log_db.into_transaction_log().is_empty(),
            "aucune requête SQL sur miryad_users : me_handler n'a pas tourné"
        );
    }

    // ——— Scenario 8 : Bearer invalide avec cookie valide est 401 MRD-AUTH-014 sans repli ———

    #[tokio::test]
    async fn me_bearer_invalide_avec_cookie_valide_est_401_sans_repli() {
        let db = test_db().await;
        let state = test_state(db);
        let cookie = session_cookie(&state, "alice-sub", None);
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me")
            .header("Authorization", "Bearer mrd_inconnu")
            .header("Cookie", cookie)
            .body(Body::empty())
            .expect("valid request");

        let (status, _headers, body) = serve(app(state), req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            String::from_utf8(body).expect("utf-8"),
            "MRD-AUTH-014: invalid or unknown API token",
            "le non-repli verrouillé par dual.sdd s'observe ici au corps : aucune session acceptée"
        );
    }

    // ——— Scenario 9 : paramètre de requête ignoré par la route ———

    #[tokio::test]
    async fn me_parametre_de_requete_est_ignore() {
        let db = test_db().await;
        resolve_user(&db, "alice-sub", None)
            .await
            .expect("resolve succeeds");
        resolve_user(&db, "bob-sub", None)
            .await
            .expect("resolve succeeds");
        let token = issue(&db, "alice-sub").await;

        let (_s1, _h1, plain) = serve(app(test_state(db.clone())), get_request(&token)).await;
        let (status, _headers, with_query) = serve(
            app(test_state(db)),
            request("GET", "/api/v1/me?page=99&subject=bob-sub", &token),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            plain, with_query,
            "la query string est ignorée faute d'extracteur"
        );
        assert!(
            String::from_utf8_lossy(&with_query).contains("\"subject\":\"alice-sub\""),
            "la lookup n'a pas dévié vers bob-sub"
        );
    }

    // ——— Scenario 10 : méthode autre que GET est 405 avec Allow ———

    #[tokio::test]
    async fn me_methode_autre_que_get_est_405_avec_allow() {
        let db = MockDatabase::new(DbBackend::Sqlite).into_connection();
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/me")
            .body(Body::empty())
            .expect("valid request");

        let (status, headers, body) = serve(app(test_state(db)), req).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert!(body.is_empty(), "corps vide");
        // Écart spec↔code consigné au rapport : me.sdd attend `Allow: GET` seul ; axum 0.8.9
        // compose `GET,HEAD` sur un `get()` (vérifié au source axum et déjà gelé par
        // `rest/mod.sdd` « GET,HEAD,POST sur la collection »). Verrou sur le réel.
        let allow = headers
            .get("allow")
            .and_then(|v| v.to_str().ok())
            .expect("en-tête Allow présent sur un 405 d'axum")
            .to_string();
        let methods: Vec<&str> = allow.split(',').map(str::trim).collect();
        assert!(methods.contains(&"GET"), "Allow contient GET : {allow}");
        assert!(methods.contains(&"HEAD"), "Allow contient HEAD : {allow}");
        assert_eq!(methods.len(), 2, "GET et HEAD seulement : {allow}");
    }

    // ——— Scenario 11 : HEAD exécute le handler et rend un corps vide ———

    #[tokio::test]
    async fn me_head_execute_le_handler_et_rend_un_corps_vide() {
        let db = test_db().await;
        resolve_user(&db, "alice-sub", None)
            .await
            .expect("resolve succeeds");
        let token = issue(&db, "alice-sub").await;

        let (_s, _h, get_body) = serve(app(test_state(db.clone())), get_request(&token)).await;
        let (status, headers, body) =
            serve(app(test_state(db.clone())), request("HEAD", "/api/v1/me", &token)).await;

        assert_eq!(status, StatusCode::OK);
        assert_json_content(&headers);
        assert_eq!(
            headers
                .get(axum::http::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            Some(get_body.len().to_string()),
            "Content-Length du HEAD = longueur du corps JSON du GET"
        );
        assert!(body.is_empty(), "le corps est évidé au top-level par axum");

        // Effet de bord de l'extraction (dual.sdd, pas de ce fichier) : last_used_at rafraîchi.
        let row = crate::auth::token::Entity::find()
            .filter(crate::auth::token::Column::Subject.eq("alice-sub"))
            .one(&db)
            .await
            .expect("readable")
            .expect("token line");
        assert!(row.last_used_at.is_some(), "l'extraction a tourné comme sur GET");
    }

    // ——— Scenario 12 : table users absente rend le handler 500 MRD-REST-003 ———

    #[tokio::test]
    async fn me_table_users_absente_rend_500_mrd_rest_003() {
        let db = test_db().await;
        let token = issue(&db, "ghost-sub").await;
        db.execute_unprepared("DROP TABLE miryad_users")
            .await
            .expect("table dropped");

        let (status, _headers, body) = serve(app(test_state(db)), get_request(&token)).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let raw = String::from_utf8(body).expect("utf-8");
        // Écart spec↔code consigné au rapport : me.sdd `Raises` attend le préfixe
        // `MRD-REST-003: database error: ` SUIVI du Display brut du DbErr ; l'arbitrage
        // « 500 génériques » de rest/error.rs (2026-09-29, fichier gelé pour cette tâche)
        // ne rend que le code, sans le détail. Le verrou porte le préfixe réellement rendu.
        assert!(
            raw.starts_with("MRD-REST-003: database error"),
            "c'est resolve_user qui échoue dans le handler : {raw}"
        );
        assert!(
            !raw.contains("MRD-AUTH-016"),
            "l'extraction a réussi, aucune erreur d'authentification ici : {raw}"
        );
    }

    // ——— Scenario 13 : ordre des groupes non verrouillé (volet Then seulement) ———

    #[tokio::test]
    async fn me_rend_exactement_les_appartenances_de_l_appelante() {
        let db = test_db().await;
        let eve = resolve_user(&db, "eve-sub", None)
            .await
            .expect("resolve succeeds");
        sync_group_memberships(&db, eve.id, &["editors".to_string(), "viewers".to_string()])
            .await
            .expect("sync succeeds");
        let token = issue(&db, "eve-sub").await;

        let (status, _headers, body) = serve(app(test_state(db)), get_request(&token)).await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        let groups: Vec<&str> = body["groups"]
            .as_array()
            .expect("groups array")
            .iter()
            .map(|g| g.as_str().expect("string"))
            .collect();
        assert_eq!(groups.len(), 2, "exactement deux éléments");
        let mut sorted = groups.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            vec!["editors", "viewers"],
            "l'ensemble des valeurs est contractuel"
        );
    }

    // ——— Tri des groupes par nom (arbitré par Sébastien le 2026-09-29, `Must`) ———

    /// `Must` « Trier le tableau groups par nom croissant » — TEST ROUGE avant implémentation :
    /// les appartenances sont posées en ordre décroissant (`viewers` puis `editors`), le balayage
    /// `SQLite` rend cet ordre d'insertion ; seul un tri explicite rend le tableau croissant.
    #[tokio::test]
    async fn me_groupes_sont_tries_par_nom() {
        let db = test_db().await;
        let eve = resolve_user(&db, "eve-sub", None)
            .await
            .expect("resolve succeeds");
        sync_group_memberships(&db, eve.id, &["viewers".to_string(), "editors".to_string()])
            .await
            .expect("sync succeeds");
        let token = issue(&db, "eve-sub").await;

        let (status, _headers, body) = serve(app(test_state(db)), get_request(&token)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            String::from_utf8(body).expect("utf-8"),
            r#"{"subject":"eve-sub","email":null,"groups":["editors","viewers"]}"#,
            "ordre stable rendu : tri par nom croissant, cas sensible (ordre binaire)"
        );
    }
}
