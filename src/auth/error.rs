use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("MRD-AUTH-001: not authenticated (no session cookie)")]
    NotAuthenticated,
    #[error("MRD-AUTH-002: invalid or expired session")]
    InvalidSession,
    #[error("MRD-AUTH-003: OIDC error: {0}")]
    Oidc(String),
    #[error("MRD-AUTH-014: invalid or unknown API token")]
    InvalidToken,
    #[error("MRD-AUTH-015: expired API token")]
    TokenExpired,
    #[error("MRD-AUTH-016: database error: {0}")]
    Database(#[from] sea_orm::DbErr),
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let status = match self {
            AuthError::NotAuthenticated | AuthError::InvalidSession => StatusCode::UNAUTHORIZED,
            AuthError::Oidc(_) => StatusCode::BAD_GATEWAY,
            AuthError::InvalidToken | AuthError::TokenExpired => StatusCode::UNAUTHORIZED,
            AuthError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, self.to_string()).into_response()
    }
}
