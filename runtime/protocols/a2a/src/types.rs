//! A2A protocol types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A2A task lifecycle states.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum A2aTaskState {
    Submitted,
    Working,
    InputRequired,
    Completed,
    Failed,
    Canceled,
}

/// A2A task object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2aTask {
    pub id: String,
    pub session_id: Option<String>,
    pub status: A2aTaskStatus,
    pub artifacts: Vec<A2aArtifact>,
    pub history: Vec<A2aMessage>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2aTaskStatus {
    pub state: A2aTaskState,
    pub message: Option<A2aMessage>,
    pub timestamp: Option<String>,
}

/// A message in the A2A task exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2aMessage {
    pub role: String, // "user" | "agent"
    pub parts: Vec<A2aPart>,
    pub metadata: Option<Value>,
}

/// A part of an A2A message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum A2aPart {
    Text { text: String },
    File { file: A2aFile },
    Data { data: Value },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2aFile {
    pub name: Option<String>,
    pub mime_type: Option<String>,
    pub bytes: Option<String>, // base64
    pub uri: Option<String>,
}

/// An artifact produced by a completed A2A task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2aArtifact {
    pub name: Option<String>,
    pub description: Option<String>,
    pub parts: Vec<A2aPart>,
    pub index: u32,
    pub last_chunk: Option<bool>,
    pub metadata: Option<Value>,
}

/// A tasks/send request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendTaskRequest {
    pub id: String,
    pub session_id: Option<String>,
    pub message: A2aMessage,
    pub history_length: Option<u32>,
    pub metadata: Option<Value>,
}

/// An SSE event for streaming task updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum A2aStreamEvent {
    TaskStatusUpdate {
        id: String,
        status: A2aTaskStatus,
        final_event: Option<bool>,
    },
    ArtifactUpdate {
        id: String,
        artifact: A2aArtifact,
    },
}
