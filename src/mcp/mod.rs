//! Serveur MCP — 5 tools CRUD par entité, sortie JSON/YAML/Markdown.
//!
//! Nécessite la feature `mcp`. Voir [`McpToolRegistry`](crate::mcp::McpToolRegistry) et [`mcp_router`](crate::mcp::mcp_router).

mod error;
mod format;
mod handler;
mod protocol;
mod registry;

pub use error::McpError;
pub use format::OutputFormat;
pub use handler::mcp_router;
pub use registry::McpToolRegistry;
