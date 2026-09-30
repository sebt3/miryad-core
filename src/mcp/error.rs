use crate::resource::HookError;
use crate::rest::error::RestError;

/// Erreur de la surface MCP — chaque variante porte, outre sa `Display` préfixée d'un
/// code unique `MRD-MCP-NNN`, le code numérique JSON-RPC 2.0 rendu par `rpc_code` et,
/// pour le seul rejet de hook, le code libre du `HookError` dans le champ `data`. Le
/// fichier ne construit lui-même aucune erreur : les variantes naissent dans le
/// registre MCP, le rendu de sortie, ou la conversion `From<RestError>`.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// `MRD-MCP-001` — refus RBAC, miroir du `403` REST, code JSON-RPC `-32001`.
    #[error("MRD-MCP-001: forbidden")]
    Forbidden,
    /// `MRD-MCP-002` — ressource absente, miroir du `404` REST, code JSON-RPC `-32002`.
    #[error("MRD-MCP-002: resource not found")]
    NotFound,
    /// `MRD-MCP-003` — `DbErr` de `sea-orm` propagé (`#[from]`, seul `From` dérivé),
    /// code JSON-RPC `-32603`.
    #[error("MRD-MCP-003: database error: {0}")]
    Database(#[from] sea_orm::DbErr),
    /// `MRD-MCP-004` — échec de mise en forme de la sortie (rendu `Handlebars` ou
    /// sérialisation `serde_json`), propre à MCP, code JSON-RPC `-32603`.
    #[error("MRD-MCP-004: template render error: {0}")]
    Render(String),
    /// `MRD-MCP-005` — désérialisation ratée des arguments d'un tool (message `serde`
    /// verbatim), code JSON-RPC `-32602`.
    #[error("MRD-MCP-005: invalid params: {0}")]
    InvalidParams(String),
    /// `MRD-MCP-006` — nom d'outil inconnu du dispatch, code JSON-RPC `-32601`.
    #[error("MRD-MCP-006: unknown tool: {0}")]
    UnknownTool(String),
    /// Miroir de `RestError::Internal` — cf. le commentaire de ce variant pour le contexte
    /// (conversion explicite d'`AuthError`, pas de `From` générique trompeur).
    #[error("MRD-MCP-007: internal error: {0}")]
    Internal(String),
    /// Erreur métier applicative (hook) — jamais un `MRD-*`, cf. `HookError`.
    #[error("{}", .0.message)]
    Application(HookError),
}

impl McpError {
    /// Code JSON-RPC 2.0 — -32700..-32600 sont réservés au protocole, -32000..-32099 est la
    /// plage libre pour l'application (spec JSON-RPC 2.0).
    pub(crate) fn rpc_code(&self) -> i32 {
        match self {
            McpError::Application(_) => -32000,
            McpError::Forbidden => -32001,
            McpError::NotFound => -32002,
            McpError::UnknownTool(_) => -32601, // "Method not found", standard JSON-RPC
            McpError::InvalidParams(_) => -32602, // "Invalid params", standard JSON-RPC
            McpError::Database(_) | McpError::Render(_) | McpError::Internal(_) => -32603, // "Internal error", standard JSON-RPC
        }
    }

    /// Code d'erreur applicatif libre (`HookError::code`), porté dans le champ JSON-RPC `data` —
    /// jamais mélangé au code numérique JSON-RPC lui-même, qui reste dans la plage standard.
    pub(crate) fn data(&self) -> Option<serde_json::Value> {
        match self {
            McpError::Application(err) => err.code.as_ref().map(|code| serde_json::json!({ "code": code })),
            _ => None,
        }
    }
}

impl From<RestError> for McpError {
    fn from(err: RestError) -> Self {
        match err {
            RestError::NotFound => McpError::NotFound,
            RestError::Forbidden => McpError::Forbidden,
            RestError::Database(e) => McpError::Database(e),
            RestError::Application(e) => McpError::Application(e),
            RestError::Internal(msg) => McpError::Internal(msg),
            // Arbitré 2026-09-29 (`mcp/error.sdd`) : l'entrée invalide de la crate est une
            // entrée invalide du client en JSON-RPC — charge utile `String` déplacée intacte.
            RestError::InvalidInput(msg) => McpError::InvalidParams(msg),
        }
    }
}
