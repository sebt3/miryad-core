use std::sync::Arc;

use cookie::Key;
use sea_orm::DatabaseConnection;

use crate::auth::oidc::OidcClientTrait;

/// État minimal requis par le sous-routeur `auth_router` et les extracteurs `AuthUser`/
/// `AuthPrincipal`. L'app consommatrice compose son propre `AppState` autour (pattern `FromRef`
/// d'axum) — miryad-core n'impose aucune structure d'état concrète.
#[derive(Clone)]
pub struct MiryadAuthState {
    pub oidc_client: Arc<dyn OidcClientTrait>,
    pub cookie_key: Key,
    pub post_login_redirect: String,
    pub post_logout_redirect: String,
    /// Utilisé par tout le flow auth : validation des tokens API, `resolve_user` et
    /// `sync_group_memberships` au callback — y compris sur le flow cookie seul.
    pub db: DatabaseConnection,
    /// Positionne l'attribut `Secure` du cookie de session `miryad_session` (cf. `auth::cookie`)
    /// — `true` en HTTPS, `false` en HTTP local de développement (arbitré 2026-09-27).
    pub secure_cookies: bool,
    /// Poivre HMAC des empreintes de tokens API (cf. `auth::token`) — consommé par
    /// `issue_token`/`validate_token`/`ensure_token`. Le type ne dérive pas `Debug`, le poivre
    /// ne fuit donc pas par un `{:?}` (arbitré 2026-09-27).
    pub token_pepper: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::auth_router;
    use crate::auth::oidc::MockOidcClient;
    use axum::body::Body;
    use axum::extract::FromRef;
    use axum::http::{Request, StatusCode};
    use sea_orm::ConnectionTrait;
    use tower::ServiceExt;

    /// Fixture : état bâti sur `MockOidcClient` (jamais de discovery, aucune I/O) et une base
    /// `MockDatabase` `SQLite` sauf là où le `Scenario` exige une vraie base en mémoire.
    fn fixture(post_login: &str, secure_cookies: bool, token_pepper: &str) -> MiryadAuthState {
        MiryadAuthState {
            oidc_client: Arc::new(MockOidcClient),
            cookie_key: Key::from(&[0u8; 64]),
            post_login_redirect: post_login.to_string(),
            post_logout_redirect: "/logout-done".to_string(),
            db: sea_orm::MockDatabase::new(sea_orm::DatabaseBackend::Sqlite).into_connection(),
            secure_cookies,
            token_pepper: token_pepper.to_string(),
        }
    }

    /// `Scenario` : « provider OIDC partagé entre clones » — `Arc::ptr_eq` vrai, aucune
    /// discovery ni construction de client HTTP (le mock n'en connaît aucune).
    #[test]
    fn clone_shares_oidc_provider() {
        let state = fixture("/", false, "");
        let cloned = state.clone();
        assert!(
            Arc::ptr_eq(&state.oidc_client, &cloned.oidc_client),
            "le clone doit partager la même instance de provider"
        );
    }

    /// `Scenario` : « redirections proprement dupliquées entre clones ».
    #[test]
    fn clone_deep_copies_redirects() {
        let state = fixture("/login-done", false, "");
        let mut cloned = state.clone();
        cloned.post_login_redirect = "/autre".to_string();

        assert_eq!(state.post_login_redirect, "/login-done");
        assert!(Arc::ptr_eq(&state.oidc_client, &cloned.oidc_client));
    }

    /// `Scenario` : « clones de db adressent le même pool » — insertion via la `db` du clone,
    /// relure par une requête `SeaORM` sur la `db` d'origine (`sqlite` en mémoire, comme dual.rs).
    #[tokio::test]
    async fn db_clone_shares_pool() {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        let state = fixture_with_db("/", false, "", db);
        let cloned = state.clone();

        cloned
            .db
            .execute_unprepared("CREATE TABLE probe (v TEXT NOT NULL)")
            .await
            .expect("table creates via clone");
        cloned
            .db
            .execute_unprepared("INSERT INTO probe VALUES ('from-clone')")
            .await
            .expect("row inserts via clone");

        let stmt = sea_orm::Statement::from_sql_and_values(
            state.db.get_database_backend(),
            "SELECT v FROM probe",
            [],
        );
        let row = state
            .db
            .query_one_raw(stmt)
            .await
            .expect("select runs on original")
            .expect("row visible on original");
        let value: String = row.try_get("", "v").expect("column v reads");
        assert_eq!(value, "from-clone", "les poignées partagent le pool");
    }

    /// `Scenario` : « l'état est son propre état de routeur » — blanket `FromRef<T> for T`
    /// d'axum-core, aucune impl de `FromRef` ne vit dans ce fichier.
    #[test]
    fn identity_from_ref_keeps_arc() {
        let state = fixture("/", false, "");
        let same = <MiryadAuthState as FromRef<MiryadAuthState>>::from_ref(&state);

        assert!(Arc::ptr_eq(&state.oidc_client, &same.oidc_client));
        assert_eq!(state.post_login_redirect, same.post_login_redirect);
        assert_eq!(state.post_logout_redirect, same.post_logout_redirect);
        assert_eq!(state.secure_cookies, same.secure_cookies);
        assert_eq!(state.token_pepper, same.token_pepper);
    }

