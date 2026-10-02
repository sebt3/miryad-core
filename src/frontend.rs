//! Service statique du frontend compilé — feature 8. Générique : ne connaît rien du contenu réel
//! des assets, sert un répertoire externe (pas d'embarquement dans le binaire, cf.
//! `docs/architecture.md`).

use std::path::PathBuf;

use axum::Router;
use axum::extract::Request;
use axum::http::header::{self, HeaderValue};
use axum::middleware::{self, Next};
use axum::response::Response;
use tower_http::services::{ServeDir, ServeFile};

// Couche prescrite par `frontend.sdd` Tasks (arbitré 2026-09-29) : « couche axum ou
// @tower_http (SetResponseHeader) sur le routeur rendu ». Variante axum, seule à ne rien
// ajouter au graphe de `Cargo.toml`. Le `Must` de la même spec ne la réserve qu'au
// `text/html` ; les assets non HTML restent sans `cache-control`.
async fn html_no_cache(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let is_html = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/html"));
    if is_html {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    response
}

/// Sert `assets_dir` en statique, avec repli SPA : tout `GET` ou `HEAD` sans fichier valide
/// trouvable sous `assets_dir` rend `assets_dir/index.html` en `200` (coquille — le routage
/// côté client, Vue Router, ne se heurte jamais à un `404` prématuré). Toute méthode autre que
/// `GET` ou `HEAD` reçoit un `405` avec `allow` ; quand `index.html` lui-même manque, la réponse
/// est un `404` vide. Toute réponse `text/html` porte `cache-control: no-cache`.
pub fn static_frontend_router<S>(assets_dir: impl Into<PathBuf>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let assets_dir = assets_dir.into();
    let index = assets_dir.join("index.html");
    let serve_dir = ServeDir::new(&assets_dir).fallback(ServeFile::new(index));

    Router::new()
        .fallback_service(serve_dir)
        .layer(middleware::from_fn(html_no_cache))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode};
    use std::path::PathBuf;
    use tower::ServiceExt;

    const INDEX: &str = "<html>spa</html>";
    const APP_JS: &str = "console.log('hi')";
    const SUB_INDEX: &str = "<html>sub</html>";
    const SENTINELLE: &str = "SENTINELLE-PARENTE-INATTEIGNABLE";

    fn fixture_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("miryad-frontend-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create tmp dir");
        std::fs::write(dir.join("index.html"), INDEX).expect("write index.html");
        std::fs::write(dir.join("app.js"), APP_JS).expect("write app.js");
        dir
    }

    fn fixture_dir_with_sub(name: &str) -> PathBuf {
        let dir = fixture_dir(name);
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).expect("create tmp sub dir");
        std::fs::write(sub.join("index.html"), SUB_INDEX).expect("write sub index.html");
        dir
    }

    fn fixture_dir_without_index(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("miryad-frontend-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create tmp dir");
        std::fs::write(dir.join("app.js"), APP_JS).expect("write app.js");
        dir
    }

    // Le répertoire servi est `root/assets` ; `root` porte la sentinelle `outside.html`.
    fn fixture_dir_with_outside(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("miryad-frontend-test-{}-{name}", std::process::id()));
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).expect("create tmp assets dir");
        std::fs::write(assets.join("index.html"), INDEX).expect("write index.html");
        std::fs::write(assets.join("app.js"), APP_JS).expect("write app.js");
        std::fs::write(root.join("outside.html"), SENTINELLE).expect("write sentinel outside assets");
        assets
    }

    async fn ping_handler() -> &'static str {
        "pong"
    }

    async fn consumer_fallback() -> &'static str {
        "fallback consommateur"
    }

    async fn drive(app: Router, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let resp = app.oneshot(req).await.expect("router does not fail");
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, headers, body)
    }

    fn header<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
        headers.get(name).and_then(|v| v.to_str().ok())
    }

    // Scenario « fichier statique servi en GET » (frontend.sdd).
    #[tokio::test]
    async fn serves_an_existing_asset() {
        let dir = fixture_dir("serves-existing");
        let app: Router = static_frontend_router(dir.clone());

        let req = Request::builder()
            .uri("/app.js")
            .body(Body::empty())
            .expect("valid request");
        let resp = app.oneshot(req).await.expect("router does not fail");

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(bytes, "console.log('hi')".as_bytes());

        std::fs::remove_dir_all(&dir).ok();
    }

    // Scenario « coquille SPA rendue sur route inconnue » (frontend.sdd, arbitré 2026-09-29).
    #[tokio::test]
    async fn falls_back_to_index_html_for_unknown_routes() {
        let dir = fixture_dir("spa-fallback");
        let app: Router = static_frontend_router(dir.clone());

        let req = Request::builder()
            .uri("/recipes/42")
            .body(Body::empty())
            .expect("valid request");
        let (status, headers, body) = drive(app, req).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, INDEX.as_bytes());
        assert_eq!(header(&headers, "content-type"), Some("text/html"));
        assert_eq!(header(&headers, "cache-control"), Some("no-cache"));

        std::fs::remove_dir_all(&dir).ok();
    }

    // Scenario « coquille SPA servie directement au chemin racine » (frontend.sdd) + `Must`
    // arbitré 2026-09-29 : la racine porte aussi `cache-control: no-cache`.
    #[tokio::test]
    async fn serves_index_at_root() {
        let dir = fixture_dir("index-at-root");
        let app: Router = static_frontend_router(dir.clone());

        let req = Request::builder()
            .uri("/")
            .body(Body::empty())
            .expect("valid request");
        let (status, headers, body) = drive(app, req).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, INDEX.as_bytes());
        assert_eq!(header(&headers, "content-type"), Some("text/html"));
        assert_eq!(header(&headers, "cache-control"), Some("no-cache"));

        std::fs::remove_dir_all(&dir).ok();
    }

    // Scenario « en-têtes du fichier servi portent sa matière » (frontend.sdd) + verrou du
    // `Must` arbitré 2026-09-29 : aucun `cache-control` sur un asset non HTML, pas d'`etag`.
    #[tokio::test]
    async fn serves_asset_with_metadata_headers() {
        let dir = fixture_dir("metadata-headers");
        let app: Router = static_frontend_router(dir.clone());

        let req = Request::builder()
            .uri("/app.js")
            .body(Body::empty())
            .expect("valid request");
        let (status, headers, body) = drive(app, req).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, APP_JS.as_bytes());
        assert_eq!(header(&headers, "content-type"), Some("text/javascript"));
        let expected_len = APP_JS.len().to_string();
        assert_eq!(
            header(&headers, "content-length"),
            Some(expected_len.as_str()),
            "content-length doit égaler la taille du fichier"
        );
        assert_eq!(header(&headers, "accept-ranges"), Some("bytes"));
        // `Must` : `last-modified` dès que le fs expose le mtime.
        assert!(headers.get("last-modified").is_some(), "last-modified attendu");
        // But du Scenario + `Must` 2026-09-29 : rien de cache-related sur le non-HTML.
        assert!(
            headers.get("cache-control").is_none(),
            "pas de cache-control sur app.js"
        );
        assert!(headers.get("etag").is_none(), "pas d'etag sur app.js");

        std::fs::remove_dir_all(&dir).ok();
    }

    // Scenario « requête HEAD rend les en-têtes sans corps » (frontend.sdd).
    #[tokio::test]
    async fn head_asset_responds_without_body() {
        let dir = fixture_dir("head-no-body");
        let app: Router = static_frontend_router(dir.clone());

        let req = Request::builder()
            .method("HEAD")
            .uri("/app.js")
            .body(Body::empty())
            .expect("valid request");
        let (status, headers, body) = drive(app, req).await;

        assert_eq!(status, StatusCode::OK);
        let expected_len = APP_JS.len().to_string();
        assert_eq!(header(&headers, "content-length"), Some(expected_len.as_str()));
        assert!(body.is_empty(), "HEAD ne rend aucun corps");

        std::fs::remove_dir_all(&dir).ok();
    }

    // Scenario « méthode autre que GET ou HEAD refusée en 405 » (frontend.sdd).
    #[tokio::test]
    async fn non_get_method_returns_405() {
        let dir = fixture_dir("method-405");
        let app: Router = static_frontend_router(dir.clone());

        let req = Request::builder()
            .method("POST")
            .uri("/recipes/42")
            .body(Body::empty())
            .expect("valid request");
        let (status, headers, body) = drive(app, req).await;

        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(header(&headers, "allow"), Some("GET,HEAD"));
        assert!(
            body.is_empty(),
            "corps vide : aucun index.html n'est rendu pour cette requête"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // Scenario « répertoire sans slash final redirigé en 307 » (frontend.sdd).
    #[tokio::test]
    async fn directory_redirects_with_trailing_slash() {
        let dir = fixture_dir_with_sub("dir-307");
        let app: Router = static_frontend_router(dir.clone());

        let req = Request::builder()
            .uri("/sub")
            .body(Body::empty())
            .expect("valid request");
        let (status, headers, _body) = drive(app.clone(), req).await;
        assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(header(&headers, "location"), Some("/sub/"));

        let req = Request::builder()
            .uri("/sub?page=2")
            .body(Body::empty())
            .expect("valid request");
        let (status, headers, _body) = drive(app, req).await;
        assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            header(&headers, "location"),
            Some("/sub/?page=2"),
            "query string conservée"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // Scenario « répertoire avec slash sert son propre index » (frontend.sdd) + verrou du
    // `Must` arbitré 2026-09-29 : tout `index.html` de sous-répertoire porte le `cache-control`.
    #[tokio::test]
    async fn subdirectory_serves_its_own_index() {
        let dir = fixture_dir_with_sub("sub-index");
        let app: Router = static_frontend_router(dir.clone());

        let req = Request::builder()
            .uri("/sub/")
            .body(Body::empty())
            .expect("valid request");
        let (status, headers, body) = drive(app, req).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, SUB_INDEX.as_bytes());
        assert_ne!(body, INDEX.as_bytes(), "pas l'index.html racine");
        assert_eq!(header(&headers, "content-type"), Some("text/html"));
        assert_eq!(header(&headers, "cache-control"), Some("no-cache"));

        std::fs::remove_dir_all(&dir).ok();
    }

    // Scenario « fichier demandé comme dossier replie sur la coquille » (frontend.sdd).
    #[tokio::test]
    async fn file_as_directory_falls_back() {
        let dir = fixture_dir("file-as-dir");
        let app: Router = static_frontend_router(dir.clone());

        let req = Request::builder()
            .uri("/app.js/nested")
            .body(Body::empty())
            .expect("valid request");
        let (status, _headers, body) = drive(app, req).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, INDEX.as_bytes());

        std::fs::remove_dir_all(&dir).ok();
    }

    // Scenario « traversée parente encodée ne sort jamais du dossier » (frontend.sdd).
    #[tokio::test]
    async fn percent_traversal_never_escapes_dir() {
        let assets = fixture_dir_with_outside("traversal");
        let app: Router = static_frontend_router(assets.clone());

        let req = Request::builder()
            .uri("/%2e%2e/outside.html")
            .body(Body::empty())
            .expect("valid request");
        let (_status, _headers, body) = drive(app.clone(), req).await;
        assert_eq!(body, INDEX.as_bytes());
        assert!(
            !body.windows(SENTINELLE.len()).any(|w| w == SENTINELLE.as_bytes()),
            "la sentinelle parente ne doit jamais paraître"
        );

        let req = Request::builder()
            .uri("/../outside.html")
            .body(Body::empty())
            .expect("valid request");
        let (_status, _headers, body) = drive(app, req).await;
        assert_eq!(
            body,
            INDEX.as_bytes(),
            "la forme littérale `..` produit le même résultat"
        );

        std::fs::remove_dir_all(assets.parent().expect("assets vit sous un root de fixture")).ok();
    }

    // Scenario « octet nul dans le chemin replie sur la coquille » (frontend.sdd).
    #[tokio::test]
    async fn nul_byte_falls_back_to_shell() {
        let dir = fixture_dir("nul-byte");
        let app: Router = static_frontend_router(dir.clone());

        let req = Request::builder()
            .uri("/%00")
            .body(Body::empty())
            .expect("valid request");
        let (status, _headers, body) = drive(app, req).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, INDEX.as_bytes());

        std::fs::remove_dir_all(&dir).ok();
    }

    // Scenario « index.html absent répond 404 vide » (frontend.sdd).
    #[tokio::test]
    async fn missing_index_returns_empty_404() {
        let dir = fixture_dir_without_index("no-index");
        let app: Router = static_frontend_router(dir.clone());

        let req = Request::builder()
            .uri("/nimporte-quoi")
            .body(Body::empty())
            .expect("valid request");
        let (status, headers, body) = drive(app, req).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.is_empty(), "le corps est vide");
        assert!(
            headers.get("content-type").is_none(),
            "aucun en-tête content-type"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // Scenario « conditionnelle fraîche répond 304 » (frontend.sdd).
    #[tokio::test]
    async fn if_modified_since_returns_304() {
        let dir = fixture_dir("ims-304");
        let app: Router = static_frontend_router(dir.clone());

        let req = Request::builder()
            .uri("/app.js")
            .body(Body::empty())
            .expect("valid request");
        let (_status, headers, _body) = drive(app.clone(), req).await;
        let last_modified = header(&headers, "last-modified")
            .expect("la première requête rend last-modified")
            .to_string();

        let req = Request::builder()
            .uri("/app.js")
            .header("if-modified-since", last_modified.as_str())
            .body(Body::empty())
            .expect("valid request");
        let (status, _headers, body) = drive(app, req).await;

        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert!(body.is_empty(), "304 au corps vide");

        std::fs::remove_dir_all(&dir).ok();
    }

    // Scenario « précondition sur date ancienne échoue en 412 » (frontend.sdd).
    #[tokio::test]
    async fn if_unmodified_since_returns_412() {
        let dir = fixture_dir("ius-412");
        let app: Router = static_frontend_router(dir.clone());

        let req = Request::builder()
            .uri("/app.js")
            .header("if-unmodified-since", "Fri, 09 Aug 1996 14:21:40 GMT")
            .body(Body::empty())
            .expect("valid request");
        let (status, _headers, body) = drive(app, req).await;

        assert_eq!(status, StatusCode::PRECONDITION_FAILED);
        assert!(body.is_empty(), "412 au corps vide");

        std::fs::remove_dir_all(&dir).ok();
    }

    // Scenario « requête range satisfiable rend 206 et la tranche » (frontend.sdd).
    #[tokio::test]
    async fn satisfiable_range_returns_206() {
        let dir = fixture_dir("range-206");
        let app: Router = static_frontend_router(dir.clone());

        let req = Request::builder()
            .uri("/app.js")
            .header("range", "bytes=0-2")
            .body(Body::empty())
            .expect("valid request");
        let (status, headers, body) = drive(app, req).await;

        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            body,
            &APP_JS.as_bytes()[..3],
            "les trois premiers octets du fichier"
        );
        let content_range = header(&headers, "content-range")
            .expect("content-range requis")
            .to_string();
        let total = APP_JS.len().to_string();
        assert!(
            content_range.starts_with("bytes 0-2/"),
            "content-range : {content_range}"
        );
        assert!(
            content_range.ends_with(&total),
            "content-range doit finir par la taille totale : {content_range}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // Scenario « range insatisfiable rend 416 » (frontend.sdd).
    #[tokio::test]
    async fn unsatisfiable_range_returns_416() {
        let dir = fixture_dir("range-416");
        let app: Router = static_frontend_router(dir.clone());

        let req = Request::builder()
            .uri("/app.js")
            .header("range", "bytes=900-")
            .body(Body::empty())
            .expect("valid request");
        let (status, headers, body) = drive(app, req).await;

        assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
        assert!(body.is_empty(), "416 au corps vide");
        let expected = format!("bytes */{}", APP_JS.len());
        assert_eq!(header(&headers, "content-range"), Some(expected.as_str()));

        std::fs::remove_dir_all(&dir).ok();
    }

    // Scenario « deux routeurs à fallback personnalisé refusent la fusion » (frontend.sdd).
    #[test]
    fn merging_two_custom_fallbacks_panics() {
        let dir = fixture_dir("merge-panic");
        let frontend: Router<()> = static_frontend_router(dir.clone());
        let other: Router<()> = Router::new().fallback(consumer_fallback);

        let merged = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || frontend.merge(other)));

        assert!(
            merged.is_err(),
            "axum 0.8.9 refuse deux fallbacks personnalisés dans un même Router (Must de frontend.sdd)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // Scenario « routes déclarées passent avant la coquille SPA » (frontend.sdd).
    #[tokio::test]
    async fn merged_routes_win_over_shell() {
        let dir = fixture_dir("merged-routes");
        let app: Router = static_frontend_router(dir.clone())
            .merge(Router::new().route("/api/ping", axum::routing::get(ping_handler)));

        let req = Request::builder()
            .uri("/api/ping")
            .body(Body::empty())
            .expect("valid request");
        let (status, _headers, body) = drive(app.clone(), req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"pong", "la réponse vient du handler, pas de index.html");

        let req = Request::builder()
            .uri("/nimporte-quoi")
            .body(Body::empty())
            .expect("valid request");
        let (status, _headers, body) = drive(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, INDEX.as_bytes());

        std::fs::remove_dir_all(&dir).ok();
    }

    // Scenario « module absent de la surface sans static-frontend » (frontend.sdd).
    // Ce fichier ne compile que sous le gating `static-frontend` de lib.rs : résoudre le
    // chemin complet ci-dessous prouve le `Then` « la compilation passe ». L'absence du
    // module dans les cinq constructions `--no-default-features` (et `tower-http` sans `fs`)
    // est verrouillée par la batterie de tooling.sdd, pas exécutable depuis ici.
    #[test]
    fn frontend_surface_follows_feature_gating() {
        fn resolves_full_path(dir: PathBuf) -> Router<()> {
            crate::frontend::static_frontend_router::<()>(dir)
        }
        let witness: fn(PathBuf) -> Router<()> = resolves_full_path;
        // Aucun accès au système de fichiers à la construction (`Must`) : construire sur un
        // chemin inexistant reste inerte ; le routeur ne porte aucune route déclarée, seul le
        // fallback SPA (`Returns`/`Must`).
        let router = witness(PathBuf::from("assets-inexistants-sous-repertoire"));
        assert!(
            !router.has_routes(),
            "aucune route déclarée : seul fallback_service compose"
        );
    }
}
