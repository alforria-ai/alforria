//! MCP client — port of `mcp/` (spec M7.6).
pub mod auth;
pub mod catalog;
pub mod client;
pub mod oauth;
pub mod service;
pub mod transport;

pub use auth::{McpAuth, McpAuthEntry, McpClientInfo, McpTokens};
pub use catalog::{sanitize, tool_name, McpToolDef};
pub use client::{HttpHandle, McpClient, Transport};
pub use oauth::{redirect_url, McpBrowser, OAuthCallbackServer, OAuthConfig, SystemBrowser};
pub use service::{
    AuthStatus, ClientItem, FinishAuthError, McpService, McpServiceInput, McpTool, NotFoundError,
    ServerInstructions, StartAuthError,
};
pub use transport::{
    path_to_file_url, McpError, StdioTransport, LATEST_PROTOCOL_VERSION,
    SUPPORTED_PROTOCOL_VERSIONS,
};
