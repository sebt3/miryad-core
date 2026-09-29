use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// Erreurs d'authentification du module `auth` : neuf variantes portant chacune un code unique
/// `MRD-AUTH-NNN` dans son `Display`, rendues en `text/plain` par l'implémentation
/// `IntoResponse` de ce fichier selon la table variante → statut (invariante, sans joker).
/// Dérives `Debug` et `thiserror::Error` uniquement — ni `Clone` ni `PartialEq`.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Variante unitaire, code `MRD-AUTH-001`, rendue `401` — aucun cookie de session
    /// `miryad_session` présent ; émise par `cookie::extract_session`.
    #[error("MRD-AUTH-001: not authenticated (no session cookie)")]
    NotAuthenticated,
    /// Variante unitaire, code `MRD-AUTH-002`, rendue `401` — cookie de session présent mais
    /// indéchiffrable, malformé ou périmé (claim `exp` atteinte, `exp == now` compris) ; émise
    /// par `cookie::extract_session`.
    #[error("MRD-AUTH-002: invalid or expired session")]
    InvalidSession,
    /// `MRD-AUTH-012` — échec piloté par le client sur la requête de callback elle-même
    /// (cookie pending absent, expiré ou malformé) ; variante propre depuis l'arbitrage
    /// 2026-09-27, rendue `400` — plus une charge utile d'`Oidc` sous `502`.
    #[error("MRD-AUTH-012: invalid or missing OIDC callback state")]
    InvalidCallback,
    /// `MRD-AUTH-013` — `state` CSRF reçu divergeant de celui attendu ; variante propre
    /// depuis l'arbitrage 2026-09-27, rendue `400` — plus une charge utile d'`Oidc` sous
    /// `502`.
    #[error("MRD-AUTH-013: CSRF state mismatch")]
    CsrfMismatch,
    /// Variante tuple d'un `String`, code `MRD-AUTH-003`, rendue `502` — l'espace libre où les
    /// appelants logent les codes internes `MRD-AUTH-004` à `MRD-AUTH-011` (les codes `012`/`013`
    /// en sont sortis le 2026-09-27, vers leurs variantes propres ci-dessus).
    #[error("MRD-AUTH-003: OIDC error: {0}")]
    Oidc(String),
    /// Variante unitaire, code `MRD-AUTH-014`, rendue `401` — l'empreinte poivrée du token
    /// présenté ne correspond à aucune ligne de `miryad_api_tokens` (inconnu ou déjà révoqué),
    /// ou la ligne s'est fait supprimer par `revoke_token` entre la lecture et l'horodatage.
    #[error("MRD-AUTH-014: invalid or unknown API token")]
    InvalidToken,
    /// Variante unitaire, code `MRD-AUTH-015`, rendue `401` — la ligne du token existe mais son
    /// `expires_at` est atteint ou dépassé (comparaison `<=` à l'instant de validation).
    #[error("MRD-AUTH-015: expired API token")]
    TokenExpired,
    /// `MRD-AUTH-017` — l'empreinte fournie est déjà provisionnée en base sous un autre
    /// `subject` (`ensure_token`) ; variante arbitrée en session avec Sébastien le
    /// 2026-09-27, rendue `409` : la ressource est déjà allouée.
    #[error("MRD-AUTH-017: token hash already provisioned for another subject")]
    TokenHashConflict,
    /// Variante tuple d'un `sea_orm::DbErr` sous `#[from]`, code `MRD-AUTH-016`, rendue `500` —
    /// la voie de passage des `DbErr` nus du module (et de l'opérateur `?` de tout appel
    /// `SeaORM`) : un `DbErr` de traversée n'est pas une erreur miryad, il traverse nu jusqu'à
    /// cette décoration.
    #[error("MRD-AUTH-016: database error: {0}")]
    Database(#[from] sea_orm::DbErr),
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        // Table invariante et exhaustive, sans joker : l'ajout d'une variante interrompt
        // la compilation jusqu'à son arbitrage de statut (`src/auth/error.sdd` `Must`).
        let status = match self {
            // 001/002 (cookie de session) et 014/015 (token API) sont confondues au niveau
            // HTTP sous `401` — seul le code du corps distingue les variantes (`Must`).
            AuthError::NotAuthenticated
            | AuthError::InvalidSession
            | AuthError::InvalidToken
            | AuthError::TokenExpired => StatusCode::UNAUTHORIZED,
            AuthError::InvalidCallback | AuthError::CsrfMismatch => StatusCode::BAD_REQUEST,
            AuthError::Oidc(_) => StatusCode::BAD_GATEWAY,
            AuthError::TokenHashConflict => StatusCode::CONFLICT,
            AuthError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, self.to_string()).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::CONTENT_TYPE;

    /// Corps du contrat `text/plain` vérifié au source d'axum-core 0.5.6 (`Must`).
    const TEXT_PLAIN: &str = "text/plain; charset=utf-8";

    fn database_boom() -> AuthError {
        AuthError::Database(sea_orm::DbErr::Custom("boom".to_string()))
    }

    /// Lecture `Content-Type` + corps (octets exacts de la `Display`) d'un rendu.
    async fn render(err: AuthError) -> (axum::http::StatusCode, String, axum::body::Bytes) {
        let resp = err.into_response();
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .expect("Content-Type header present")
            .to_str()
            .expect("valid Content-Type")
            .to_string();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        (status, content_type, body)
    }

    // ——— Display : une variante = un littéral exact, par `to_string` ———

    /// `Scenario` : « Display 001 cookie de session absent ».
    #[test]
    fn display_001_not_authenticated() {
        assert_eq!(
            AuthError::NotAuthenticated.to_string(),
            "MRD-AUTH-001: not authenticated (no session cookie)"
        );
    }

    /// `Scenario` : « Display 002 session invalide ».
    #[test]
    fn display_002_invalid_session() {
        assert_eq!(
            AuthError::InvalidSession.to_string(),
            "MRD-AUTH-002: invalid or expired session"
        );
    }

    /// `Scenario` : « Display 003 charge utile transmise verbatim » — patron `{0}`,
    /// aucun filtre ni troncature : la charge utile suit le préfixe octet pour octet.
    #[test]
    fn display_003_oidc_payload_verbatim() {
        let rendered = AuthError::Oidc("issuer unreachable".to_string()).to_string();
        assert_eq!(rendered, "MRD-AUTH-003: OIDC error: issuer unreachable");
        assert_eq!(
            rendered.strip_prefix("MRD-AUTH-003: OIDC error: "),
            Some("issuer unreachable")
        );
    }

    /// `Scenario` : « Display 014 token inconnu ou révoqué ».
    #[test]
    fn display_014_invalid_token() {
        assert_eq!(
            AuthError::InvalidToken.to_string(),
            "MRD-AUTH-014: invalid or unknown API token"
        );
    }

    /// `Scenario` : « Display 015 token expiré ».
    #[test]
    fn display_015_token_expired() {
        assert_eq!(
            AuthError::TokenExpired.to_string(),
            "MRD-AUTH-015: expired API token"
        );
    }

    /// `Scenario` : « Display 016 `DbErr` embarqué » — préfixe `Custom Error: ` de
    /// sea-orm 2.0.2 inclus, exposition verbatim confirmée comme contrat voulu.
    #[test]
    fn display_016_database_embedded_err() {
        assert_eq!(
            database_boom().to_string(),
            "MRD-AUTH-016: database error: Custom Error: boom"
        );
    }

    /// `Scenario` : « Display 012 callback invalide » — variante propre depuis
    /// l'arbitrage 2026-09-27, littéral exact du `Must`.
    #[test]
    fn display_012_invalid_callback() {
        assert_eq!(
            AuthError::InvalidCallback.to_string(),
            "MRD-AUTH-012: invalid or missing OIDC callback state"
        );
    }

    /// `Scenario` : « Display 013 CSRF divergent » — variante propre depuis
    /// l'arbitrage 2026-09-27, littéral exact du `Must`.
    #[test]
    fn display_013_csrf_mismatch() {
        assert_eq!(
            AuthError::CsrfMismatch.to_string(),
            "MRD-AUTH-013: CSRF state mismatch"
        );
    }

    /// `Scenario` : « Display 017 empreinte détenue par un autre sujet » — littéral
    /// exact arbitré en session avec Sébastien le 2026-09-27.
    #[test]
    fn display_017_token_hash_conflict() {
        assert_eq!(
            AuthError::TokenHashConflict.to_string(),
            "MRD-AUTH-017: token hash already provisioned for another subject"
        );
    }

    // ——— Rendu HTTP : table variante → statut exacte, corps = octets de la Display ———

    /// `Scenario` : « rendu HTTP 401 pour 001 ».
    #[tokio::test]
    async fn http_render_001_not_authenticated_is_401() {
        let (status, content_type, body) = render(AuthError::NotAuthenticated).await;
        assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(&body[..], b"MRD-AUTH-001: not authenticated (no session cookie)");
    }

    /// `Scenario` : « rendu HTTP 401 pour 002 ».
    #[tokio::test]
    async fn http_render_002_invalid_session_is_401() {
        let (status, content_type, body) = render(AuthError::InvalidSession).await;
        assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(&body[..], b"MRD-AUTH-002: invalid or expired session");
    }

    /// `Scenario` : « rendu HTTP 400 pour 012 » — arbitré 2026-09-27, plus `502`.
    #[tokio::test]
    async fn http_render_012_invalid_callback_is_400() {
        let (status, content_type, _) = render(AuthError::InvalidCallback).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(content_type, TEXT_PLAIN);
    }

    /// `Scenario` : « rendu HTTP 400 pour 013 » — arbitré 2026-09-27, plus `502`.
    #[tokio::test]
    async fn http_render_013_csrf_mismatch_is_400() {
        let (status, content_type, _) = render(AuthError::CsrfMismatch).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(content_type, TEXT_PLAIN);
    }

    /// `Scenario` : « rendu HTTP 502 pour 003 » — `Oidc` réservé aux pannes réelles
    /// du fournisseur, charge utile rendue sous le préfixe de catégorie.
    #[tokio::test]
    async fn http_render_003_oidc_is_502() {
        let (status, content_type, body) = render(AuthError::Oidc("discovery failed".to_string())).await;
        assert_eq!(status, axum::http::StatusCode::BAD_GATEWAY);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(&body[..], b"MRD-AUTH-003: OIDC error: discovery failed");
    }

    /// `Scenario` : « rendu HTTP 401 pour 014 ».
    #[tokio::test]
    async fn http_render_014_invalid_token_is_401() {
        let (status, content_type, body) = render(AuthError::InvalidToken).await;
        assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(&body[..], b"MRD-AUTH-014: invalid or unknown API token");
    }

    /// `Scenario` : « rendu HTTP 401 pour 015 » — confondu au niveau HTTP avec 014,
    /// seul le code du corps distingue les variantes.
    #[tokio::test]
    async fn http_render_015_token_expired_is_401() {
        let (status, content_type, body) = render(AuthError::TokenExpired).await;
        assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(&body[..], b"MRD-AUTH-015: expired API token");
    }

    /// `Scenario` : « rendu HTTP 500 pour 016 » — `DbErr` verbatim sur le fil,
    /// exposition confirmée comme voulue (self-hosted).
    #[tokio::test]
    async fn http_render_016_database_is_500() {
        let (status, content_type, body) = render(database_boom()).await;
        assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(&body[..], b"MRD-AUTH-016: database error: Custom Error: boom");
    }

    /// `Scenario` : « rendu HTTP 409 pour 017 » — arbitré en session avec Sébastien
    /// le 2026-09-27, l'empreinte est une ressource déjà allouée.
    #[tokio::test]
    async fn http_render_017_token_hash_conflict_is_409() {
        let (status, content_type, body) = render(AuthError::TokenHashConflict).await;
        assert_eq!(status, axum::http::StatusCode::CONFLICT);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(
            &body[..],
            b"MRD-AUTH-017: token hash already provisioned for another subject"
        );
    }

    // ——— Conversion `#[from]` et contrat de `source` ———

    /// `Scenario` : « erreur `SeaORM` convertie par l'opérateur de propagation » —
    /// le chemin qu'emprunte `?` dans `issue_token`, `validate_token`,
    /// `revoke_token`, `ensure_token` et `graphql_handler`.
    #[test]
    fn db_err_converts_to_database_variant() {
        let err = AuthError::from(sea_orm::DbErr::Custom("connection lost".to_string()));
        assert!(
            matches!(err, AuthError::Database(ref db_err)
                if db_err.to_string() == "Custom Error: connection lost"),
            "la conversion conserve l'erreur d'origine : {err}"
        );
        assert_eq!(
            err.to_string(),
            "MRD-AUTH-016: database error: Custom Error: connection lost"
        );
    }

    /// `Scenario` : « source présente sur 016 seulement » — `#[from]` vaut
    /// `#[source]` (thiserror-impl 2.0.20) ; les huit autres valent `None`,
    /// `Oidc` et les trois variantes 2026-09-27 compris.
    #[test]
    fn source_is_some_on_database_only() {
        let nine: [(AuthError, &str); 9] = [
            (AuthError::NotAuthenticated, "NotAuthenticated"),
            (AuthError::InvalidSession, "InvalidSession"),
            (AuthError::InvalidCallback, "InvalidCallback"),
            (AuthError::CsrfMismatch, "CsrfMismatch"),
            (AuthError::Oidc("x".to_string()), "Oidc"),
            (AuthError::InvalidToken, "InvalidToken"),
            (AuthError::TokenExpired, "TokenExpired"),
            (AuthError::TokenHashConflict, "TokenHashConflict"),
            (database_boom(), "Database"),
        ];
        for (err, name) in nine {
            let source = std::error::Error::source(&err);
            if name == "Database" {
                let db_err = source
                    .and_then(|e| e.downcast_ref::<sea_orm::DbErr>())
                    .expect("Database source is the embedded DbErr");
                assert_eq!(db_err.to_string(), "Custom Error: boom");
            } else {
                assert!(
                    source.is_none(),
                    "{name} doit rendre source() == None, pas {source:?}"
                );
            }
        }
    }
}
