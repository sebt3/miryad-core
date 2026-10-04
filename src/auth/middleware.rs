use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;

use crate::auth::cookie::extract_session;
use crate::auth::error::AuthError;
use crate::auth::state::MiryadAuthState;

/// Identité de la requête courante, extraite du cookie de session — pas d'évaluation RBAC ici,
/// juste "qui fait la requête" (cf. feature 3 pour le "a le droit de quoi").
///
/// `Debug` est dérivé (arbitré 2026-09-27) : les trois champs sont des données publiques une
/// fois authentifiées, aucun secret à rédiger — `id_token` est déjà lisible par son porteur.
/// `Clone` et `PartialEq` volontairement absents ; type adjacent du flow navigateur, la
/// confusion avec `AuthPrincipal` (dual-auth) est la frontière actée avec `dual.rs`.
#[derive(Debug)]
pub struct AuthUser {
    /// Claim `sub` de l'`id_token` validé, recopié verbatim de l'`OidcIdentity` rendu par
    /// `extract_session` — aucune validation de format ici.
    pub subject: String,
    /// Claim `email` du login, `Some` verbatim ou `None` si le fournisseur ne l'a pas
    /// transmise — métadonnée, jamais une credential.
    pub email: Option<String>,
    /// `id_token` OIDC brut du payload déchiffré, jamais re-sérialisé ni revérifié (la seule
    /// borne de fraîcheur est le contrôle d'`exp` de `cookie.rs`).
    pub id_token: String,
}

impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
    MiryadAuthState: FromRef<S>,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth_state = MiryadAuthState::from_ref(state);

        let cookie_header = parts
            .headers
            .get("Cookie")
            .and_then(|v| v.to_str().ok())
            .map(std::string::ToString::to_string);

        let identity = extract_session(cookie_header.as_deref(), &auth_state.cookie_key)
            .inspect_err(|e| tracing::debug!("auth rejected: {}", e))?;

        tracing::debug!(subject = %identity.subject, "auth ok");
        Ok(Self {
            subject: identity.subject,
            email: identity.email,
            id_token: identity.id_token,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::cookie::build_set_cookie;
    use crate::auth::oidc::{MockOidcClient, OidcIdentity};
    use ::cookie::Key;
    use axum::{
        Router,
        body::Body,
        http::{HeaderValue, Request, StatusCode},
        routing::get,
    };
    use tower::ServiceExt;

    // ——— Fixtures ———

    /// Corps exacts rendus par ./error.rs pour les deux seules variantes atteignables ici.
    const BODY_001: &str = "MRD-AUTH-001: not authenticated (no session cookie)";
    const BODY_002: &str = "MRD-AUTH-002: invalid or expired session";
    /// Contrat `text/plain` du rendu d'erreur (./error.sdd), vérifié sur les rejets.
    const TEXT_PLAIN: &str = "text/plain; charset=utf-8";

    fn mock_db() -> sea_orm::DatabaseConnection {
        sea_orm::MockDatabase::new(sea_orm::DatabaseBackend::Sqlite).into_connection()
    }

    fn test_state() -> MiryadAuthState {
        MiryadAuthState {
            oidc_client: std::sync::Arc::new(MockOidcClient::default()),
            cookie_key: Key::from(&[0u8; 64]),
            post_login_redirect: "/".to_string(),
            post_logout_redirect: "/".to_string(),
            db: mock_db(),
            secure_cookies: false,
            token_pepper: String::new(),
        }
    }

    /// Handler du `Scenario` « session valide » : prend @`AuthUser` par valeur et concatène
    /// @subject, @email et @`id_token` séparés par un caractère `|`.
    async fn protected_handler(user: AuthUser) -> String {
        format!(
            "{}|{}|{}",
            user.subject,
            user.email.unwrap_or_default(),
            user.id_token
        )
    }

    fn make_app() -> Router {
        Router::new()
            .route("/protected", get(protected_handler))
            .with_state(test_state())
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("horloge post-epoch")
            .as_secs()
    }

    /// `JWT` tri-segment dont la claim `exp` vaut `exp` (base64 url-safe sans bourrage).
    fn make_jwt(exp: u64) -> String {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("header.{payload}.sig")
    }

    /// `JWT` tri-segment dont le payload ne porte aucune claim `exp`.
    fn make_jwt_without_exp() -> String {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"sub":"x"}"#);
        format!("header.{payload}.sig")
    }

    /// Scelle l'identité du contrat de test (`user-123`, `test@example.com`) autour de
    /// `id_token` via `build_set_cookie` sous `key`, et rend la paire `nom=valeur` utilisable
    /// en en-tête `Cookie`.
    fn session_pair(key: &Key, id_token: String) -> String {
        let identity = OidcIdentity {
            id_token,
            subject: "user-123".to_string(),
            email: Some("test@example.com".to_string()),
            preferred_username: None,
        };
        build_set_cookie(&identity, key, false)
            .split(';')
            .next()
            .expect("une ligne `Set-Cookie` porte une paire `nom=valeur`")
            .to_string()
    }

    /// Sert la requête et rend `(statut, Content-Type, corps)` — les `Then` des `Scenario`
    /// affirment chaque pièce par chaîne exacte.
    async fn serve(req: Request<Body>, app: Router) -> (StatusCode, String, String) {
        let resp = app
            .oneshot(req)
            .await
            .expect("le routeur ne rend jamais d'erreur de service");
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .expect("le rendu d'erreur comme la réponse du handler portent un `Content-Type`")
            .to_str()
            .expect("ascii dans `Content-Type`")
            .to_string();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("corps lisible");
        (status, content_type, String::from_utf8_lossy(&body).to_string())
    }

    fn request_with_cookie(pair: &str) -> Request<Body> {
        Request::builder()
            .uri("/protected")
            .header("Cookie", pair)
            .body(Body::empty())
            .expect("requête valide")
    }

    // ——— Les 11 `Scenario` de `middleware.sdd` ———

    /// `Scenario` : « session valide — les trois champs parviennent au handler par valeur » —
    /// corps exactement `user-123|test@example.com|<jwt>`, verbatim de l'`OidcIdentity`.
    #[tokio::test]
    async fn protected_with_valid_session_passes_and_exposes_subject() {
        let jwt = make_jwt(now_secs() + 3600);
        let pair = session_pair(&Key::from(&[0u8; 64]), jwt.clone());

        let (status, content_type, body) = serve(request_with_cookie(&pair), make_app()).await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            content_type.starts_with("text/plain"),
            "corps texte du handler : {content_type}"
        );
        assert_eq!(
            body,
            format!("user-123|test@example.com|{jwt}"),
            "les trois champs sont ceux de l'`OidcIdentity`, verbatim, le handler en est possesseur"
        );
    }

    /// `Scenario` : « en-tête Cookie absent — 401 avec code MRD-AUTH-001 » — corps exact de la
    /// `Display`, le handler ne s'exécute pas : le corps reçu n'est pas @subject.
    #[tokio::test]
    async fn protected_without_cookie_returns_401() {
        let req = Request::builder()
            .uri("/protected")
            .body(Body::empty())
            .expect("requête valide");

        let (status, content_type, body) = serve(req, make_app()).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(content_type, TEXT_PLAIN, "rejet rendu par ./error.rs en texte");
        assert_eq!(
            body, BODY_001,
            "corps exact, jamais le subject d'un handler non exécuté"
        );
    }

    /// `Scenario` : « en-tête Cookie sans entrée `miryad_session` — 401 MRD-AUTH-001 » — la
    /// recherche est exacte sur le nom, d'autres cookies ne sauraient le suppléer.
    #[tokio::test]
    async fn en_tete_cookie_sans_entree_miryad_session_rend_401_001() {
        let req = request_with_cookie("theme=dark; XSRF=abc; _ga=42");

        let (status, content_type, body) = serve(req, make_app()).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(body, BODY_001);
    }

    /// `Scenario` : « cookie scellé d'une autre clé — 401 MRD-AUTH-002, indistinguable d'une
    /// altération » — `PrivateJar::decrypt` rend `None` sans départager les causes.
    #[tokio::test]
    async fn cookie_scelle_dune_autre_cle_rend_401_002() {
        let pair = session_pair(&Key::from(&[7u8; 64]), make_jwt(now_secs() + 3600));

        let (status, content_type, body) = serve(request_with_cookie(&pair), make_app()).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(
            body, BODY_002,
            "clé étrangère et valeur re-signée rendent le même code"
        );
    }

    /// `Scenario` : « session dont le claim exp est passé — 401 MRD-AUTH-002 » — le cookie se
    /// déchiffre, c'est le contrôle d'horloge de ./cookie.rs qui rejette.
    #[tokio::test]
    async fn session_dont_le_claim_exp_est_passe_rend_401_002() {
        let pair = session_pair(&Key::from(&[0u8; 64]), make_jwt(1_000_000));

        let (status, content_type, body) = serve(request_with_cookie(&pair), make_app()).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(body, BODY_002);
    }

    /// `Scenario` : « cookie légitime mais `id_token` sans claim exp lisible — 401 MRD-AUTH-002 »
    /// — le cookie se déchiffre mais ne prouve aucune fraîcheur.
    #[tokio::test]
    async fn cookie_legitime_sans_claim_exp_lisible_rend_401_002() {
        let pair = session_pair(&Key::from(&[0u8; 64]), make_jwt_without_exp());

        let (status, content_type, body) = serve(request_with_cookie(&pair), make_app()).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(body, BODY_002);
    }

    /// `Scenario` : « en-tête Cookie non UTF-8 — traité comme absent, 401 MRD-AUTH-001 » —
    /// octets opaques acceptés par le crate `http` que `to_str` refuse ; l'illisible est
    /// indiscernable de l'absent.
    #[tokio::test]
    async fn en_tete_cookie_non_utf8_traite_comme_absent_rend_401_001() {
        let opaque = HeaderValue::from_bytes(b"miryad_session=abc\xfa\xfb")
            .expect("le crate `http` accepte des octets opaques non UTF-8");
        let req = Request::builder()
            .uri("/protected")
            .header("Cookie", opaque)
            .body(Body::empty())
            .expect("requête valide");

        let (status, content_type, body) = serve(req, make_app()).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(
            body, BODY_001,
            "la `to_str` ratée se confond avec l'absence, même code"
        );
    }

    /// `Scenario` : « bearer sans cookie — 401 MRD-AUTH-001, Authorization jamais consulté » —
    /// l'extrait ne lit que `Cookie` ; la base non préparée n'est pas approchée (un accès
    /// remonterait en `500`, pas en `401`).
    #[tokio::test]
    async fn bearer_sans_cookie_rend_401_001_et_authorization_jamais_consulte() {
        let req = Request::builder()
            .uri("/protected")
            .header("Authorization", "Bearer un-token-importe-aucune-importance")
            .body(Body::empty())
            .expect("requête valide");

        let (status, content_type, body) = serve(req, make_app()).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(body, BODY_001, "le bearer est invisible pour cet extracteur");
    }

    /// `Scenario` : « mock db nu — aucune requête exécutée sur une extraction valide » — la
    /// base partagée est un `MockDatabase` nu (toute requête y échouerait) et son journal de
    /// transactions reste vide : @subject arrive par le seul chemin du cookie.
    #[tokio::test]
    async fn mock_db_nu_aucune_requete_nest_executee_sur_extraction_valide() {
        let db = mock_db();
        let witness = db.clone();
        let app = Router::new()
            .route("/protected", get(protected_handler))
            .with_state(MiryadAuthState { db, ..test_state() });
        let pair = session_pair(&Key::from(&[0u8; 64]), make_jwt(now_secs() + 3600));

        let (status, _content_type, body) = serve(request_with_cookie(&pair), app).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with("user-123|"), "l'extraction a réussi : {body}");
        assert!(
            witness.into_transaction_log().is_empty(),
            "@db n'est jamais atteint, même pour une extraction valide"
        );
    }

    /// `Scenario` : « état composé de l'app consommatrice via `FromRef` explicite » — @S =
    /// `AppState` porté par un @`FromRef` explicite, et la forme réflexive nue (@S =
    /// @`MiryadAuthState`, impl blanket d'axum-core 0.5.6) : les deux formes montent le handler.
    #[tokio::test]
    async fn etat_compose_de_lapp_consommatrice_via_from_ref_explicite() {
        #[derive(Clone)]
        struct AppState {
            auth: MiryadAuthState,
            other: i32,
        }
        impl FromRef<AppState> for MiryadAuthState {
            fn from_ref(input: &AppState) -> Self {
                input.auth.clone()
            }
        }

        let pair = session_pair(&Key::from(&[0u8; 64]), make_jwt(now_secs() + 3600));

        let state = AppState {
            auth: test_state(),
            other: 7,
        };
        assert_eq!(
            state.other, 7,
            "fixture : l'état composé porte bien un champ étranger à @auth"
        );
        let composed = Router::<AppState>::new()
            .route("/protected", get(protected_handler))
            .with_state(state);
        let (status, _content_type, body) = serve(request_with_cookie(&pair), composed).await;
        assert_eq!(status, StatusCode::OK, "projection `FromRef` explicite");
        assert!(
            body.starts_with("user-123|"),
            "le handler lit le @subject validé : {body}"
        );

        let (bare_status, _, bare_body) = serve(request_with_cookie(&pair), make_app()).await;
        assert_eq!(bare_status, StatusCode::OK, "état réflexif nu par l'impl blanket");
        assert!(bare_body.starts_with("user-123|"), "{bare_body}");
    }

    /// `Scenario` : « Debug affiche les trois champs sans panic » (arbitré 2026-09-27) —
    /// aucun secret à rédiger : les trois champs sont déjà lisibles par le porteur
    /// authentifié lui-même.
    #[test]
    fn debug_displays_three_fields_without_panic() {
        let user = AuthUser {
            subject: "user-123".to_string(),
            email: Some("test@example.com".to_string()),
            id_token: "header.payload.sig".to_string(),
        };

        let rendered = format!("{user:?}");
        for attendu in ["AuthUser", "user-123", "test@example.com", "header.payload.sig"] {
            assert!(
                rendered.contains(attendu),
                "le `Debug` doit contenir {attendu:?} : {rendered}"
            );
        }
    }
}
