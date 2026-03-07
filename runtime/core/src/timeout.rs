use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Timeout configuration for a workflow execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutConfig {
    /// Maximum time a single node may run before being killed and failed.
    pub node_timeout: Option<Duration>,
    /// Maximum total time a workflow execution may run.
    pub workflow_timeout: Option<Duration>,
    /// How often a worker must renew its lease heartbeat.
    /// If a worker misses this, the lease is reclaimed and the node is re-queued.
    pub heartbeat_interval: Duration,
    /// Maximum time a human_approval node waits before routing to fallback or failing.
    pub approval_timeout: Option<Duration>,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            node_timeout: Some(Duration::from_secs(300)), // 5 min
            workflow_timeout: None,
            heartbeat_interval: Duration::from_secs(30),
            approval_timeout: None,
        }
    }
}
