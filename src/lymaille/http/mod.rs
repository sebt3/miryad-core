//! Surface REST+SSE des sessions interactives (`src/lymaille/http/mod.sdd`).
//!
//! Montage : la couche `lymaille` rend [`conversations_router`], que l'application
//! consommatrice merge elle-même dans son routeur (`app.merge(conversations_router(manager))`,
//! comme `me_router`/`tokens_router` — jamais par `resource_router`). Le préfixe
//! `/api/v1/lymaille` est figé ici, aucun chemin de conversation ne peut être servi ailleurs.
//!
//! Ces routes **ne sont pas des `MiryadResource`** : elles sont exemptées de la règle de parité
//! des surfaces (REST/MCP/OpenAPI) comme `/auth/*` (acté le 2026-11-03, DSD `DEC-25`), et
//! n'apparaissent donc pas dans l'OpenAPI générique. Tout droit passe pourtant par
//! `../identity.rs`, le `AuthPrincipal` du dual-auth (401 anonyme) et `rest::error` — aucune
//! décision d'accès propre ici, et jamais un identifiant d'utilisateur accepté du client : la
//! cible est toujours le `subject` de l'appelante.
//!
//! Cloisonnement : chaque handler relaie le `subject` du principal à `SessionManager`, qui ne
//! voit que les sessions de ce sujet ; la session d'un autre est un `404` indiscernable
//! d'une session absente.

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::extract::Extension;
use axum::extract::FromRef;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::post;

use crate::auth::MiryadAuthState;
use crate::lymaille::interactive::manager::SessionManager;

mod routes;
mod sse;

/// Taille maximale d'un corps JSON sur cette surface (`Must` de la spec) : 64 Ko, posée ici —
/// le texte d'un message est borné plus fin par le handler (`32 Ko`).
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Routeur des conversations interactives, à merger par l'application consommatrice (DSD
/// `FR-12`, `DEC-25`). `manager` est un `Extension` propre au routeur, jamais un état global ;
/// `S` est l'état de l'application, dont le `MiryadAuthState` est extrait pour l'extracteur
/// `AuthPrincipal` et les lectures d'état persisté (même borne que `me_router`).
///
/// Les dix routes servies, sous `/api/v1/lymaille` et nulle part ailleurs :
/// `POST /conversations` · `GET /conversations` · `POST /conversations/{id}/messages` ·
/// `GET /conversations/{id}/events` · `GET /conversations/{id}/history` ·
/// `POST /conversations/{id}/cancel` · `POST /conversations/{id}/resume` ·
/// `DELETE /conversations/{id}` · `POST /toolsets/{name}/activate` ·
/// `DELETE /toolsets/{name}/draft`. Aucun slash final n'est normalisé.
pub fn conversations_router<S>(manager: SessionManager) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>,
{
    Router::new().nest(
        "/api/v1/lymaille",
        Router::new()
            .route(
                "/conversations",
                post(routes::create_conversation).get(routes::list_conversations),
            )
            .route("/conversations/{id}/messages", post(routes::send_message))
            .route("/conversations/{id}/events", get(sse::events))
            .route("/conversations/{id}/history", get(routes::history))
            .route("/conversations/{id}/cancel", post(routes::cancel_conversation))
            .route("/conversations/{id}/resume", post(routes::resume_conversation))
            .route("/conversations/{id}", delete(routes::close_conversation))
            .route("/toolsets/{name}/activate", post(routes::activate_toolset_route))
            .route("/toolsets/{name}/draft", delete(routes::discard_draft_route))
            .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
            .layer(Extension(manager)),
    )
}

#[cfg(test)]
mod tests {
    //! Scénarios d'agrégation de `./mod.sdd` : table exacte des dix routes servies, refus
    //! anonyme `401`, slash final non normalisé, corps borné à 64 Ko.

    use super::conversations_router;
    use crate::lymaille::http::routes::fix;

    use axum::Router;
    use axum::body::Body;
    use axum::http::Method;
    use axum::http::Request;
    use axum::http::StatusCode;
    use axum::http::header::AUTHORIZATION;
    use axum::http::header::CONTENT_TYPE;
    use tower::ServiceExt;

    /// Le routeur monté sur son état d'auth (base de `setup`, jamais consultée par ces sondes).
    async fn mounted() -> (Router<()>, String) {
        let (db, _hub, manager) = fix::setup().await;
        let alice = fix::token_for(&db, "alice").await;
        let app = conversations_router(manager).with_state(fix::test_state(db));
        (app, alice)
    }

