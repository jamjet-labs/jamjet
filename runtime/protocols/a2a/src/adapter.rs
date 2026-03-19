//! `ProtocolAdapter` implementation for the A2A protocol.
//!
//! Bridges the generic `ProtocolAdapter` trait to the published `jamjet-a2a`
//! crate's `A2aClient`, enabling A2A agents to be invoked transparently from
//! the JamJet protocol registry.

use async_trait::async_trait;
use jamjet_a2a::client::A2aClient;
use jamjet_a2a_types::{
    CancelTaskRequest, GetTaskRequest, Message, Part, PartContent, Role, SendMessageRequest,
    StreamResponse, TaskState, TaskStatusUpdateEvent,
};
use jamjet_protocols::{
    ProtocolAdapter, RemoteCapabilities, RemoteSkill, TaskEvent, TaskHandle, TaskRequest,
    TaskStatus, TaskStream,
};
use serde_json::json;
use std::collections::HashMap;
use tracing::instrument;
use uuid::Uuid;

/// A2A protocol adapter implementing `ProtocolAdapter`.
pub struct A2aAdapter {
    client: A2aClient,
}

impl A2aAdapter {
    pub fn new() -> Self {
        Self {
            client: A2aClient::new(),
        }
    }
}

impl Default for A2aAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper: build a `Message` from a user-role data payload.
fn make_user_message(input: serde_json::Value, skill: &str, task_id: Option<&str>) -> Message {
    let mut metadata_map = HashMap::new();
    metadata_map.insert("skill".to_string(), json!(skill));

    Message {
        message_id: Uuid::new_v4().to_string(),
        context_id: None,
        task_id: task_id.map(|s| s.to_string()),
        role: Role::User,
        parts: vec![Part {
            content: PartContent::Data(input),
            metadata: None,
            filename: None,
            media_type: None,
        }],
        metadata: Some(metadata_map),
        extensions: vec![],
        reference_task_ids: vec![],
    }
}

/// Helper: extract a text string from a message's parts (first Text part found).
fn extract_text_from_parts(parts: &[Part]) -> Option<String> {
    parts.iter().find_map(|p| {
        if let PartContent::Text(ref text) = p.content {
            Some(text.clone())
        } else {
            None
        }
    })
}

/// Helper: extract a data Value from a message's parts (first Data part found).
fn extract_data_from_parts(parts: &[Part]) -> Option<serde_json::Value> {
    parts.iter().find_map(|p| {
        if let PartContent::Data(ref data) = p.content {
            Some(data.clone())
        } else {
            None
        }
    })
}

#[async_trait]
impl ProtocolAdapter for A2aAdapter {
    #[instrument(skip(self), fields(url = %url))]
    async fn discover(&self, url: &str) -> Result<RemoteCapabilities, String> {
        let card = self.client.discover(url).await.map_err(|e| e.to_string())?;

        let skills = card
            .skills
            .iter()
            .map(|s| RemoteSkill {
                name: s.name.clone(),
                description: Some(s.description.clone()),
                input_schema: None,
                output_schema: None,
            })
            .collect();

        Ok(RemoteCapabilities {
            name: card.name,
            description: Some(card.description),
            skills,
            protocols: vec!["a2a".into()],
        })
    }

    #[instrument(skip(self, task), fields(url = %url, skill = %task.skill))]
    async fn invoke(&self, url: &str, task: TaskRequest) -> Result<TaskHandle, String> {
        let task_id = Uuid::new_v4().to_string();
        let message = make_user_message(task.input, &task.skill, Some(&task_id));

        let metadata = if task.metadata.is_object() {
            let map: HashMap<String, serde_json::Value> =
                serde_json::from_value(task.metadata).unwrap_or_default();
            if map.is_empty() { None } else { Some(map) }
        } else {
            None
        };

        let request = SendMessageRequest {
            tenant: None,
            message,
            configuration: None,
            metadata,
        };

        self.client
            .send_message(url, request)
            .await
            .map_err(|e| e.to_string())?;

        Ok(TaskHandle {
            task_id,
            remote_url: url.to_string(),
        })
    }

