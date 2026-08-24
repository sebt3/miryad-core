use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::resource::HookError;

#[derive(Debug, thiserror::Error)]
pub enum RestError {
    #[error("MRD-REST-001: resource not found")]
    NotFound,
    #[error("MRD-REST-002: access denied")]
    Forbidden,
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
            RestError::Database(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()).into_response(),
            RestError::Application(ref err) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ApplicationErrorBody {
                    code: err.code.as_deref(),
                    message: &err.message,
                }),
            )
                .into_response(),
            RestError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()).into_response(),
        }
    }
}
