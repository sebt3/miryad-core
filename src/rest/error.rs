//! Taxonomie d'erreur de la surface REST — le seul type public `RestError` à sept
//! variantes, chacune portant soit un code unique `MRD-REST-001` à `MRD-REST-006` dans
//! sa `Display`, soit le `HookError` applicatif du consommateur, jamais codé `MRD-*`.
//! L'`IntoResponse` décidé ici fixe par variante le statut et la forme du corps : texte
//! porteur du code, ou JSON `{code, message}` pour le rejet métier. Les corps `500` sont
//! génériques (`MRD-REST-003: database error` / `MRD-REST-004: internal error`) et le
//! détail part dans une trace `error!` — jamais sur le fil (arbitré 2026-09-29). Le fichier
//! ne construit lui-même aucune erreur — les variantes viennent des handlers du module
//! (`mod.rs`, `admin.rs`, `me.rs`, `tokens.rs`, via `core.rs`) ; le même `RestError`
//! irrigue la surface MCP par la conversion `From<RestError> for McpError` (feature
//! `mcp`), qui rend une enveloppe JSON-RPC à la place de ce `IntoResponse`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::resource::HookError;

/// Erreur des handlers REST — sept variantes, chacune portant soit un code unique
/// `MRD-REST-NNN` (`001` à `006`) dans sa `Display`, soit le `HookError` applicatif
/// (`Application`, jamais `MRD-*`). Son `IntoResponse` rend statut et corps par variante.
#[derive(Debug, thiserror::Error)]
pub enum RestError {
    /// `MRD-REST-001` — ressource absente pour l'identifiant demandé, rendu `404`.
    #[error("MRD-REST-001: resource not found")]
    NotFound,
    /// `MRD-REST-002` — refus RBAC (liste refusée, lecture/écriture/création non
    /// autorisées), rendu `403`.
    #[error("MRD-REST-002: access denied")]
    Forbidden,
    /// `MRD-REST-003` — `DbErr` de `sea-orm` propagé (`#[from]`, seul `From` dérivé),
    /// rendu `500`.
    #[error("MRD-REST-003: database error: {0}")]
    Database(#[from] sea_orm::DbErr),
    /// Erreur métier applicative (hook) — jamais un `MRD-*`, cf. `HookError`.
    #[error("{}", .0.message)]
    Application(HookError),
    /// Enveloppe une erreur d'un autre sous-système (ex. `auth::AuthError` dans
    /// `rest::tokens`) dont seule une variante est réellement atteignable depuis un handler REST
    /// — pas de `From` générique qui laisserait croire à une conversion sans perte.
    #[error("MRD-REST-004: internal error: {0}")]
    Internal(String),
    /// `MRD-REST-005` — entrée invalide signalée par la crate (message contrôlé, jamais un
    /// texte de driver), rendu `422` en texte porteur du code (sixième variante, arbitrée
    /// par Sébastien le 2026-09-29) ; `422` partagé avec `Application`, qui garde son corps JSON.
    #[error("MRD-REST-005: invalid input: {0}")]
    InvalidInput(String),
    /// `MRD-REST-006` — conflit d'empreinte de token, rendu `409` (septième variante,
    /// arbitrée par Sébastien le 2026-10-03 : une décision de politique, pas une panne
    /// interne). Émetteur : `to_rest_error` de `rest::tokens` sur
    /// `AuthError::TokenHashConflict`, parité avec le `409` de `auth::error`.
    #[error("MRD-REST-006: conflict")]
    Conflict,
}

#[derive(Serialize)]
struct ApplicationErrorBody<'a> {
    code: Option<&'a str>,
    message: &'a str,
}

