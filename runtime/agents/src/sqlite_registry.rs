//! SQLite-backed agent registry.
//!
//! Uses the same database as the `jamjet-state` SQLite backend.
//! The `agents` table is created by the `jamjet-state` migration.

use crate::card::AgentCard;
use crate::lifecycle::AgentStatus;
use crate::registry::{Agent, AgentFilter, AgentId, AgentRegistry};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};
use tracing::instrument;
use uuid::Uuid;

pub struct SqliteAgentRegistry {
    pool: SqlitePool,
}

impl SqliteAgentRegistry {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Connect using an existing database URL (shared with state backend).
    pub async fn connect(database_url: &str) -> Result<Self, sqlx::Error> {
        use sqlx::sqlite::SqliteConnectOptions;
        use std::str::FromStr;
        let opts = SqliteConnectOptions::from_str(database_url)?.create_if_missing(true);
        let pool = SqlitePool::connect_with(opts).await?;
        Ok(Self { pool })
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn status_to_str(s: &AgentStatus) -> &'static str {
    match s {
        AgentStatus::Registered => "registered",
        AgentStatus::Active => "active",
        AgentStatus::Paused => "paused",
        AgentStatus::Deactivated => "deactivated",
        AgentStatus::Archived => "archived",
    }
}

fn str_to_status(s: &str) -> Result<AgentStatus, String> {
    match s {
        "registered" => Ok(AgentStatus::Registered),
        "active" => Ok(AgentStatus::Active),
        "paused" => Ok(AgentStatus::Paused),
        "deactivated" => Ok(AgentStatus::Deactivated),
        "archived" => Ok(AgentStatus::Archived),
        other => Err(format!("unknown agent status: {other}")),
    }
}

fn parse_dt(s: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| format!("bad datetime: {e}"))
}

