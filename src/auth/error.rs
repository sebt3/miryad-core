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
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let status = match self {
            AuthError::NotAuthenticated | AuthError::InvalidSession => StatusCode::UNAUTHORIZED,
            AuthError::Oidc(_) => StatusCode::BAD_GATEWAY,
        };
        (status, self.to_string()).into_response()
    }
}