impl IntoResponse for RestError {
    fn into_response(self) -> Response {
        match self {
            RestError::NotFound => (StatusCode::NOT_FOUND, self.to_string()).into_response(),
            RestError::Forbidden => (StatusCode::FORBIDDEN, self.to_string()).into_response(),
            // `500` génériques (arbitré par Sébastien le 2026-09-29) : le client ne voit que
            // le code, la `Display` complète (détail du `DbErr`, charge utile) part dans
            // l'unique trace `error!` de la variante — jamais sur le fil.
            rest @ RestError::Database(_) => {
                tracing::error!("rest error: {rest}");
                (StatusCode::INTERNAL_SERVER_ERROR, "MRD-REST-003: database error").into_response()
            }
            rest @ RestError::Internal(_) => {
                tracing::error!("rest error: {rest}");
                (StatusCode::INTERNAL_SERVER_ERROR, "MRD-REST-004: internal error").into_response()
            }
            RestError::Application(ref err) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ApplicationErrorBody {
                    code: err.code.as_deref(),
                    message: &err.message,
                }),
            )
                .into_response(),
            RestError::InvalidInput(_) => {
                (StatusCode::UNPROCESSABLE_ENTITY, self.to_string()).into_response()
            }
            RestError::Conflict => (StatusCode::CONFLICT, self.to_string()).into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::membership::trace_capture::CapturedTraces;
    use crate::users::membership::trace_capture::capture_traces;
    use axum::http::header::CONTENT_TYPE;
    use std::error::Error as _;

    /// Corps du contrat `text/plain` vérifié au source d'axum-core 0.5.6 (`Must`).
    const TEXT_PLAIN: &str = "text/plain; charset=utf-8";

    /// `Scenario` : « rendu HTTP 422 JSON avec code de hook » — `axum::Json` pose
    /// `application/json` sans charset (axum 0.8.9, `Must`).
    const APPLICATION_JSON: &str = "application/json";

    fn database_boom() -> RestError {
        RestError::Database(sea_orm::DbErr::Custom("boom".to_string()))
    }

