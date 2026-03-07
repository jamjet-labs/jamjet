use crate::card::AgentCard;
use crate::lifecycle::AgentStatus;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type AgentId = Uuid;

/// A registered agent with its card, status, and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: AgentId,
    pub card: AgentCard,
    pub status: AgentStatus,
    pub registered_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_heartbeat: Option<DateTime<Utc>>,
}

/// Filter for agent discovery queries.
#[derive(Debug, Default)]
pub struct AgentFilter {
    pub skill: Option<String>,
    pub protocol: Option<String>,
    pub status: Option<AgentStatus>,
}

#[async_trait]
pub trait AgentRegistry: Send + Sync {
    /// Register a new agent with the given Agent Card.
    async fn register(&self, card: AgentCard) -> Result<AgentId, String>;

    /// Get an agent by its internal id.
    async fn get(&self, id: AgentId) -> Result<Option<Agent>, String>;

    /// Get an agent by its URI (e.g. "jamjet://myorg/research-analyst").
    async fn get_by_uri(&self, uri: &str) -> Result<Option<Agent>, String>;

    /// Find agents matching a filter (by skill, protocol, status).
    async fn find(&self, filter: AgentFilter) -> Result<Vec<Agent>, String>;

    /// Update an agent's status.
    async fn update_status(&self, id: AgentId, status: AgentStatus) -> Result<(), String>;

    /// Record a heartbeat for an active agent.
    async fn heartbeat(&self, id: AgentId) -> Result<(), String>;

    /// Discover a remote A2A agent by fetching its Agent Card from a URL.
    /// Stores the agent in the registry as an external agent.
    async fn discover_remote(&self, url: &str) -> Result<Agent, String>;
}
