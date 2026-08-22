use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum RestError {
    #[error("MRD-REST-001: resource not found")]
    NotFound,
    #[error("MRD-REST-002: access denied")]
    Forbidden,
    #[error("MRD-REST-003: database error: {0}")]
    Database(#[from] sea_orm::DbErr),
}

impl IntoResponse for RestError {
    fn into_response(self) -> Response {
        let status = match self {
            RestError::NotFound => StatusCode::NOT_FOUND,
            RestError::Forbidden => StatusCode::FORBIDDEN,
            RestError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, self.to_string()).into_response()
    }
}