    /// Lecture `Content-Type` + corps d'un rendu (`Scenario` de rendu, `Accepts` de la spec).
    async fn render(err: RestError) -> (StatusCode, String, axum::body::Bytes) {
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

    /// Lignes `error` capturées, pour les clauses « exactement un événement `error` ».
    fn error_lines(traces: &CapturedTraces) -> Vec<String> {
        traces
            .lines()
            .into_iter()
            .filter(|(level, _)| *level == tracing::Level::ERROR)
            .map(|(_, message)| message)
            .collect()
    }

    // ——— Display : une variante = un littéral exact, par `to_string` ———

    /// `Scenario` : « Display 001 ressource absente ».
    #[test]
    fn display_001_resource_not_found() {
        assert_eq!(
            RestError::NotFound.to_string(),
            "MRD-REST-001: resource not found"
        );
    }

    /// `Scenario` : « Display 002 accès refusé ».
    #[test]
    fn display_002_access_denied() {
        assert_eq!(RestError::Forbidden.to_string(), "MRD-REST-002: access denied");
    }

    /// `Scenario` : « Display 003 `DbErr` embarqué » — préfixe `Custom Error: ` de
    /// `sea_orm::DbErr::Custom` vérifié dans sea-orm 2.0.2 (`Must`).
    #[test]
    fn display_003_database_carries_embedded_dberr() {
        assert_eq!(
            database_boom().to_string(),
            "MRD-REST-003: database error: Custom Error: boom"
        );
    }

    /// `Scenario` : « Display 004 charge utile verbatim ».
    #[test]
    fn display_004_internal_carries_payload_verbatim() {
        assert_eq!(
            RestError::Internal("upstream exploded".to_string()).to_string(),
            "MRD-REST-004: internal error: upstream exploded"
        );
    }

    /// `Scenario` : « Display `Application` message nu sans code » — ni le code, ni son
    /// sous-préfixe, aucune sous-chaîne `MRD-` (`Must`).
    #[test]
    fn display_application_is_bare_message_without_code() {
        let rendered =
            RestError::Application(HookError::with_code("WIDGET-001", "label must not be empty")).to_string();
        assert_eq!(rendered, "label must not be empty");
        assert!(!rendered.contains("WIDGET-001"));
        assert!(!rendered.contains("WIDGET-"));
        assert!(!rendered.contains("MRD-"));
    }

    // ——— @Error::source et conversions dérivées ———

    /// `Scenario` : « source de l'erreur sur `Database` seulement » — `#[from]` vaut
    /// `#[source]`, `Application` compris vaut `None` (`Must`).
    #[test]
    fn error_source_is_some_only_for_database() {
        let errors = [
            RestError::NotFound,
            RestError::Forbidden,
            database_boom(),
            RestError::Application(HookError::new("hooked")),
            RestError::Internal("upstream exploded".to_string()),
            RestError::InvalidInput("bad input".to_string()),
            RestError::Conflict,
        ];
        assert_eq!(errors.len(), 7, "une valeur par variante, sept depuis 2026-10-03");
        let mut database_checked = false;
        for err in &errors {
            match err {
                RestError::Database(_) => {
                    let source = err.source().expect("Database carries its DbErr as source");
                    let db_err = source
                        .downcast_ref::<sea_orm::DbErr>()
                        .expect("source is the embedded DbErr");
                    assert_eq!(db_err.to_string(), "Custom Error: boom");
                    database_checked = true;
                }
                other => {
                    assert!(
                        other.source().is_none(),
                        "seule Database porte une source : {other}"
                    );
                }
            }
        }
        assert!(database_checked, "la variante Database était absente de la table");
    }

    /// `Scenario` : « `DbErr` propagé par l'opérateur d'interrogation devient `Database` » —
    /// la conversion `#[from]` est le chemin du `?` des dix handlers (`Must`).
    #[test]
    fn from_dberr_becomes_database_and_keeps_source() {
        let err: RestError = sea_orm::DbErr::Custom("connection lost".to_string()).into();
        assert!(
            matches!(err, RestError::Database(_)),
            "la conversion dérivée vaut Database : {err:?}"
        );
        assert_eq!(
            err.to_string(),
            "MRD-REST-003: database error: Custom Error: connection lost"
        );
        let source = err.source().expect("converted error keeps its source");
        assert_eq!(
            source
                .downcast_ref::<sea_orm::DbErr>()
                .expect("source is the DbErr")
                .to_string(),
            "Custom Error: connection lost"
        );
    }

    // ——— Rendu HTTP : table variante → statut, corps exact, `Content-Type` ———

    /// `Scenario` : « rendu HTTP 404 pour 001 ».
    #[tokio::test]
    async fn http_render_001_not_found_is_404() {
        let (status, content_type, body) = render(RestError::NotFound).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(&body[..], b"MRD-REST-001: resource not found");
    }

    /// `Scenario` : « rendu HTTP 403 pour 002 ».
    #[tokio::test]
    async fn http_render_002_forbidden_is_403() {
        let (status, content_type, body) = render(RestError::Forbidden).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(&body[..], b"MRD-REST-002: access denied");
    }

    /// `Scenario` : « rendu HTTP 500 pour 003, corps générique et détail en trace » —
    /// arbitré par Sébastien le 2026-09-29 : le `DbErr` ne sort jamais sur le fil.
    #[tokio::test]
    async fn http_render_003_database_is_500_generic_body_with_error_trace() {
        let (_guard, traces) = capture_traces();
        let (status, content_type, body) = render(database_boom()).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(&body[..], b"MRD-REST-003: database error");
        let rendered = std::str::from_utf8(&body).expect("utf-8 body");
        assert!(
            !rendered.contains("boom"),
            "le détail du DbErr ne doit jamais passer sur le fil : {rendered}"
        );
        let errors = error_lines(&traces);
        assert_eq!(errors.len(), 1, "exactement un événement error capturé");
        assert!(
            errors[0].contains("boom"),
            "la trace error! porte la Display complète (détail du DbErr) : {}",
            errors[0]
        );
    }

    /// `Scenario` : « rendu HTTP 500 pour 004, corps générique et détail en trace » —
    /// arbitré par Sébastien le 2026-09-29 : la charge utile ne sort jamais sur le fil.
    #[tokio::test]
    async fn http_render_004_internal_is_500_generic_body_with_error_trace() {
        let (_guard, traces) = capture_traces();
        let (status, content_type, body) = render(RestError::Internal("upstream exploded".to_string())).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(&body[..], b"MRD-REST-004: internal error");
        assert!(
            !std::str::from_utf8(&body)
                .expect("utf-8 body")
                .contains("upstream exploded"),
            "la charge utile ne doit jamais passer sur le fil"
        );
        let errors = error_lines(&traces);
        assert_eq!(errors.len(), 1, "exactement un événement error capturé");
        assert!(
            errors[0].contains("upstream exploded"),
            "la trace error! porte la Display complète (charge utile) : {}",
            errors[0]
        );
    }

    /// `Scenario` : « rendu HTTP 422 JSON avec code de hook » — deux clés exactes,
    /// interprété par `serde_json` (`Must` : pas de comparaison d'octets).
    #[tokio::test]
    async fn http_render_application_with_code_is_422_json() {
        let err = RestError::Application(HookError::with_code("WIDGET-001", "label must not be empty"));
        let (status, content_type, body) = render(err).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(content_type, APPLICATION_JSON);
        let value: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON body");
        let object = value.as_object().expect("JSON object body");
        assert_eq!(object.len(), 2, "exactement les clés code et message");
        assert_eq!(object["code"], "WIDGET-001");
        assert_eq!(object["message"], "label must not be empty");
        let rendered = std::str::from_utf8(&body).expect("utf-8 body");
        assert!(
            !rendered.contains("MRD-"),
            "le corps 422 ne contient jamais MRD- : {rendered}"
        );
    }

    /// `Scenario` : « rendu HTTP 422 JSON sans code de hook » — la clé `code` est présente
    /// et vaut `null`, aucun `skip_serializing_if` (`Must`).
    #[tokio::test]
    async fn http_render_application_without_code_is_422_json_null_code() {
        let (status, content_type, body) = render(RestError::Application(HookError::new("boom"))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(content_type, APPLICATION_JSON);
        let value: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON body");
        let object = value.as_object().expect("JSON object body");
        assert!(object.contains_key("code"), "la clé code est présente");
        assert_eq!(object["code"], serde_json::Value::Null);
        assert_eq!(object["message"], "boom");
    }

    /// `Scenario` : « rendu HTTP 422 pour 005 » — sixième variante arbitrée par Sébastien
    /// le 2026-09-29 : texte porteur du code, `422` partagé avec `Application`.
    #[tokio::test]
    async fn http_render_005_invalid_input_is_422() {
        let (status, content_type, body) = render(RestError::InvalidInput(
            "expires_at must be in the future".to_string(),
        ))
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(
            &body[..],
            b"MRD-REST-005: invalid input: expires_at must be in the future"
        );
    }

    /// `Scenario` : « rendu HTTP 409 pour 006 » — septième variante arbitrée par Sébastien
    /// le 2026-10-03, émetteur `to_rest_error` de `rest::tokens` sur
    /// `AuthError::TokenHashConflict` (parité avec le `409` de `auth::error`). La variante
    /// est muette — la spec n'autorise que les deux traces `error!` des variantes `500` —
    /// donc aucun abonné `tracing` n'est capturé ici : le mutisme est certifié par la
    /// branche du `match` qui ne pose que statut + corps (`Done when` de la spec).
    #[tokio::test]
    async fn http_render_006_conflict_is_409() {
        let err = RestError::Conflict;
        assert_eq!(err.to_string(), "MRD-REST-006: conflict");
        let (status, content_type, body) = render(err).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(&body[..], b"MRD-REST-006: conflict");
    }

    /// `Scenario` : « `Internal` portant un `AuthError` garde le code AUTH hors du corps
    /// client » — ce que produit `to_rest_error` de ./tokens.rs ; le double code ne vit que
    /// dans la `Display` et la trace (arbitré par Sébastien le 2026-09-29).
    #[tokio::test]
    async fn internal_carrying_auth_error_keeps_auth_code_out_of_body() {
        let auth_err = crate::auth::AuthError::Oidc("discovery failed".to_string());
        let err = RestError::Internal(auth_err.to_string());
        assert_eq!(
            err.to_string(),
            "MRD-REST-004: internal error: MRD-AUTH-003: OIDC error: discovery failed"
        );
        let (_guard, traces) = capture_traces();
        let (status, content_type, body) = render(err).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(content_type, TEXT_PLAIN);
        assert_eq!(&body[..], b"MRD-REST-004: internal error");
        let errors = error_lines(&traces);
        assert_eq!(errors.len(), 1, "exactement un événement error capturé");
        assert!(
            errors[0].contains("MRD-AUTH-003"),
            "le code AUTH imbriqué vit dans la trace : {}",
            errors[0]
        );
    }
}
