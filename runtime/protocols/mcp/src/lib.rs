//! JamJet MCP Client and Server
//!
//! Implements the Model Context Protocol as both client and server.
//!
//! Client: connect to external MCP servers, discover tools, invoke tools.
//! Server: expose agent tools/resources to external MCP clients.

pub mod adapter;
pub mod client;
pub mod pool;
pub mod server;
#[cfg(test)]
mod tests;
pub mod transport;
pub mod types;

pub use adapter::McpAdapter;
pub use client::McpClient;
pub use pool::McpClientPool;
pub use server::{McpServer, McpServerConfig, RegisteredTool};
pub use transport::{HttpSseTransport, McpTransport, StdioTransport};
