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
}

impl McpError {
    /// Code JSON-RPC 2.0 — -32700..-32600 sont réservés au protocole, -32000..-32099 est la
    /// plage libre pour l'application (spec JSON-RPC 2.0).
    pub(crate) fn rpc_code(&self) -> i32 {
        match self {
            McpError::Forbidden => -32001,
            McpError::NotFound => -32002,
            McpError::UnknownTool(_) => -32601, // "Method not found", standard JSON-RPC
            McpError::InvalidParams(_) => -32602, // "Invalid params", standard JSON-RPC
            McpError::Database(_) | McpError::Render(_) => -32603, // "Internal error", standard JSON-RPC
        }
    }
}

impl From<RestError> for McpError {
    fn from(err: RestError) -> Self {
        match err {
            RestError::NotFound => McpError::NotFound,
            RestError::Forbidden => McpError::Forbidden,
            RestError::Database(e) => McpError::Database(e),
        }
    }
}
