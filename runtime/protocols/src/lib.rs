//! JamJet Protocol Adapter Framework
//!
//! The `ProtocolAdapter` trait defines the common interface all protocol
//! adapters must implement. Built-in adapters: MCP, A2A, ANP.
//! New protocols can be added by implementing this trait without modifying
//! the runtime core.

pub mod anp;
pub mod registry;

pub use registry::ProtocolRegistry;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::pin::Pin;
use tokio_stream::Stream;

/// A task request to a remote agent or tool provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRequest {
    pub skill: String,
    pub input: Value,
    pub timeout_secs: Option<u64>,
    pub stream: bool,
    pub metadata: Value,
}

/// A handle to a submitted task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskHandle {
    pub task_id: String,
    pub remote_url: String,
}

/// An event streamed from a running task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskEvent {
    Progress {
        message: String,
        progress: Option<f32>,
    },
    Artifact {
        name: String,
        data: Value,
    },
    InputRequired {
        prompt: String,
    },
    Completed {
        output: Value,
    },
    Failed {
        error: String,
    },
}

/// Current status of a remote task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Submitted,
    Working,
    InputRequired,
    Completed { output: Value },
    Failed { error: String },
    Cancelled,
}

/// Capabilities discovered from a remote agent or tool provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteCapabilities {
    pub name: String,
    pub description: Option<String>,
    pub skills: Vec<RemoteSkill>,
    pub protocols: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSkill {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Option<Value>,
    pub output_schema: Option<Value>,
}

pub type TaskStream = Pin<Box<dyn Stream<Item = TaskEvent> + Send>>;

// ── Structured streaming (I2.5) ───────────────────────────────────────────────

/// A typed stream chunk for structured streaming across all protocol adapters.
///
/// Follows the OTel GenAI structured streaming convention:
/// text_delta → tool_call → progress → artifact → final
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamChunk {
    /// Incremental text delta (LLM streaming tokens).
    TextDelta { delta: String },
    /// A tool call invocation during streaming.
    ToolCall { name: String, arguments: Value },
    /// Progress indicator (0.0–1.0).
    Progress {
        message: String,
        fraction: Option<f32>,
    },
    /// A completed artifact (file, data, structured output).
    Artifact {
        name: String,
        data: Value,
        mime_type: Option<String>,
    },
    /// Final completed output. Signals end of stream.
    Final { output: Value },
    /// Error that terminates the stream.
    Error { message: String },
}

pub type StructuredStream = Pin<Box<dyn Stream<Item = StreamChunk> + Send>>;

/// The protocol adapter trait. Implement this to add a new agent communication protocol.
#[async_trait]
pub trait ProtocolAdapter: Send + Sync {
    /// Discover remote capabilities (fetch Agent Card or equivalent).
    async fn discover(&self, url: &str) -> Result<RemoteCapabilities, String>;

    /// Submit a task/request to the remote.
    async fn invoke(&self, url: &str, task: TaskRequest) -> Result<TaskHandle, String>;

    /// Stream task progress events (for streaming tasks).
    async fn stream(&self, url: &str, task: TaskRequest) -> Result<TaskStream, String>;

    /// Stream task with structured typed chunks (I2.6).
    /// Default: wraps `stream()` and maps `TaskEvent` to `StreamChunk`.
    /// Each chunk is also emitted as a `tracing` span event (I2.8).
    async fn stream_structured(
        &self,
        url: &str,
        task: TaskRequest,
    ) -> Result<StructuredStream, String> {
        use tokio_stream::StreamExt;
        let event_stream = self.stream(url, task).await?;
        let chunk_stream = event_stream.map(|event| {
            let chunk = match event {
                TaskEvent::Progress { message, progress } => StreamChunk::Progress {
                    message,
                    fraction: progress,
                },
                TaskEvent::Artifact { name, data } => StreamChunk::Artifact {
                    name,
                    data,
                    mime_type: None,
                },
                TaskEvent::Completed { output } => StreamChunk::Final { output },
                TaskEvent::Failed { error } => StreamChunk::Error { message: error },
                TaskEvent::InputRequired { prompt } => StreamChunk::Progress {
                    message: format!("input required: {prompt}"),
                    fraction: None,
                },
            };
            // I2.8: emit span event for each structured stream chunk.
            let chunk_type = match &chunk {
                StreamChunk::TextDelta { .. } => "text_delta",
                StreamChunk::ToolCall { .. } => "tool_call",
                StreamChunk::Progress { .. } => "progress",
                StreamChunk::Artifact { .. } => "artifact",
                StreamChunk::Final { .. } => "final",
                StreamChunk::Error { .. } => "error",
            };
            tracing::debug!(stream_chunk = chunk_type, "stream_chunk_emitted");
            chunk
        });
        Ok(Box::pin(chunk_stream))
    }

    /// Stream with backpressure: bounded channel buffers at most `buffer_size` chunks
    /// before applying flow control to the producer (I2.9).
    async fn stream_with_backpressure(
        &self,
        url: &str,
        task: TaskRequest,
        buffer_size: usize,
    ) -> Result<StructuredStream, String> {
        use tokio_stream::StreamExt;
        let (tx, rx) = tokio::sync::mpsc::channel::<StreamChunk>(buffer_size);
        let mut source = self.stream_structured(url, task).await?;
        tokio::spawn(async move {
            while let Some(chunk) = source.next().await {
                if tx.send(chunk).await.is_err() {
                    break; // receiver dropped
                }
            }
        });
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    /// Poll task status by task_id.
    async fn status(&self, url: &str, task_id: &str) -> Result<TaskStatus, String>;

    /// Cancel a running task.
    async fn cancel(&self, url: &str, task_id: &str) -> Result<(), String>;
}
