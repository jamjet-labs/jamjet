//! `ChatMessage` type and `MessageStore` trait — chat message persistence.
//!
//! `MessageStore` stores raw conversation messages (role + content) grouped by
//! `conversation_id`. This is separate from `FactStore`, which stores extracted
//! facts derived from conversations. Agents use `MessageStore` to replay
//! conversation history and provide context windows to LLMs.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::scope::Scope;
use crate::store::MemoryError;

/// Unique identifier for a chat message.
pub type MessageId = Uuid;

/// A single chat message within a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: MessageId,
    pub conversation_id: String,
    pub role: String,
    pub content: String,
    pub scope: Scope,
    pub seq: i32,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

impl ChatMessage {
    pub fn new(
        conversation_id: impl Into<String>,
        role: impl Into<String>,
        content: impl Into<String>,
        scope: Scope,
        seq: i32,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            conversation_id: conversation_id.into(),
            role: role.into(),
            content: content.into(),
            scope,
            seq,
            created_at: Utc::now(),
            metadata: serde_json::Map::new(),
        }
    }
}

/// Persistence interface for chat messages.
///
/// Implementations MUST be `Send + Sync` so that `Arc<dyn MessageStore>` can be
/// shared across async tasks.
#[async_trait]
pub trait MessageStore: Send + Sync {
    /// Save one or more messages to a conversation. Returns the ids of the
    /// saved messages.
    async fn save_messages(
        &self,
        conversation_id: &str,
        messages: &[ChatMessage],
        scope: &Scope,
    ) -> Result<Vec<MessageId>, MemoryError>;

    /// Retrieve messages from a conversation, optionally limited to the last N
    /// messages. Results are ordered by `seq` ascending.
    async fn get_messages(
        &self,
        conversation_id: &str,
        last_n: Option<usize>,
        scope: &Scope,
    ) -> Result<Vec<ChatMessage>, MemoryError>;

    /// List all conversation ids visible to the given scope.
    async fn list_conversations(&self, scope: &Scope) -> Result<Vec<String>, MemoryError>;

    /// Delete all messages in a conversation. Returns the number of rows deleted.
    async fn delete_messages(
        &self,
        conversation_id: &str,
        scope: &Scope,
    ) -> Result<u64, MemoryError>;
}
