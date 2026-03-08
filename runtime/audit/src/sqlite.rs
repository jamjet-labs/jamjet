//! SQLite-backed audit log backend.
//!
//! IMPORTANT: This implementation NEVER issues UPDATE or DELETE SQL against
//! `audit_log`. The table is append-only by design.

use crate::backend::{AuditBackend, AuditError, AuditQuery};
use crate::entry::{ActorType, AuditLogEntry};
use async_trait::async_trait;
use sqlx::SqlitePool;
use uuid::Uuid;

/// SQL DDL for the audit log table.
///
/// Called at server startup — uses `IF NOT EXISTS` so it is safe to run
/// repeatedly (no migrations needed for the initial schema).
pub const AUDIT_LOG_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS audit_log (
    id              TEXT PRIMARY KEY,
    event_id        TEXT NOT NULL,
    execution_id    TEXT NOT NULL,
    sequence        INTEGER NOT NULL,
    event_type      TEXT NOT NULL,
    actor_id        TEXT NOT NULL,
    actor_type      TEXT NOT NULL,
    tool_call_hash  TEXT,
    policy_decision TEXT,
    http_request_id TEXT,
    http_method     TEXT,
    http_path       TEXT,
    ip_address      TEXT,
    created_at      TEXT NOT NULL,
    raw_event       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_audit_execution_id ON audit_log (execution_id);
CREATE INDEX IF NOT EXISTS idx_audit_actor_id     ON audit_log (actor_id);
CREATE INDEX IF NOT EXISTS idx_audit_event_type   ON audit_log (event_type);
CREATE INDEX IF NOT EXISTS idx_audit_created_at   ON audit_log (created_at);
"#;

pub struct SqliteAuditBackend {
    pool: SqlitePool,
}

impl SqliteAuditBackend {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Open a new pool from a database URL and return a backend.
    pub async fn open(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = SqlitePool::connect(database_url).await?;
        Ok(Self { pool })
    }

    /// Apply the audit log DDL. Call once at server startup.
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        for stmt in AUDIT_LOG_DDL.split(';') {
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            sqlx::query(stmt).execute(&self.pool).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl AuditBackend for SqliteAuditBackend {
    async fn append(&self, entry: AuditLogEntry) -> Result<(), AuditError> {
        let raw = serde_json::to_string(&entry.raw_event)
            .map_err(|e| AuditError::Serialization(e.to_string()))?;

        sqlx::query(
            r#"
            INSERT OR IGNORE INTO audit_log
                (id, event_id, execution_id, sequence, event_type,
                 actor_id, actor_type, tool_call_hash, policy_decision,
                 http_request_id, http_method, http_path, ip_address,
                 created_at, raw_event)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(entry.id.to_string())
        .bind(entry.event_id.to_string())
        .bind(&entry.execution_id)
        .bind(entry.sequence)
        .bind(&entry.event_type)
        .bind(&entry.actor_id)
        .bind(actor_type_str(&entry.actor_type))
        .bind(&entry.tool_call_hash)
        .bind(&entry.policy_decision)
        .bind(&entry.http_request_id)
        .bind(&entry.http_method)
        .bind(&entry.http_path)
        .bind(&entry.ip_address)
        .bind(entry.created_at.to_rfc3339())
        .bind(raw)
        .execute(&self.pool)
        .await
        .map_err(|e| AuditError::Database(e.to_string()))?;

        Ok(())
    }

    async fn query(&self, q: &AuditQuery) -> Result<Vec<AuditLogEntry>, AuditError> {
        // Build a dynamic WHERE clause.
        let conditions: Vec<&str> = vec!["1=1"];
        // Note: SQLite doesn't support true dynamic binding like diesel/sea-orm,
        // so we use a simple approach: pre-filter with the fields we have.
        // Full dynamic query building would require a query builder; here we keep
        // it simple with optional clauses materialized in the WHERE string.
        let from_str;
        let to_str;

        let mut wheres = vec!["1=1".to_string()];
        if let Some(from) = &q.from {
            from_str = from.to_rfc3339();
            wheres.push(format!("created_at >= '{from_str}'"));
        }
        if let Some(to) = &q.to {
            to_str = to.to_rfc3339();
            wheres.push(format!("created_at <= '{to_str}'"));
        }
        if let Some(actor) = &q.actor_id {
            wheres.push(format!("actor_id = '{actor}'"));
        }
        if let Some(event_type) = &q.event_type {
            wheres.push(format!("event_type = '{event_type}'"));
        }
        if let Some(exec_id) = &q.execution_id {
            wheres.push(format!("execution_id = '{exec_id}'"));
        }

        let _ = conditions; // suppress unused warning
        let where_clause = wheres.join(" AND ");
        let sql = format!(
            "SELECT * FROM audit_log WHERE {where_clause} ORDER BY created_at DESC LIMIT {} OFFSET {}",
            q.limit, q.offset
        );

        let rows = sqlx::query_as::<_, AuditRow>(&sql)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AuditError::Database(e.to_string()))?;

        rows.into_iter().map(row_to_entry).collect()
    }

    async fn count(&self, q: &AuditQuery) -> Result<u64, AuditError> {
        let mut wheres = vec!["1=1".to_string()];
        if let Some(from) = &q.from {
            wheres.push(format!("created_at >= '{}'", from.to_rfc3339()));
        }
        if let Some(to) = &q.to {
            wheres.push(format!("created_at <= '{}'", to.to_rfc3339()));
        }
        if let Some(actor) = &q.actor_id {
            wheres.push(format!("actor_id = '{actor}'"));
        }
        if let Some(event_type) = &q.event_type {
            wheres.push(format!("event_type = '{event_type}'"));
        }
        if let Some(exec_id) = &q.execution_id {
            wheres.push(format!("execution_id = '{exec_id}'"));
        }

        let where_clause = wheres.join(" AND ");
        let sql = format!("SELECT COUNT(*) FROM audit_log WHERE {where_clause}");

        let count: (i64,) = sqlx::query_as(&sql)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| AuditError::Database(e.to_string()))?;

        Ok(count.0 as u64)
    }
}

// ── Internal row type ─────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct AuditRow {
    id: String,
    event_id: String,
    execution_id: String,
    sequence: i64,
    event_type: String,
    actor_id: String,
    actor_type: String,
    tool_call_hash: Option<String>,
    policy_decision: Option<String>,
    http_request_id: Option<String>,
    http_method: Option<String>,
    http_path: Option<String>,
    ip_address: Option<String>,
    created_at: String,
    raw_event: String,
}

fn row_to_entry(row: AuditRow) -> Result<AuditLogEntry, AuditError> {
    let raw_event: serde_json::Value = serde_json::from_str(&row.raw_event)
        .map_err(|e| AuditError::Serialization(e.to_string()))?;

    let created_at = chrono::DateTime::parse_from_rfc3339(&row.created_at)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| AuditError::Serialization(e.to_string()))?;

    Ok(AuditLogEntry {
        id: Uuid::parse_str(&row.id).map_err(|e| AuditError::Serialization(e.to_string()))?,
        event_id: Uuid::parse_str(&row.event_id)
            .map_err(|e| AuditError::Serialization(e.to_string()))?,
        execution_id: row.execution_id,
        sequence: row.sequence,
        event_type: row.event_type,
        actor_id: row.actor_id,
        actor_type: parse_actor_type(&row.actor_type),
        tool_call_hash: row.tool_call_hash,
        policy_decision: row.policy_decision,
        http_request_id: row.http_request_id,
        http_method: row.http_method,
        http_path: row.http_path,
        ip_address: row.ip_address,
        created_at,
        raw_event,
    })
}

fn actor_type_str(a: &ActorType) -> &'static str {
    match a {
        ActorType::Human => "human",
        ActorType::Agent => "agent",
        ActorType::System => "system",
    }
}

fn parse_actor_type(s: &str) -> ActorType {
    match s {
        "human" => ActorType::Human,
        "agent" => ActorType::Agent,
        _ => ActorType::System,
    }
}
