use jamjet_agents::AgentRegistry;
use jamjet_mcp::McpAdapter;
use jamjet_a2a::A2aAdapter;
use jamjet_protocols::{anp::AnpAdapter, ProtocolRegistry};
use jamjet_state::backend::StateBackend;
use std::sync::Arc;

/// Shared application state injected into Axum route handlers.
#[derive(Clone)]
pub struct AppState {
    pub backend: Arc<dyn StateBackend>,
    pub agents: Arc<dyn AgentRegistry>,
    /// Protocol adapter registry — pre-loaded with MCP, A2A, and ANP adapters.
    pub protocols: ProtocolRegistry,
}

/// Build a `ProtocolRegistry` pre-loaded with the built-in adapters.
///
/// Registered adapters and their URL-prefix bindings:
/// - `"mcp"`  → `McpAdapter`  — matches `http://`, `https://` (lowest priority)
/// - `"a2a"`  → `A2aAdapter`  — matches `https://` (overrides generic for a2a paths)
/// - `"anp"`  → `AnpAdapter`  — matches `did:` prefixes
///
/// Callers may call `registry.register(...)` after this to add custom adapters
/// or override built-in ones.
pub fn default_protocol_registry() -> ProtocolRegistry {
    let mut reg = ProtocolRegistry::new();

    reg.register(
        "mcp",
        Arc::new(McpAdapter::new()),
        vec!["http://", "https://"],
    );
    reg.register(
        "a2a",
        Arc::new(A2aAdapter::new()),
        // A2A agents typically live at paths containing /a2a or /.well-known/agent.json
        vec![] as Vec<String>,
    );
    reg.register(
        "anp",
        Arc::new(AnpAdapter::new()),
        vec!["did:"],
    );

    reg
}
