//! PostgreSQL-backed `MessageStore` implementation.
//!
//! Uses native Postgres types: `UUID` for ids, `TIMESTAMPTZ` for timestamps,
//! `JSONB` for metadata. Placeholder syntax uses `$1, $2, …`.

use crate::message::{ChatMessage, MessageId, MessageStore};
use crate::scope::Scope;
use crate::store::MemoryError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

/// DDL statements for the Postgres messages table. Each element is a single
/// statement to be executed independently.
const PG_MESSAGE_STORE_DDL: &[&str] = &[
    r#"
    CREATE TABLE IF NOT EXISTS messages (
        id              UUID PRIMARY KEY,
        conversation_id TEXT NOT NULL,
        role            TEXT NOT NULL,
        content         TEXT NOT NULL,
        org_id          TEXT NOT NULL DEFAULT 'default',
        user_id         TEXT,
        seq             INTEGER NOT NULL,
        created_at      TIMESTAMPTZ NOT NULL,
        metadata        JSONB NOT NULL DEFAULT 'null'
    )
    "#,
    "CREATE INDEX IF NOT EXISTS idx_pg_messages_conversation ON messages (conversation_id, seq)",
    "CREATE INDEX IF NOT EXISTS idx_pg_messages_org_user ON messages (org_id, user_id)",
];

// ---------------------------------------------------------------------------
// PostgresMessageStore
// ---------------------------------------------------------------------------

pub struct PostgresMessageStore {
    pool: PgPool,
}

impl PostgresMessageStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Open a connection pool from a database URL and return a store.
    pub async fn open(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPool::connect(database_url).await?;
        Ok(Self { pool })
    }

    /// Apply the DDL. Safe to call multiple times (uses `IF NOT EXISTS`).
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        for stmt in PG_MESSAGE_STORE_DDL {
            sqlx::query(stmt).execute(&self.pool).await?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal row type
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct MessageRow {
    id: Uuid,
    conversation_id: String,
    role: String,
    content: String,
    org_id: String,
    user_id: Option<String>,
    seq: i32,
    created_at: DateTime<Utc>,
    metadata: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn row_to_message(row: MessageRow) -> Result<ChatMessage, MemoryError> {
    let metadata: serde_json::Map<String, serde_json::Value> = match &row.metadata {
        serde_json::Value::Null => serde_json::Map::new(),
        serde_json::Value::Object(map) => map.clone(),
        other => serde_json::from_value(other.clone())
            .map_err(|e| MemoryError::Serialization(e.to_string()))?,
    };

    Ok(ChatMessage {
        id: row.id,
        conversation_id: row.conversation_id,
        role: row.role,
        content: row.content,
        scope: Scope {
            org_id: row.org_id,
            agent_id: None,
            user_id: row.user_id,
            session_id: None,
        },
        seq: row.seq,
        created_at: row.created_at,
        metadata,
    })
}

// ---------------------------------------------------------------------------
// MessageStore implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl MessageStore for PostgresMessageStore {
    async fn save_messages(
        &self,
        conversation_id: &str,
        messages: &[ChatMessage],
        scope: &Scope,
    ) -> Result<Vec<MessageId>, MemoryError> {
        let mut ids = Vec::with_capacity(messages.len());

        for msg in messages {
            let metadata_json = if msg.metadata.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::to_value(&msg.metadata)
                    .map_err(|e| MemoryError::Serialization(e.to_string()))?
            };

            sqlx::query(
                r#"
                INSERT INTO messages
                    (id, conversation_id, role, content, org_id, user_id, seq, created_at, metadata)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                "#,
            )
            .bind(msg.id)
            .bind(conversation_id)
            .bind(&msg.role)
            .bind(&msg.content)
            .bind(&scope.org_id)
            .bind(scope.user_id.as_deref())
            .bind(msg.seq)
            .bind(msg.created_at)
            .bind(&metadata_json)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;

            ids.push(msg.id);
        }

        Ok(ids)
    }

    async fn get_messages(
        &self,
        conversation_id: &str,
        last_n: Option<usize>,
        scope: &Scope,
    ) -> Result<Vec<ChatMessage>, MemoryError> {
        let rows = match last_n {
            Some(n) => {
                // Subquery: pick the last N rows by seq DESC, then re-order ASC.
                let sql = r#"
                    SELECT * FROM (
                        SELECT * FROM messages
                        WHERE conversation_id = $1 AND org_id = $2
                        ORDER BY seq DESC
                        LIMIT $3
                    ) sub ORDER BY seq ASC
                "#;

                sqlx::query_as::<_, MessageRow>(sql)
                    .bind(conversation_id)
                    .bind(&scope.org_id)
                    .bind(n as i64)
                    .fetch_all(&self.pool)
                    .await
                    .map_err(|e| MemoryError::Database(e.to_string()))?
            }
            None => {
                let sql = r#"
                    SELECT * FROM messages
                    WHERE conversation_id = $1 AND org_id = $2
                    ORDER BY seq ASC
                "#;

                sqlx::query_as::<_, MessageRow>(sql)
                    .bind(conversation_id)
                    .bind(&scope.org_id)
                    .fetch_all(&self.pool)
                    .await
                    .map_err(|e| MemoryError::Database(e.to_string()))?
            }
        };

        rows.into_iter().map(row_to_message).collect()
    }

    async fn list_conversations(&self, scope: &Scope) -> Result<Vec<String>, MemoryError> {
        let sql = r#"
            SELECT DISTINCT conversation_id
            FROM messages
            WHERE org_id = $1
            ORDER BY conversation_id
        "#;

        let rows: Vec<(String,)> = sqlx::query_as(sql)
            .bind(&scope.org_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;

        Ok(rows.into_iter().map(|(c,)| c).collect())
    }

    async fn delete_messages(
        &self,
        conversation_id: &str,
        scope: &Scope,
    ) -> Result<u64, MemoryError> {
        let result = sqlx::query("DELETE FROM messages WHERE conversation_id = $1 AND org_id = $2")
            .bind(conversation_id)
            .bind(&scope.org_id)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;

        Ok(result.rows_affected())
    }
}
