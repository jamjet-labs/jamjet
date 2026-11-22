//! Postgres-backed agent registry.
//!
//! Uses the same database as the `jamjet-state` Postgres backend; the
//! `agents` table comes from its migrations. It behaves as the SQLite
//! registry does: one agent per URI, a status update of an unknown id is an
//! error, a heartbeat of one is not. Times are the database's, so agents
//! registered by several engine instances sort by one clock.

use crate::card::AgentCard;
use crate::lifecycle::AgentStatus;
use crate::registry::{Agent, AgentFilter, AgentId, AgentRegistry};
use crate::sqlite_registry::{status_to_str, str_to_status};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};
use tracing::instrument;
use uuid::Uuid;

pub struct PgAgentRegistry {
    pool: PgPool,
}

impl PgAgentRegistry {
    /// A registry on `pool`, which must hold the `jamjet-state` Postgres
    /// schema.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn row_to_agent(row: &PgRow) -> Result<Agent, String> {
    let get_err = |e: sqlx::Error| e.to_string();
    let card: AgentCard =
        serde_json::from_str(row.try_get::<&str, _>("card_json").map_err(get_err)?)
            .map_err(|e| e.to_string())?;
    Ok(Agent {
        id: row.try_get::<Uuid, _>("id").map_err(get_err)?,
        card,
        status: str_to_status(row.try_get::<&str, _>("status").map_err(get_err)?)?,
        registered_at: row
            .try_get::<DateTime<Utc>, _>("registered_at")
            .map_err(get_err)?,
        updated_at: row
            .try_get::<DateTime<Utc>, _>("updated_at")
            .map_err(get_err)?,
        last_heartbeat: row
            .try_get::<Option<DateTime<Utc>>, _>("last_heartbeat")
            .map_err(get_err)?,
    })
}

/// Postgres text cannot hold a NUL: refuse it with a plain message rather
/// than the server's encoding error.
fn no_nul(what: &str, value: &str) -> Result<(), String> {
    if value.contains('\0') {
        return Err(format!("agent {what} contains a NUL character"));
    }
    Ok(())
}

#[async_trait]
impl AgentRegistry for PgAgentRegistry {
    #[instrument(skip(self, card), fields(uri = %card.uri))]
    async fn register(&self, card: AgentCard) -> Result<AgentId, String> {
        no_nul("uri", &card.uri)?;
        let id = Uuid::new_v4();
        let card_json = serde_json::to_string(&card).map_err(|e| e.to_string())?;
        sqlx::query(
            "INSERT INTO agents (id, uri, card_json, status, registered_at, updated_at) \
             VALUES ($1, $2, $3, 'registered', statement_timestamp(), statement_timestamp())",
        )
        .bind(id)
        .bind(&card.uri)
        .bind(&card_json)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(id)
    }

    async fn get(&self, id: AgentId) -> Result<Option<Agent>, String> {
        let row = sqlx::query(
            "SELECT id, card_json, status, registered_at, updated_at, last_heartbeat \
             FROM agents WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        row.map(|r| row_to_agent(&r)).transpose()
    }

    async fn get_by_uri(&self, uri: &str) -> Result<Option<Agent>, String> {
        if uri.contains('\0') {
            return Ok(None);
        }
        let row = sqlx::query(
            "SELECT id, card_json, status, registered_at, updated_at, last_heartbeat \
             FROM agents WHERE uri = $1",
        )
        .bind(uri)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        row.map(|r| row_to_agent(&r)).transpose()
    }

    async fn find(&self, filter: AgentFilter) -> Result<Vec<Agent>, String> {
        // Status in SQL; skill and protocol in memory, as SQLite does.
        let rows = match &filter.status {
            Some(s) => {
                sqlx::query(
                    "SELECT id, card_json, status, registered_at, updated_at, last_heartbeat \
                     FROM agents WHERE status = $1 ORDER BY registered_at DESC, id",
                )
                .bind(status_to_str(s))
                .fetch_all(&self.pool)
                .await
            }
            None => {
                sqlx::query(
                    "SELECT id, card_json, status, registered_at, updated_at, last_heartbeat \
                     FROM agents ORDER BY registered_at DESC, id",
                )
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(|e| e.to_string())?;

        let mut agents: Vec<Agent> = rows.iter().map(row_to_agent).collect::<Result<_, _>>()?;
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
        let rows = sqlx::query(
            "UPDATE agents SET status = $1, updated_at = statement_timestamp() WHERE id = $2",
        )
        .bind(status_to_str(&status))
        .bind(id)
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
        sqlx::query(
            "UPDATE agents SET last_heartbeat = statement_timestamp(), \
             updated_at = statement_timestamp() WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    #[instrument(skip(self), fields(url = url))]
    async fn discover_remote(&self, url: &str) -> Result<Agent, String> {
        // Through the egress guard, as the SQLite registry: public https
        // only, pinned, no redirects, bounded, and a generic error.
        let card = crate::outbound::discover_card(url, crate::outbound::private_allowed())
            .await
            .map_err(|e| e.to_string())?;
        let id = self.register(card).await?;
        self.get(id)
            .await?
            .ok_or_else(|| "agent not found after registration".into())
    }
}