fn row_to_agent(row: &sqlx::sqlite::SqliteRow) -> Result<Agent, String> {
    let id = Uuid::parse_str(row.try_get::<&str, _>("id").map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let card: AgentCard = serde_json::from_str(
        row.try_get::<&str, _>("card_json")
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let status = str_to_status(
        row.try_get::<&str, _>("status")
            .map_err(|e| e.to_string())?,
    )?;
    let registered_at = parse_dt(
        row.try_get::<&str, _>("registered_at")
            .map_err(|e| e.to_string())?,
    )?;
    let updated_at = parse_dt(
        row.try_get::<&str, _>("updated_at")
            .map_err(|e| e.to_string())?,
    )?;
    let last_heartbeat: Option<DateTime<Utc>> = row
        .try_get::<Option<&str>, _>("last_heartbeat")
        .map_err(|e| e.to_string())?
        .map(parse_dt)
        .transpose()?;

    Ok(Agent {
        id,
        card,
        status,
        registered_at,
        updated_at,
        last_heartbeat,
    })
}

// ── AgentRegistry impl ────────────────────────────────────────────────────────

#[async_trait]
impl AgentRegistry for SqliteAgentRegistry {
    #[instrument(skip(self, card), fields(uri = %card.uri))]
    async fn register(&self, card: AgentCard) -> Result<AgentId, String> {
        let id = Uuid::new_v4();
        let card_json = serde_json::to_string(&card).map_err(|e| e.to_string())?;
        let now = Utc::now().to_rfc3339();

        sqlx::query(
            "INSERT INTO agents (id, uri, card_json, status, registered_at, updated_at) VALUES (?, ?, ?, 'registered', ?, ?)",
        )
        .bind(id.to_string())
        .bind(&card.uri)
        .bind(&card_json)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;

        Ok(id)
    }

    async fn get(&self, id: AgentId) -> Result<Option<Agent>, String> {
        let id_str = id.to_string();
        let row = sqlx::query("SELECT * FROM agents WHERE id = ?")
            .bind(&id_str)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| e.to_string())?;

        row.map(|r| row_to_agent(&r)).transpose()
    }

    async fn get_by_uri(&self, uri: &str) -> Result<Option<Agent>, String> {
        let row = sqlx::query("SELECT * FROM agents WHERE uri = ?")
            .bind(uri)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| e.to_string())?;

        row.map(|r| row_to_agent(&r)).transpose()
    }

    async fn find(&self, filter: AgentFilter) -> Result<Vec<Agent>, String> {
        // Build query dynamically based on filter fields.
        // For Phase 1, status filter is enough; skill/protocol matching is in-memory.
        let rows = match &filter.status {
            Some(s) => {
                sqlx::query("SELECT * FROM agents WHERE status = ? ORDER BY registered_at DESC")
                    .bind(status_to_str(s))
                    .fetch_all(&self.pool)
                    .await
                    .map_err(|e| e.to_string())?
            }
            None => sqlx::query("SELECT * FROM agents ORDER BY registered_at DESC")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| e.to_string())?,
        };

        let mut agents: Vec<Agent> = rows.iter().map(row_to_agent).collect::<Result<_, _>>()?;

        // Post-filter by skill and protocol.
        if let Some(skill) = &filter.skill {
            agents.retain(|a| a.card.capabilities.skills.iter().any(|s| &s.name == skill));
        }
        if let Some(protocol) = &filter.protocol {
            agents.retain(|a| a.card.capabilities.protocols.contains(protocol));
        }

        Ok(agents)
    }

    #[instrument(skip(self), fields(agent_id = %id))]
    async fn update_status(&self, id: AgentId, status: AgentStatus) -> Result<(), String> {
        let id_str = id.to_string();
        let status_str = status_to_str(&status);
        let now = Utc::now().to_rfc3339();

        let rows = sqlx::query("UPDATE agents SET status = ?, updated_at = ? WHERE id = ?")
            .bind(status_str)
            .bind(&now)
            .bind(&id_str)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?
            .rows_affected();

        if rows == 0 {
            return Err(format!("agent {id} not found"));
        }
        Ok(())
    }

    #[instrument(skip(self), fields(agent_id = %id))]
    async fn heartbeat(&self, id: AgentId) -> Result<(), String> {
        let id_str = id.to_string();
        let now = Utc::now().to_rfc3339();

        sqlx::query("UPDATE agents SET last_heartbeat = ?, updated_at = ? WHERE id = ?")
            .bind(&now)
            .bind(&now)
            .bind(&id_str)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;

        Ok(())
    }

    #[instrument(skip(self), fields(url = url))]
    async fn discover_remote(&self, url: &str) -> Result<Agent, String> {
        let agent_card_url = format!("{url}/.well-known/agent.json");
        let card: AgentCard = reqwest::Client::new()
            .get(&agent_card_url)
            .send()
            .await
            .map_err(|e| format!("fetch Agent Card: {e}"))?
            .json()
            .await
            .map_err(|e| format!("parse Agent Card: {e}"))?;

        let id = self.register(card).await?;
        self.get(id)
            .await?
            .ok_or_else(|| "agent not found after registration".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{AgentCapabilities, AuthSpec, AutonomyLevel};

    async fn open_test_registry() -> SqliteAgentRegistry {
        // Use the state migration to create the agents table.
        let backend = jamjet_state::SqliteBackend::open("sqlite::memory:")
            .await
            .expect("failed to open in-memory db");
        // Re-use the pool from SqliteBackend by connecting to the same in-memory URL.
        // In-memory SQLite is connection-scoped, so we need a fresh connection here
        // that shares the schema from migrations.
        // For tests, connect to a named in-memory DB.
        let pool = SqlitePool::connect("sqlite::memory:").await.expect("pool");
        // Run migrations manually.
        sqlx::migrate!("../state/migrations")
            .run(&pool)
            .await
            .expect("migrations");
        SqliteAgentRegistry { pool }
    }

    fn sample_card(uri: &str) -> AgentCard {
        AgentCard {
            id: uuid::Uuid::new_v4().to_string(),
            uri: uri.to_string(),
            name: "Test Agent".into(),
            description: "A test agent".into(),
            version: "1.0.0".into(),
            capabilities: AgentCapabilities {
                skills: vec![],
                protocols: vec!["mcp_client".into()],
                tools_provided: vec![],
                tools_consumed: vec![],
            },
            autonomy: AutonomyLevel::Guided,
            constraints: None,
            auth: AuthSpec::None,
            latency_class: None,
            cost_class: None,
            reasoning_modes: vec![],
            labels: Default::default(),
        }
    }

    #[tokio::test]
    async fn test_register_and_get() {
        let reg = open_test_registry().await;
        let card = sample_card("jamjet://test/agent1");
        let id = reg.register(card.clone()).await.unwrap();
        let agent = reg.get(id).await.unwrap().unwrap();
        assert_eq!(agent.card.uri, "jamjet://test/agent1");
        assert_eq!(agent.status, AgentStatus::Registered);
    }

    #[tokio::test]
    async fn test_status_transition() {
        let reg = open_test_registry().await;
        let id = reg
            .register(sample_card("jamjet://test/agent2"))
            .await
            .unwrap();
        reg.update_status(id, AgentStatus::Active).await.unwrap();
        let agent = reg.get(id).await.unwrap().unwrap();
        assert_eq!(agent.status, AgentStatus::Active);
    }

    #[tokio::test]
    async fn test_find_by_status() {
        let reg = open_test_registry().await;
        let id1 = reg.register(sample_card("jamjet://test/a3")).await.unwrap();
        let _id2 = reg.register(sample_card("jamjet://test/a4")).await.unwrap();
        reg.update_status(id1, AgentStatus::Active).await.unwrap();

        let active = reg
            .find(AgentFilter {
                status: Some(AgentStatus::Active),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].status, AgentStatus::Active);
    }
}
