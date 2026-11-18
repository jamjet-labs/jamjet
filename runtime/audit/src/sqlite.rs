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
    raw_event       TEXT NOT NULL,
    tenant_id       TEXT NOT NULL DEFAULT 'default',
    expires_at      TEXT,
    redacted        INTEGER NOT NULL DEFAULT 0,
    prev_hash       TEXT,
    entry_hash      TEXT,
    signature       TEXT
);
CREATE INDEX IF NOT EXISTS idx_audit_execution_id ON audit_log (execution_id);
CREATE INDEX IF NOT EXISTS idx_audit_actor_id     ON audit_log (actor_id);
CREATE INDEX IF NOT EXISTS idx_audit_event_type   ON audit_log (event_type);
CREATE INDEX IF NOT EXISTS idx_audit_created_at   ON audit_log (created_at);
CREATE INDEX IF NOT EXISTS idx_audit_tenant_id    ON audit_log (tenant_id);
CREATE INDEX IF NOT EXISTS idx_audit_expires_at   ON audit_log (expires_at);
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

    /// Purge audit entries whose retention period has expired.
    ///
    /// Returns the number of entries removed.
    pub async fn purge_expired(&self) -> Result<u64, AuditError> {
        let now = chrono::Utc::now().to_rfc3339();
        let result =
            sqlx::query("DELETE FROM audit_log WHERE expires_at IS NOT NULL AND expires_at < ?")
                .bind(&now)
                .execute(&self.pool)
                .await
                .map_err(|e| AuditError::Database(e.to_string()))?;

        Ok(result.rows_affected())
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
        // Forward-migrate audit_log tables created before the tamper-evidence
        // columns existed (the CREATE above is `IF NOT EXISTS`, so a pre-existing
        // table is left untouched). Adding a column that is already present is a
        // benign "duplicate column name" error we deliberately ignore.
        for col in ["prev_hash", "entry_hash", "signature"] {
            let sql = format!("ALTER TABLE audit_log ADD COLUMN {col} TEXT");
            if let Err(e) = sqlx::query(&sql).execute(&self.pool).await {
                if !e.to_string().to_lowercase().contains("duplicate column") {
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl AuditBackend for SqliteAuditBackend {
    async fn append(&self, entry: AuditLogEntry) -> Result<(), AuditError> {
        let raw = serde_json::to_string(&entry.raw_event)
            .map_err(|e| AuditError::Serialization(e.to_string()))?;

        let expires_at = entry.expires_at.map(|dt| dt.to_rfc3339());

        sqlx::query(
            r#"
            INSERT OR IGNORE INTO audit_log
                (id, event_id, execution_id, sequence, event_type,
                 actor_id, actor_type, tool_call_hash, policy_decision,
                 http_request_id, http_method, http_path, ip_address,
                 created_at, raw_event, tenant_id, expires_at, redacted,
                 prev_hash, entry_hash, signature)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
        .bind(&entry.tenant_id)
        .bind(expires_at.as_deref())
        .bind(entry.redacted as i32)
        .bind(&entry.prev_hash)
        .bind(&entry.entry_hash)
        .bind(&entry.signature)
        .execute(&self.pool)
        .await
        .map_err(|e| AuditError::Database(e.to_string()))?;

        Ok(())
    }

    async fn query(&self, q: &AuditQuery) -> Result<Vec<AuditLogEntry>, AuditError> {
        let (where_clause, values) = where_clause(q);
        let sql = format!(
            "SELECT * FROM audit_log WHERE {where_clause} ORDER BY created_at DESC LIMIT {} OFFSET {}",
            q.limit, q.offset
        );

        let mut query = sqlx::query_as::<_, AuditRow>(&sql);
        for value in &values {
            query = query.bind(value.as_str());
        }
        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AuditError::Database(e.to_string()))?;

        rows.into_iter().map(row_to_entry).collect()
    }

    async fn count(&self, q: &AuditQuery) -> Result<u64, AuditError> {
        let (where_clause, values) = where_clause(q);
        let sql = format!("SELECT COUNT(*) FROM audit_log WHERE {where_clause}");

        let mut query = sqlx::query_as::<_, (i64,)>(&sql);
        for value in &values {
            query = query.bind(value.as_str());
        }
        let count = query
            .fetch_one(&self.pool)
            .await
            .map_err(|e| AuditError::Database(e.to_string()))?;

        Ok(count.0 as u64)
    }
}

/// The WHERE clause for `q`, with a `?` for each filter value, and the values
/// to bind in placeholder order.
///
/// The values are bound, never written into the SQL text: `GET /audit` fills
/// `q` from request query parameters, and a spliced value with a quote in it
/// could end the string literal and rewrite the query. `limit` and `offset`
/// are `u32`, so they stay in the statement text.
fn where_clause(q: &AuditQuery) -> (String, Vec<String>) {
    let mut wheres = vec!["1=1"];
    let mut values = Vec::new();
    if let Some(from) = &q.from {
        wheres.push("created_at >= ?");
        values.push(from.to_rfc3339());
    }
    if let Some(to) = &q.to {
        wheres.push("created_at <= ?");
        values.push(to.to_rfc3339());
    }
    if let Some(actor) = &q.actor_id {
        wheres.push("actor_id = ?");
        values.push(actor.clone());
    }
    if let Some(event_type) = &q.event_type {
        wheres.push("event_type = ?");
        values.push(event_type.clone());
    }
    if let Some(exec_id) = &q.execution_id {
        wheres.push("execution_id = ?");
        values.push(exec_id.clone());
    }
    if let Some(tid) = &q.tenant_id {
        wheres.push("tenant_id = ?");
        values.push(tid.clone());
    }
    (wheres.join(" AND "), values)
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
    tenant_id: String,
    expires_at: Option<String>,
    redacted: i32,
    prev_hash: Option<String>,
    entry_hash: Option<String>,
    signature: Option<String>,
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
        // Strict: a stored actor_type that is not one of the enum's values
        // was not written by the engine. Reading it as `System` (as before)
        // let a row whose actor_type text had been altered verify as system.
        actor_type: parse_actor_type(&row.actor_type).ok_or_else(|| {
            AuditError::Serialization(format!(
                "audit entry {} has an unknown actor_type {:?}",
                row.id, row.actor_type
            ))
        })?,
        tool_call_hash: row.tool_call_hash,
        policy_decision: row.policy_decision,
        http_request_id: row.http_request_id,
        http_method: row.http_method,
        http_path: row.http_path,
        ip_address: row.ip_address,
        created_at,
        raw_event,
        tenant_id: row.tenant_id,
        expires_at: row
            .expires_at
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc)),
        redacted: row.redacted != 0,
        prev_hash: row.prev_hash,
        entry_hash: row.entry_hash,
        signature: row.signature,
    })
}

fn actor_type_str(a: &ActorType) -> &'static str {
    match a {
        ActorType::Human => "human",
        ActorType::Agent => "agent",
        ActorType::System => "system",
    }
}

fn parse_actor_type(s: &str) -> Option<ActorType> {
    match s {
        "human" => Some(ActorType::Human),
        "agent" => Some(ActorType::Agent),
        "system" => Some(ActorType::System),
        _ => None,
    }
}
