use crate::resource::HookError;
use crate::rest::error::RestError;

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("MRD-MCP-001: forbidden")]
    Forbidden,
    #[error("MRD-MCP-002: resource not found")]
    NotFound,
    #[error("MRD-MCP-003: database error: {0}")]
    Database(#[from] sea_orm::DbErr),
    #[error("MRD-MCP-004: template render error: {0}")]
    Render(String),
    #[error("MRD-MCP-005: invalid params: {0}")]
    InvalidParams(String),
    #[error("MRD-MCP-006: unknown tool: {0}")]
    UnknownTool(String),
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
            McpError::Database(_) | McpError::Render(_) => -32603, // "Internal error", standard JSON-RPC
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
        }
    }
}
