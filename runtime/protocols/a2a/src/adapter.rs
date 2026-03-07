//! `ProtocolAdapter` implementation for the A2A protocol.
//!
//! Bridges the generic `ProtocolAdapter` trait to `A2aClient`, enabling
//! A2A agents to be invoked transparently from the JamJet protocol registry.

use crate::client::A2aClient;
use crate::types::{A2aMessage, A2aPart, A2aTaskState, SendTaskRequest};
use async_trait::async_trait;
use jamjet_protocols::{
    ProtocolAdapter, RemoteCapabilities, RemoteSkill, TaskEvent, TaskHandle, TaskRequest,
    TaskStatus, TaskStream,
};
use serde_json::json;
use std::time::Duration;
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

#[async_trait]
impl ProtocolAdapter for A2aAdapter {
    #[instrument(skip(self), fields(url = %url))]
    async fn discover(&self, url: &str) -> Result<RemoteCapabilities, String> {
        let card = self.client.discover(url).await?;

        let skills = card
            .capabilities
            .skills
            .iter()
            .map(|s| RemoteSkill {
                name: s.name.clone(),
                description: Some(s.description.clone()),
                input_schema: serde_json::from_str(&s.input_schema).ok(),
                output_schema: serde_json::from_str(&s.output_schema).ok(),
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
        let message = A2aMessage {
            role: "user".into(),
            parts: vec![A2aPart::Data { data: task.input }],
            metadata: Some(json!({ "skill": task.skill })),
        };

        let metadata = if task.metadata.is_object() {
            Some(task.metadata)
        } else {
            None
        };
        let request = SendTaskRequest {
            id: task_id.clone(),
            session_id: None,
            message,
            history_length: None,
            metadata,
        };

        self.client.send_task(url, request).await?;

        Ok(TaskHandle {
            task_id,
            remote_url: url.to_string(),
        })
    }

    async fn stream(&self, url: &str, task: TaskRequest) -> Result<TaskStream, String> {
        // Submit task first, then subscribe to SSE.
        let handle = self.invoke(url, task).await?;

        let stream = self.client.subscribe_task(url, &handle.task_id).await?;

        // Map A2aStreamEvent to TaskEvent.
        use futures::StreamExt;
        let mapped = stream.filter_map(|result| {
            futures::future::ready(result.ok().and_then(|event| {
                use crate::types::A2aStreamEvent;
                match event {
                    A2aStreamEvent::TaskStatusUpdate { status, .. } => match status.state {
                        A2aTaskState::Working => {
                            let msg = status
                                .message
                                .and_then(|m| {
                                    m.parts.into_iter().find_map(|p| {
                                        if let A2aPart::Text { text } = p {
                                            Some(text)
                                        } else {
                                            None
                                        }
                                    })
                                })
                                .unwrap_or_default();
                            Some(TaskEvent::Progress {
                                message: msg,
                                progress: None,
                            })
                        }
                        A2aTaskState::Completed => Some(TaskEvent::Completed { output: json!({}) }),
                        A2aTaskState::Failed => {
                            let err = status
                                .message
                                .and_then(|m| {
                                    m.parts.into_iter().find_map(|p| {
                                        if let A2aPart::Text { text } = p {
                                            Some(text)
                                        } else {
                                            None
                                        }
                                    })
                                })
                                .unwrap_or_else(|| "unknown error".into());
                            Some(TaskEvent::Failed { error: err })
                        }
                        A2aTaskState::InputRequired => {
                            let prompt = status
                                .message
                                .and_then(|m| {
                                    m.parts.into_iter().find_map(|p| {
                                        if let A2aPart::Text { text } = p {
                                            Some(text)
                                        } else {
                                            None
                                        }
                                    })
                                })
                                .unwrap_or_else(|| "Input required".into());
                            Some(TaskEvent::InputRequired { prompt })
                        }
                        _ => None,
                    },
                    A2aStreamEvent::ArtifactUpdate { artifact, .. } => {
                        let data = artifact
                            .parts
                            .into_iter()
                            .find_map(|p| {
                                if let A2aPart::Data { data } = p {
                                    Some(data)
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(json!({}));
                        let name = artifact.name.unwrap_or_else(|| "artifact".into());
                        Some(TaskEvent::Artifact { name, data })
                    }
                }
            }))
        });

        Ok(Box::pin(mapped))
    }

    #[instrument(skip(self), fields(url = %url, task_id = %task_id))]
    async fn status(&self, url: &str, task_id: &str) -> Result<TaskStatus, String> {
        let task = self.client.get_task(url, task_id).await?;

        Ok(match task.status.state {
            A2aTaskState::Submitted => TaskStatus::Submitted,
            A2aTaskState::Working => TaskStatus::Working,
            A2aTaskState::InputRequired => TaskStatus::InputRequired,
            A2aTaskState::Completed => {
                let output = task
                    .artifacts
                    .first()
                    .and_then(|a| a.parts.first())
                    .and_then(|p| {
                        if let A2aPart::Data { data } = p {
                            Some(data.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or(json!({}));
                TaskStatus::Completed { output }
            }
            A2aTaskState::Failed => {
                let error = task
                    .status
                    .message
                    .and_then(|m| {
                        m.parts.into_iter().find_map(|p| {
                            if let A2aPart::Text { text } = p {
                                Some(text)
                            } else {
                                None
                            }
                        })
                    })
                    .unwrap_or_else(|| "unknown error".into());
                TaskStatus::Failed { error }
            }
            A2aTaskState::Canceled => TaskStatus::Cancelled,
        })
    }

    #[instrument(skip(self), fields(url = %url, task_id = %task_id))]
    async fn cancel(&self, url: &str, task_id: &str) -> Result<(), String> {
        self.client.cancel_task(url, task_id).await
    }
}
