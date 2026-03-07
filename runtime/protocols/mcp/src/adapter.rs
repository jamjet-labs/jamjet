//! MCP `ProtocolAdapter` — wraps `McpClient` to implement the generic adapter interface.
//!
//! MCP tool calls are synchronous (request-response), so:
//! - `discover()` → initialize + tools/list → RemoteCapabilities
//! - `invoke()` → tools/call (synchronous), caches result by task_id
//! - `status()` → looks up cached result, returns Completed/Failed
//! - `stream()` → calls tool, emits single Completed event
//! - `cancel()` → no-op (MCP has no cancellation)

use crate::client::McpClient;
use crate::transport::HttpSseTransport;
use async_trait::async_trait;
use jamjet_protocols::{
    ProtocolAdapter, RemoteCapabilities, RemoteSkill, TaskEvent, TaskHandle, TaskRequest,
    TaskStatus, TaskStream,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio_stream::once;
use tracing::{debug, instrument};
use uuid::Uuid;

pub struct McpAdapter {
    /// Cached results: task_id → Ok(output) | Err(message)
    results: Arc<Mutex<HashMap<String, Result<Value, String>>>>,
}

impl McpAdapter {
    pub fn new() -> Self {
        Self {
            results: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn client_for(url: &str) -> McpClient {
        let transport = HttpSseTransport::new(url.to_string());
        McpClient::new(url.to_string(), Box::new(transport))
    }
}

impl Default for McpAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProtocolAdapter for McpAdapter {
    #[instrument(skip(self), fields(url = %url))]
    async fn discover(&self, url: &str) -> Result<RemoteCapabilities, String> {
        let client = Self::client_for(url);
        client.initialize().await?;

        let tools = client.list_tools().await?;
        client.close().await;

        let skills: Vec<RemoteSkill> = tools
            .into_iter()
            .map(|t| RemoteSkill {
                name: t.name,
                description: t.description,
                input_schema: Some(t.input_schema),
                output_schema: None,
            })
            .collect();

        Ok(RemoteCapabilities {
            name: url.to_string(),
            description: Some("MCP server".into()),
            skills,
            protocols: vec!["mcp".into()],
        })
    }

    #[instrument(skip(self, task), fields(url = %url, tool = %task.skill))]
    async fn invoke(&self, url: &str, task: TaskRequest) -> Result<TaskHandle, String> {
        let client = Self::client_for(url);
        client.initialize().await?;

        let resp = client.call_tool(&task.skill, task.input).await;
        client.close().await;

        let task_id = Uuid::new_v4().to_string();
        debug!(task_id = %task_id, "MCP tool call completed (synchronous)");

        let result: Result<Value, String> = match resp {
            Ok(r) if r.is_error == Some(true) => {
                let msg = r
                    .content
                    .into_iter()
                    .find_map(|c| {
                        if let crate::types::McpContent::Text { text } = c {
                            Some(text)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| "MCP tool returned an error".into());
                Err(msg)
            }
            Ok(r) => {
                // Collect all text/data content into a JSON value.
                let parts: Vec<Value> = r.content.into_iter().map(|c| match c {
                    crate::types::McpContent::Text { text } => {
                        serde_json::json!({ "type": "text", "text": text })
                    }
                    crate::types::McpContent::Image { data, mime_type } => {
                        serde_json::json!({ "type": "image", "data": data, "mimeType": mime_type })
                    }
                    crate::types::McpContent::Resource { uri, mime_type, text } => {
                        serde_json::json!({ "type": "resource", "uri": uri, "mimeType": mime_type, "text": text })
                    }
                }).collect();
                let output = if parts.len() == 1 {
                    parts.into_iter().next().unwrap()
                } else {
                    serde_json::Value::Array(parts)
                };
                Ok(output)
            }
            Err(e) => Err(e),
        };

        self.results.lock().unwrap().insert(task_id.clone(), result);

        Ok(TaskHandle {
            task_id,
            remote_url: url.to_string(),
        })
    }

    async fn stream(&self, url: &str, task: TaskRequest) -> Result<TaskStream, String> {
        // MCP is synchronous — invoke the tool and emit a single Completed event.
        let handle = self.invoke(url, task).await?;
        let status = self.status(url, &handle.task_id).await?;

        let event = match status {
            TaskStatus::Completed { output } => TaskEvent::Completed { output },
            TaskStatus::Failed { error } => TaskEvent::Failed { error },
            _ => TaskEvent::Failed {
                error: "unexpected MCP task state".into(),
            },
        };

        Ok(Box::pin(once(event)))
    }

    async fn status(&self, _url: &str, task_id: &str) -> Result<TaskStatus, String> {
        let guard = self.results.lock().unwrap();
        match guard.get(task_id) {
            Some(Ok(output)) => Ok(TaskStatus::Completed {
                output: output.clone(),
            }),
            Some(Err(e)) => Ok(TaskStatus::Failed { error: e.clone() }),
            None => Err(format!("MCP task not found: {task_id}")),
        }
    }

    async fn cancel(&self, _url: &str, _task_id: &str) -> Result<(), String> {
        // MCP tool calls are synchronous; cancellation is a no-op.
        Ok(())
    }
}
