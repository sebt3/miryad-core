//! Taxonomie d'erreur de la surface REST — le seul type public `RestError` à cinq
//! variantes, chacune portant soit un code unique `MRD-REST-001` à `MRD-REST-004` dans
//! sa `Display`, soit le `HookError` applicatif du consommateur, jamais codé `MRD-*`.
//! L'`IntoResponse` décidé ici fixe par variante le statut et la forme du corps : texte
//! porteur du code, ou JSON `{code, message}` pour le rejet métier. Le fichier ne
//! construit lui-même aucune erreur — les variantes viennent des handlers du module
//! (`mod.rs`, `admin.rs`, `me.rs`, `tokens.rs`, via `core.rs`) ; le même `RestError`
//! irrigue la surface MCP par la conversion `From<RestError> for McpError` (feature
//! `mcp`), qui rend une enveloppe JSON-RPC à la place de ce `IntoResponse`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::resource::HookError;

/// Erreur des handlers REST — cinq variantes, chacune portant soit un code unique
/// `MRD-REST-NNN` (`001` à `004`) dans sa `Display`, soit le `HookError` applicatif
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
            // `500` partagé par bras fusionné (`match_same_arms`) : statut identique, le `to_string()`
            // porte la `Display` propre à la variante (`MRD-REST-003` / `MRD-REST-004`).
            RestError::Database(_) | RestError::Internal(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()).into_response()
            }
            RestError::Application(ref err) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ApplicationErrorBody {
                    code: err.code.as_deref(),
                    message: &err.message,
                }),
            )
                .into_response(),
        }
    }
}
