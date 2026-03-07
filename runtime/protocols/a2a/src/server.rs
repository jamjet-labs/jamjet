//! A2A server — publish Agent Card, accept tasks, manage lifecycle, stream updates.
//!
//! Exposes:
//!   GET  /.well-known/agent.json  — Agent Card (D2.9)
//!   POST /                        — JSON-RPC: tasks/send, tasks/get, tasks/cancel (D2.10–D2.11)
//!   GET  /sse/{task_id}           — SSE task progress stream (D2.13)

use crate::types::*;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{sse::Event as SseEvent, sse::Sse, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use jamjet_agents::card::AgentCard;
use serde_json::{json, Value};
use std::{collections::HashMap, convert::Infallible, sync::Arc};
use tokio::sync::{broadcast, Mutex};
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tracing::{info, warn};

// ── Task store ────────────────────────────────────────────────────────────────

/// In-memory task store with per-task SSE broadcast channels.
pub struct TaskStore {
    tasks: Mutex<HashMap<String, A2aTask>>,
    /// Per-task broadcast channels for SSE streaming.
    channels: Mutex<HashMap<String, broadcast::Sender<A2aStreamEvent>>>,
}

impl TaskStore {
    fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            channels: Mutex::new(HashMap::new()),
        }
    }

    async fn insert(&self, task: A2aTask) {
        let task_id = task.id.clone();
        let (tx, _) = broadcast::channel(64);
        self.tasks.lock().await.insert(task_id.clone(), task);
        self.channels.lock().await.insert(task_id, tx);
    }

    async fn get(&self, task_id: &str) -> Option<A2aTask> {
        self.tasks.lock().await.get(task_id).cloned()
    }

    async fn update_status(&self, task_id: &str, state: A2aTaskState, message: Option<A2aMessage>) {
        let status = A2aTaskStatus {
            state: state.clone(),
            message: message.clone(),
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
        };

        let event = A2aStreamEvent::TaskStatusUpdate {
            id: task_id.to_string(),
            status: status.clone(),
            final_event: Some(matches!(
                state,
                A2aTaskState::Completed | A2aTaskState::Failed | A2aTaskState::Canceled
            )),
        };

        {
            let mut tasks = self.tasks.lock().await;
            if let Some(task) = tasks.get_mut(task_id) {
                task.status = status;
                if let Some(msg) = message {
                    task.history.push(msg);
                }
            }
        }

        // Broadcast to SSE subscribers.
        let channels = self.channels.lock().await;
        if let Some(tx) = channels.get(task_id) {
            let _ = tx.send(event);
        }
    }

    async fn add_artifact(&self, task_id: &str, artifact: A2aArtifact) {
        let event = A2aStreamEvent::ArtifactUpdate {
            id: task_id.to_string(),
            artifact: artifact.clone(),
        };
        {
            let mut tasks = self.tasks.lock().await;
            if let Some(task) = tasks.get_mut(task_id) {
                task.artifacts.push(artifact);
            }
        }
        let channels = self.channels.lock().await;
        if let Some(tx) = channels.get(task_id) {
            let _ = tx.send(event);
        }
    }

    async fn subscribe(&self, task_id: &str) -> Option<broadcast::Receiver<A2aStreamEvent>> {
        self.channels
            .lock()
            .await
            .get(task_id)
            .map(|tx| tx.subscribe())
    }

    async fn cancel(&self, task_id: &str) {
        self.update_status(task_id, A2aTaskState::Canceled, None)
            .await;
    }
}

// ── Server state ──────────────────────────────────────────────────────────────

#[derive(Clone)]
struct ServerState {
    card: Arc<AgentCard>,
    tasks: Arc<TaskStore>,
    /// Optional task handler — called when tasks/send is received.
    /// If None, tasks are accepted but immediately transition to "failed".
    handler: Arc<Option<Box<dyn TaskHandler>>>,
}

