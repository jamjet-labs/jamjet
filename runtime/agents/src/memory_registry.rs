//! In-memory agent registry — no persistence, suitable for testing and
//! ephemeral sandbox deployments (e.g. Glama).

use crate::card::AgentCard;
use crate::lifecycle::AgentStatus;
use crate::registry::{Agent, AgentFilter, AgentId, AgentRegistry};
use async_trait::async_trait;
use chrono::Utc;
use dashmap::DashMap;
use uuid::Uuid;

pub struct InMemoryAgentRegistry {
    agents: DashMap<AgentId, Agent>,
}

impl InMemoryAgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: DashMap::new(),
        }
    }
}

impl Default for InMemoryAgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentRegistry for InMemoryAgentRegistry {
    async fn register(&self, card: AgentCard) -> Result<AgentId, String> {
        let id = Uuid::new_v4();
        let now = Utc::now();
        let agent = Agent {
            id,
            card,
            status: AgentStatus::Registered,
            registered_at: now,
            updated_at: now,
            last_heartbeat: None,
        };
        self.agents.insert(id, agent);
        Ok(id)
    }

    async fn get(&self, id: AgentId) -> Result<Option<Agent>, String> {
        Ok(self.agents.get(&id).map(|r| r.value().clone()))
    }

    async fn get_by_uri(&self, uri: &str) -> Result<Option<Agent>, String> {
        Ok(self
            .agents
            .iter()
            .find(|r| r.value().card.uri == uri)
            .map(|r| r.value().clone()))
    }

    async fn find(&self, filter: AgentFilter) -> Result<Vec<Agent>, String> {
        let results: Vec<Agent> = self
            .agents
            .iter()
            .filter(|r| {
                let a = r.value();
                if let Some(ref status) = filter.status {
                    if &a.status != status {
                        return false;
                    }
                }
                if let Some(ref skill) = filter.skill {
                    let has_skill = a
                        .card
                        .capabilities
                        .skills
                        .iter()
                        .any(|s| s.name.eq_ignore_ascii_case(skill));
                    if !has_skill {
                        return false;
                    }
                }
                if let Some(ref protocol) = filter.protocol {
                    let has_proto = a
                        .card
                        .capabilities
                        .protocols
                        .iter()
                        .any(|p| p.eq_ignore_ascii_case(protocol));
                    if !has_proto {
                        return false;
                    }
                }
                true
            })
            .map(|r| r.value().clone())
            .collect();
        Ok(results)
    }

    async fn update_status(&self, id: AgentId, status: AgentStatus) -> Result<(), String> {
        match self.agents.get_mut(&id) {
            Some(mut entry) => {
                entry.status = status;
                entry.updated_at = Utc::now();
                Ok(())
            }
            None => Err(format!("agent not found: {id}")),
        }
    }

    async fn heartbeat(&self, id: AgentId) -> Result<(), String> {
        match self.agents.get_mut(&id) {
            Some(mut entry) => {
                entry.last_heartbeat = Some(Utc::now());
                Ok(())
            }
            None => Err(format!("agent not found: {id}")),
        }
    }

    async fn discover_remote(&self, url: &str) -> Result<Agent, String> {
        let card_url = if url.ends_with("/.well-known/agent.json") {
            url.to_string()
        } else {
            format!("{}/.well-known/agent.json", url.trim_end_matches('/'))
        };
        let resp = reqwest::get(&card_url)
            .await
            .map_err(|e| format!("failed to fetch agent card: {e}"))?;
        let card: AgentCard = resp
            .json()
            .await
            .map_err(|e| format!("failed to parse agent card: {e}"))?;
        let id = self.register(card.clone()).await?;
        self.get(id)
            .await?
            .ok_or_else(|| "agent registered but not found".to_string())
    }
}
