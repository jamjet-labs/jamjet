use jamjet_agents::AgentRegistry;
use jamjet_state::backend::StateBackend;
use std::sync::Arc;

/// Shared application state injected into Axum route handlers.
#[derive(Clone)]
pub struct AppState {
    pub backend: Arc<dyn StateBackend>,
    pub agents: Arc<dyn AgentRegistry>,
}