/// Callback trait for processing incoming A2A tasks.
pub trait TaskHandler: Send + Sync {
    fn handle(
        &self,
        task_id: String,
        message: A2aMessage,
        tasks: Arc<TaskStore>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
}

// ── Routes ────────────────────────────────────────────────────────────────────

/// Build the Axum router for the A2A server.
fn build_router(state: ServerState) -> Router {
    Router::new()
        .route("/.well-known/agent.json", get(agent_card))
        .route("/", post(rpc_handler))
        .route("/sse/:task_id", get(sse_handler))
        .with_state(state)
}

async fn agent_card(State(state): State<ServerState>) -> Json<Value> {
    Json(serde_json::to_value(&*state.card).unwrap_or(json!({})))
}

async fn rpc_handler(
    State(state): State<ServerState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let method = body.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let rpc_id = body.get("id").cloned().unwrap_or(json!(1));

    match method {
        "tasks/send" => {
            let params = body.get("params").cloned().unwrap_or(json!({}));
            let request: SendTaskRequest = match serde_json::from_value(params) {
                Ok(r) => r,
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "jsonrpc": "2.0",
                            "id": rpc_id,
                            "error": { "code": -32600, "message": e.to_string() }
                        })),
                    )
                        .into_response();
                }
            };

            let task_id = request.id.clone();
            let now = chrono::Utc::now().to_rfc3339();
            let task = A2aTask {
                id: task_id.clone(),
                session_id: request.session_id.clone(),
                status: A2aTaskStatus {
                    state: A2aTaskState::Submitted,
                    message: None,
                    timestamp: Some(now),
                },
                artifacts: Vec::new(),
                history: vec![request.message.clone()],
                metadata: request.metadata.clone(),
            };
            state.tasks.insert(task).await;

            info!(task_id = %task_id, "A2A task received");

            // Spawn handler if registered, else immediately fail.
            let tasks_clone = Arc::clone(&state.tasks);
            let handler_clone = Arc::clone(&state.handler);
            let msg = request.message.clone();
            let tid = task_id.clone();
            tokio::spawn(async move {
                if let Some(handler) = handler_clone.as_ref() {
                    tasks_clone
                        .update_status(&tid, A2aTaskState::Working, None)
                        .await;
                    handler.handle(tid.clone(), msg, tasks_clone.clone()).await;
                } else {
                    tasks_clone
                        .update_status(
                            &tid,
                            A2aTaskState::Failed,
                            Some(A2aMessage {
                                role: "agent".into(),
                                parts: vec![A2aPart::Text {
                                    text: "No handler registered".into(),
                                }],
                                metadata: None,
                            }),
                        )
                        .await;
                }
            });

            let task_result = state.tasks.get(&task_id).await.unwrap();
            (
                StatusCode::OK,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": rpc_id,
                    "result": task_result,
                })),
            )
                .into_response()
        }

        "tasks/get" => {
            let task_id = body["params"]["id"].as_str().unwrap_or("");
            match state.tasks.get(task_id).await {
                Some(task) => (
                    StatusCode::OK,
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": rpc_id,
                        "result": task,
                    })),
                )
                    .into_response(),
                None => (
                    StatusCode::OK,
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": rpc_id,
                        "error": { "code": -32001, "message": format!("task not found: {task_id}") }
                    })),
                )
                    .into_response(),
            }
        }

        "tasks/cancel" => {
            let task_id = body["params"]["id"].as_str().unwrap_or("");
            state.tasks.cancel(task_id).await;
            (
                StatusCode::OK,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": rpc_id,
                    "result": { "id": task_id, "status": "canceled" }
                })),
            )
                .into_response()
        }

        _ => {
            warn!(method = %method, "Unknown A2A RPC method");
            (
                StatusCode::OK,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": rpc_id,
                    "error": { "code": -32601, "message": format!("method not found: {method}") }
                })),
            )
                .into_response()
        }
    }
}

async fn sse_handler(
    State(state): State<ServerState>,
    Path(task_id): Path<String>,
) -> impl IntoResponse {
    let Some(rx) = state.tasks.subscribe(&task_id).await else {
        return (StatusCode::NOT_FOUND, "task not found").into_response();
    };

    let stream = BroadcastStream::new(rx).filter_map(|result| {
        result.ok().and_then(|event| {
            serde_json::to_string(&event)
                .ok()
                .map(|data| Ok::<_, Infallible>(SseEvent::default().data(data)))
        })
    });

    Sse::new(stream).into_response()
}

// ── Public API ────────────────────────────────────────────────────────────────

/// A2A server handle.
pub struct A2aServer {
    card: AgentCard,
    port: u16,
    handler: Option<Box<dyn TaskHandler>>,
}

impl A2aServer {
    pub fn new(card: AgentCard, port: u16) -> Self {
        Self {
            card,
            port,
            handler: None,
        }
    }

    pub fn with_handler(mut self, handler: impl TaskHandler + 'static) -> Self {
        self.handler = Some(Box::new(handler));
        self
    }

    /// Start the A2A server and serve until the process exits.
    pub async fn start(self) -> Result<(), String> {
        let state = ServerState {
            card: Arc::new(self.card.clone()),
            tasks: Arc::new(TaskStore::new()),
            handler: Arc::new(self.handler),
        };

        let router = build_router(state);
        let addr = format!("0.0.0.0:{}", self.port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| format!("failed to bind {addr}: {e}"))?;

        info!(
            agent_id = %self.card.id,
            addr = %addr,
            "A2A server listening"
        );

        axum::serve(listener, router)
            .await
            .map_err(|e| format!("A2A server error: {e}"))?;

        Ok(())
    }
}