    async fn probe(
        app: Router<()>,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Body,
    ) -> StatusCode {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        let request = builder.body(body).expect("requete de sonde valide");
        app.oneshot(request).await.expect("le routeur repond").status()
    }

    /// Scenario « les routes exactes existent, rien d'autre » : les dix routes (méthode
    /// comprise) appellent l'extracteur `AuthPrincipal` — une sonde anonyme y rend `401` — et
    /// aucune forme hors table n'atteint un handler (`404`/`405`, jamais `401`).
    #[tokio::test(flavor = "multi_thread")]
    async fn les_routes_exactes_existent_rien_dautre() {
        let (app, _alice) = mounted().await;
        let dix: [(&str, &str); 10] = [
            ("POST", "/api/v1/lymaille/conversations"),
            ("GET", "/api/v1/lymaille/conversations"),
            ("POST", "/api/v1/lymaille/conversations/x1/messages"),
            ("GET", "/api/v1/lymaille/conversations/x1/events"),
            ("GET", "/api/v1/lymaille/conversations/x1/history"),
            ("POST", "/api/v1/lymaille/conversations/x1/cancel"),
            ("POST", "/api/v1/lymaille/conversations/x1/resume"),
            ("DELETE", "/api/v1/lymaille/conversations/x1"),
            ("POST", "/api/v1/lymaille/toolsets/ts/activate"),
            ("DELETE", "/api/v1/lymaille/toolsets/ts/draft"),
        ];
        for (method, uri) in dix {
            let status = probe(
                app.clone(),
                Method::from_bytes(method.as_bytes()).expect("méthode"),
                uri,
                None,
                Body::empty(),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} {uri} : route servie, principal exige"
            );
        }
        let hors_table: [(&str, &str); 6] = [
            ("PUT", "/api/v1/lymaille/conversations"),
            ("GET", "/api/v1/lymaille/conversations/x1/messages"),
            ("POST", "/api/v1/lymaille/conversations/x1/history"),
            ("DELETE", "/api/v1/lymaille/conversations/x1/events"),
            ("POST", "/api/v1/lymaille/toolsets/ts/draft"),
            ("GET", "/api/v1/lymaille/conversations/x1"),
        ];
        for (method, uri) in hors_table {
            let status = probe(
                app.clone(),
                Method::from_bytes(method.as_bytes()).expect("méthode"),
                uri,
                None,
                Body::empty(),
            )
            .await;
            assert_ne!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} {uri} : aucune route de la table ne correspond"
            );
        }
        let hors_prefixe = probe(app, Method::POST, "/conversations", None, Body::empty()).await;
        assert_eq!(
            hors_prefixe,
            StatusCode::NOT_FOUND,
            "rien n'est servi hors du préfixe figé"
        );
    }

    /// Scenario « un appel anonyme est refusé » : `POST /api/v1/lymaille/conversations` sans
    /// aucun matériau d'identité rend `401`.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_appel_anonyme_est_refuse() {
        let (app, _alice) = mounted().await;
        let status = probe(
            app,
            Method::POST,
            "/api/v1/lymaille/conversations",
            None,
            Body::empty(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "l'anonyme est refusé à la porte"
        );
    }

    /// Scenario « slash final non normalisé » : `GET /api/v1/lymaille/conversations/` n'est
    /// aucune route — `404`, ni une route de plus, ni un `401` d'extrapolation.
    #[tokio::test(flavor = "multi_thread")]
    async fn slash_final_non_normalise() {
        let (app, _alice) = mounted().await;
        let status = probe(
            app,
            Method::GET,
            "/api/v1/lymaille/conversations/",
            None,
            Body::empty(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "le slash final n'est pas normalisé"
        );
    }

    /// `Must` : corps borné à 64 Ko par `DefaultBodyLimit` posé ici — 70 Ko est refusé sans
    /// être jamais passé au `serde`. Le `RestError` n'a pas de variante `413` et la spec
    /// n'arbite pas ce code : le refus tombe dans la traduction `json_body`, le même `422` à
    /// texte fixe que tout corps invalide (écart signalé au rapport).
    #[tokio::test(flavor = "multi_thread")]
    async fn le_corps_est_borne_a_64_ko() {
        let (app, alice) = mounted().await;
        let padding = "p".repeat(70 * 1024);
        let body = Body::from(format!(r#"{{"context":"c1","padding":"{padding}"}}"#));
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/lymaille/conversations")
            .header(AUTHORIZATION, format!("Bearer {alice}"))
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .expect("requete valide");
        let status = app.oneshot(request).await.expect("le routeur repond").status();
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "le corps de 70 Ko est refusé"
        );
    }
}