    async fn stream(&self, url: &str, task: TaskRequest) -> Result<TaskStream, String> {
        // Submit task first, then subscribe to SSE.
        let handle = self.invoke(url, task).await?;

        let stream = self
            .client
            .subscribe(url, &handle.task_id)
            .await
            .map_err(|e| e.to_string())?;

        // Map StreamResponse events to TaskEvent.
        use futures::StreamExt;
        let mapped = stream.filter_map(|result| {
            futures::future::ready(result.ok().and_then(|event| match event {
                StreamResponse::StatusUpdate(TaskStatusUpdateEvent { status, .. }) => {
                    match status.state {
                        TaskState::Working => {
                            let msg = status
                                .message
                                .as_ref()
                                .and_then(|m| extract_text_from_parts(&m.parts))
                                .unwrap_or_default();
                            Some(TaskEvent::Progress {
                                message: msg,
                                progress: None,
                            })
                        }
                        TaskState::Completed => Some(TaskEvent::Completed { output: json!({}) }),
                        TaskState::Failed => {
                            let err = status
                                .message
                                .as_ref()
                                .and_then(|m| extract_text_from_parts(&m.parts))
                                .unwrap_or_else(|| "unknown error".into());
                            Some(TaskEvent::Failed { error: err })
                        }
                        TaskState::InputRequired => {
                            let prompt = status
                                .message
                                .as_ref()
                                .and_then(|m| extract_text_from_parts(&m.parts))
                                .unwrap_or_else(|| "Input required".into());
                            Some(TaskEvent::InputRequired { prompt })
                        }
                        _ => None,
                    }
                }
                StreamResponse::ArtifactUpdate(artifact_event) => {
                    let data = extract_data_from_parts(&artifact_event.artifact.parts)
                        .unwrap_or(json!({}));
                    let name = artifact_event
                        .artifact
                        .name
                        .unwrap_or_else(|| "artifact".into());
                    Some(TaskEvent::Artifact { name, data })
                }
                // Task and Message variants are not mapped to events.
                _ => None,
            }))
        });

        Ok(Box::pin(mapped))
    }

    #[instrument(skip(self), fields(url = %url, task_id = %task_id))]
    async fn status(&self, url: &str, task_id: &str) -> Result<TaskStatus, String> {
        let task = self
            .client
            .get_task(
                url,
                GetTaskRequest {
                    tenant: None,
                    id: task_id.to_string(),
                    history_length: None,
                },
            )
            .await
            .map_err(|e| e.to_string())?;

        Ok(match task.status.state {
            TaskState::Submitted => TaskStatus::Submitted,
            TaskState::Working => TaskStatus::Working,
            TaskState::InputRequired => TaskStatus::InputRequired,
            TaskState::Completed => {
                let output = task
                    .artifacts
                    .first()
                    .and_then(|a| a.parts.first())
                    .and_then(|p| extract_data_from_parts(std::slice::from_ref(p)))
                    .unwrap_or(json!({}));
                TaskStatus::Completed { output }
            }
            TaskState::Failed => {
                let error = task
                    .status
                    .message
                    .as_ref()
                    .and_then(|m| extract_text_from_parts(&m.parts))
                    .unwrap_or_else(|| "unknown error".into());
                TaskStatus::Failed { error }
            }
            TaskState::Canceled => TaskStatus::Cancelled,
            // Rejected and AuthRequired map to Failed for the ProtocolAdapter trait.
            _ => TaskStatus::Failed {
                error: format!("unexpected task state: {:?}", task.status.state),
            },
        })
    }

    #[instrument(skip(self), fields(url = %url, task_id = %task_id))]
    async fn cancel(&self, url: &str, task_id: &str) -> Result<(), String> {
        self.client
            .cancel_task(
                url,
                CancelTaskRequest {
                    tenant: None,
                    id: task_id.to_string(),
                    metadata: None,
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