    /// `Scenario` : « état transportable entre threads » — bornes des routeurs tenues à la
    /// compilation ; le test passe par sa seule existence.
    #[test]
    fn state_is_send_sync_static() {
        fn assert_send_sync_static<T: Clone + Send + Sync + 'static>() {}
        assert_send_sync_static::<MiryadAuthState>();
    }

    /// `Scenario` : « rotation de clé en place par le porteur » — la mutation n'existe que par
    /// la liaison mutable de l'appelant, aucun setter ici.
    #[test]
    fn pub_field_swapped_in_place() {
        let k1_key = Key::from(&[0u8; 64]);
        let mut state = fixture("/", false, "");
        assert_eq!(state.cookie_key, k1_key);

        state.cookie_key = Key::from(&[7u8; 64]);
        assert_ne!(
            state.cookie_key, k1_key,
            "la clé K2 substituée doit différer de K1"
        );
    }

    /// `Scenario` : « provider jamais validé accepté » — construction par littéral avec un
    /// provider sans discovery, aucune erreur ni panic, et le provider reste appelable.
    #[test]
    fn unvalidated_provider_accepted() {
        let state = fixture("/", false, "");
        let (url, _csrf, _nonce) = state.oidc_client.authorization_url();
        assert_eq!(url.as_str(), "https://issuer.example.com/authorize");
    }

    /// `Scenario` : « composition dans l'état concret de l'application » — `AppTestState` avec
    /// un `FromRef` écrit à la main, `auth_router` instanciée dessus, `GET /auth/logout` via
    /// `oneshot` → `302` + `Location` du `post_logout_redirect`.
    #[tokio::test]
    async fn composes_into_app_state_via_from_ref() {
        #[derive(Clone)]
        struct AppTestState {
            auth: MiryadAuthState,
        }
        impl FromRef<AppTestState> for MiryadAuthState {
            fn from_ref(input: &AppTestState) -> Self {
                input.auth.clone()
            }
        }

        let app = auth_router::<AppTestState>().with_state(AppTestState {
            auth: fixture("/", false, ""),
        });
        let req = Request::builder()
            .uri("/auth/logout")
            .body(Body::empty())
            .expect("valid request");
        let resp = app.oneshot(req).await.expect("router does not fail");

        assert_eq!(resp.status(), StatusCode::FOUND);
        assert_eq!(
            resp.headers().get("Location").expect("Location header"),
            "/logout-done",
            "Location rend le post_logout_redirect de l'état composé"
        );
    }

    /// `Scenario` : « type présent sans features par défaut » — référence par le chemin public
    /// `crate::auth::MiryadAuthState`, module non gateé ; exercé par la batterie sur
    /// `--no-default-features` comme sur toutes les autres combinaisons.
    #[test]
    fn present_without_default_features() {
        fn takes_public_path(state: &crate::auth::MiryadAuthState) -> bool {
            !state.post_login_redirect.is_empty()
        }
        assert!(takes_public_path(&fixture("/", false, "")));
    }

    /// `Scenario` : « `secure_cookies` et `token_pepper` se clonent indépendamment » (arbitré
    /// 2026-09-27) — copies propres, comme les redirections.
    #[test]
    fn secure_cookies_and_token_pepper_clone_independently() {
        let state = fixture("/", true, "pepper-1");
        let mut cloned = state.clone();
        cloned.secure_cookies = false;
        cloned.token_pepper = "pepper-2".to_string();

        assert!(state.secure_cookies);
        assert_eq!(state.token_pepper, "pepper-1");
    }

    /// `Scenario` : « tout littéral de construction de la crate porte les sept champs » — le
    /// littéral des sept champs compile ici ; la preuve crate-wide est la compilation de tous
    /// les sites de construction (`mod.rs`, `middleware.rs`, `dual.rs`, `rest`, `graphql`).
    #[test]
    fn secure_cookies_and_token_pepper_present_in_every_literal_construction() {
        let state = MiryadAuthState {
            oidc_client: Arc::new(MockOidcClient),
            cookie_key: Key::from(&[0u8; 64]),
            post_login_redirect: "/a".to_string(),
            post_logout_redirect: "/b".to_string(),
            db: sea_orm::MockDatabase::new(sea_orm::DatabaseBackend::Sqlite).into_connection(),
            secure_cookies: true,
            token_pepper: "x".to_string(),
        };
        assert!(state.secure_cookies);
        assert_eq!(state.token_pepper, "x");
    }

    /// Variante de fixture pour le `Scenario` du pool partagé (vraie connexion `SQLite`).
    fn fixture_with_db(
        post_login: &str,
        secure_cookies: bool,
        token_pepper: &str,
        db: DatabaseConnection,
    ) -> MiryadAuthState {
        MiryadAuthState {
            oidc_client: Arc::new(MockOidcClient),
            cookie_key: Key::from(&[0u8; 64]),
            post_login_redirect: post_login.to_string(),
            post_logout_redirect: "/logout-done".to_string(),
            db,
            secure_cookies,
            token_pepper: token_pepper.to_string(),
        }
    }
}
