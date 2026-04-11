//! SQLite-backed `MessageStore` implementation.
//!
//! All `DateTime<Utc>` values are stored as RFC 3339 strings.
//! UUIDs are stored as TEXT. `metadata` is a JSON object string (or `"null"`
//! when empty).

use crate::message::{ChatMessage, MessageId, MessageStore};
use crate::scope::Scope;
use crate::store::MemoryError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

pub const MESSAGE_STORE_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS messages (
    id              TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL,
    role            TEXT NOT NULL,
    content         TEXT NOT NULL,
    org_id          TEXT NOT NULL DEFAULT 'default',
    user_id         TEXT,
    seq             INTEGER NOT NULL,
    created_at      TEXT NOT NULL,
    metadata        TEXT NOT NULL DEFAULT 'null'
);
CREATE INDEX IF NOT EXISTS idx_messages_conversation ON messages (conversation_id, seq);
CREATE INDEX IF NOT EXISTS idx_messages_org_user ON messages (org_id, user_id);
"#;

// ---------------------------------------------------------------------------
// SqliteMessageStore
// ---------------------------------------------------------------------------

pub struct SqliteMessageStore {
    pool: SqlitePool,
}

impl SqliteMessageStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Open a connection pool from a database URL and return a store.
    pub async fn open(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = SqlitePool::connect(database_url).await?;
        Ok(Self { pool })
    }

    /// Apply the DDL. Safe to call multiple times (uses `IF NOT EXISTS`).
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        for stmt in MESSAGE_STORE_DDL.split(';') {
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
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
    id: String,
    conversation_id: String,
    role: String,
    content: String,
    org_id: String,
    user_id: Option<String>,
    seq: i32,
    created_at: String,
    metadata: String,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn parse_dt(s: &str) -> Result<DateTime<Utc>, MemoryError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| MemoryError::Serialization(e.to_string()))
}

fn row_to_message(row: MessageRow) -> Result<ChatMessage, MemoryError> {
    let id = Uuid::parse_str(&row.id).map_err(|e| MemoryError::Serialization(e.to_string()))?;

    let metadata: serde_json::Map<String, serde_json::Value> =
        if row.metadata == "null" || row.metadata.is_empty() {
            serde_json::Map::new()
        } else {
            serde_json::from_str(&row.metadata)
                .map_err(|e| MemoryError::Serialization(e.to_string()))?
        };

    Ok(ChatMessage {
        id,
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
        created_at: parse_dt(&row.created_at)?,
        metadata,
    })
}

// ---------------------------------------------------------------------------
// MessageStore implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl MessageStore for SqliteMessageStore {
    async fn save_messages(
        &self,
        conversation_id: &str,
        messages: &[ChatMessage],
        scope: &Scope,
    ) -> Result<Vec<MessageId>, MemoryError> {
        let mut ids = Vec::with_capacity(messages.len());

        for msg in messages {
            let metadata_json = if msg.metadata.is_empty() {
                "null".to_string()
            } else {
                serde_json::to_string(&msg.metadata)
                    .map_err(|e| MemoryError::Serialization(e.to_string()))?
            };

            sqlx::query(
                r#"
                INSERT INTO messages
                    (id, conversation_id, role, content, org_id, user_id, seq, created_at, metadata)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(msg.id.to_string())
            .bind(conversation_id)
            .bind(&msg.role)
            .bind(&msg.content)
            .bind(&scope.org_id)
            .bind(scope.user_id.as_deref())
            .bind(msg.seq)
            .bind(msg.created_at.to_rfc3339())
            .bind(metadata_json)
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
                        WHERE conversation_id = ? AND org_id = ?
                        ORDER BY seq DESC
                        LIMIT ?
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
                    WHERE conversation_id = ? AND org_id = ?
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
            WHERE org_id = ?
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
        let result = sqlx::query("DELETE FROM messages WHERE conversation_id = ? AND org_id = ?")
            .bind(conversation_id)
            .bind(&scope.org_id)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;

        Ok(result.rows_affected())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> SqliteMessageStore {
        let store = SqliteMessageStore::open("sqlite::memory:").await.unwrap();
        store.migrate().await.unwrap();
        store
    }

    #[tokio::test]
    async fn message_round_trip() {
        let store = test_store().await;
        let scope = Scope::user("acme", "alice");

        let msgs = vec![
            ChatMessage::new("conv-1", "user", "Hello", scope.clone(), 0),
            ChatMessage::new("conv-1", "assistant", "Hi there!", scope.clone(), 1),
            ChatMessage::new("conv-1", "user", "How are you?", scope.clone(), 2),
        ];

        let ids = store.save_messages("conv-1", &msgs, &scope).await.unwrap();
        assert_eq!(ids.len(), 3);

        let all = store.get_messages("conv-1", None, &scope).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].role, "user");
        assert_eq!(all[1].role, "assistant");
        assert_eq!(all[2].content, "How are you?");

        let last2 = store.get_messages("conv-1", Some(2), &scope).await.unwrap();
        assert_eq!(last2.len(), 2);
        assert_eq!(last2[0].role, "assistant");
        assert_eq!(last2[1].role, "user");

        let convs = store.list_conversations(&scope).await.unwrap();
        assert_eq!(convs, vec!["conv-1"]);

        let deleted = store.delete_messages("conv-1", &scope).await.unwrap();
        assert_eq!(deleted, 3);

        let empty = store.get_messages("conv-1", None, &scope).await.unwrap();
        assert!(empty.is_empty());
    }
}
