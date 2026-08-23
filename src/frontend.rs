//! Service statique du frontend compilé — feature 8. Générique : ne connaît rien du contenu réel
//! des assets, sert un répertoire externe (pas d'embarquement dans le binaire, cf.
//! `docs/architecture.md`).

use std::path::PathBuf;

use axum::Router;
use tower_http::services::{ServeDir, ServeFile};

/// Sert `assets_dir` en statique, avec fallback SPA : toute route non trouvée (donc toute route
/// gérée côté client par Vue Router) renvoie `assets_dir/index.html` plutôt qu'un 404.
pub fn static_frontend_router<S>(assets_dir: impl Into<PathBuf>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let assets_dir = assets_dir.into();
    let index = assets_dir.join("index.html");
    let serve_dir = ServeDir::new(&assets_dir).fallback(ServeFile::new(index));

    Router::new().fallback_service(serve_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::path::PathBuf;
    use tower::ServiceExt;

    fn fixture_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("miryad-frontend-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create tmp dir");
        std::fs::write(dir.join("index.html"), "<html>spa</html>").expect("write index.html");
        std::fs::write(dir.join("app.js"), "console.log('hi')").expect("write app.js");
        dir
    }

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

    #[tokio::test]
    async fn falls_back_to_index_html_for_unknown_routes() {
        let dir = fixture_dir("spa-fallback");
        let app: Router = static_frontend_router(dir.clone());

        let req = Request::builder()
            .uri("/recipes/42")
            .body(Body::empty())
            .expect("valid request");
        let resp = app.oneshot(req).await.expect("router does not fail");

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(bytes, "<html>spa</html>".as_bytes());

        std::fs::remove_dir_all(&dir).ok();
    }
}
