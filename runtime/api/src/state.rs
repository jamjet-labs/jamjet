use jamjet_a2a_proto::A2aAdapter;
use jamjet_agents::AgentRegistry;
use jamjet_audit::{AuditBackend, AuditEnricher};
use jamjet_mcp::McpAdapter;
use jamjet_protocols::{anp::AnpAdapter, ProtocolRegistry};
use jamjet_state::backend::StateBackend;
use jamjet_state::TenantId;
use std::sync::Arc;

/// Function type for creating tenant-scoped backends.
pub type BackendForFn = Arc<dyn Fn(&TenantId) -> Arc<dyn StateBackend> + Send + Sync>;

/// Shared application state injected into Axum route handlers.
#[derive(Clone)]
pub struct AppState {
    /// The raw, unscoped backend. Used by scheduler/worker (cross-tenant).
    pub backend: Arc<dyn StateBackend>,
    /// Creates a tenant-scoped backend view.
    pub backend_for_fn: BackendForFn,
    pub agents: Arc<dyn AgentRegistry>,
    /// Audit log backend — append-only, immutable.
    pub audit: Arc<dyn AuditBackend>,
    /// Audit enricher — wraps all `append_event` calls with audit metadata.
    pub enricher: Arc<AuditEnricher>,
    /// Protocol adapter registry — pre-loaded with MCP, A2A, and ANP adapters.
    pub protocols: ProtocolRegistry,
}

impl AppState {
    /// Get a tenant-scoped backend for the given tenant.
    pub fn backend_for(&self, tenant_id: &TenantId) -> Arc<dyn StateBackend> {
        (self.backend_for_fn)(tenant_id)
    }
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
    reg.register("anp", Arc::new(AnpAdapter::new()), vec!["did:"]);

    reg
}
