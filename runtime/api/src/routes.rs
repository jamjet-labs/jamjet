use crate::auth::{require_auth, require_write_role, AuthState};
use crate::error::ApiError;
use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use jamjet_agents::{AgentCard, AgentFilter, AgentStatus};
use jamjet_audit::backend::AuditQuery;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{WorkItem, WorkflowDefinition};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

/// Build the Axum router with all API routes.
pub fn build_router(state: AppState) -> Router {
    let auth_state = AuthState {
        backend: state.backend.clone(),
    };

    // Protected routes — require valid Bearer token.
    let protected = Router::new()
        // Workflow definitions
        .route("/workflows", post(create_workflow))
        // Executions
        .route("/executions", post(start_execution).get(list_executions))
        .route("/executions/:id", get(get_execution))
        .route("/executions/:id/cancel", post(cancel_execution))
        .route("/executions/:id/events", get(list_events))
        .route("/executions/:id/approve", post(approve_execution))
        .route("/executions/:id/external-event", post(send_external_event))
        // Agents
        .route("/agents", post(register_agent).get(list_agents))
        .route("/agents/discover", post(discover_agent))
        .route("/agents/:id", get(get_agent))
        .route("/agents/:id/activate", post(activate_agent))
        .route("/agents/:id/deactivate", post(deactivate_agent))
        .route("/agents/:id/heartbeat", post(agent_heartbeat))
        // Admin
        .route("/workers", get(list_workers))
        // Audit log — immutable, append-only
        .route("/audit", get(list_audit_log))
        .layer(middleware::from_fn(require_write_role))
        .layer(middleware::from_fn_with_state(auth_state, require_auth))
        .with_state(state.clone());

    // Public routes — no auth required.
    Router::new()
        .route("/health", get(health))
        .route("/.well-known/did.json", get(serve_did_document))
        .merge(protected)
        .with_state(state)
}

// ── Health ───────────────────────────────────────────────────────────────────

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

// ── Workflow definitions ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateWorkflowRequest {
    ir: Value,
}

async fn create_workflow(
    State(state): State<AppState>,
    Json(body): Json<CreateWorkflowRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let workflow_id = body
        .ir
        .get("workflow_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::BadRequest("ir.workflow_id is required".into()))?
        .to_string();
    let version = body
        .ir
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::BadRequest("ir.version is required".into()))?
        .to_string();

    let def = WorkflowDefinition {
        workflow_id: workflow_id.clone(),
        version: version.clone(),
        ir: body.ir,
        created_at: Utc::now(),
    };
    state.backend.store_workflow(def).await?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "workflow_id": workflow_id,
            "version": version,
        })),
    ))
}

// ── Executions ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct StartExecutionRequest {
    workflow_id: String,
    workflow_version: Option<String>,
    input: Value,
}

async fn start_execution(
    State(state): State<AppState>,
    Json(body): Json<StartExecutionRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let version = body.workflow_version.unwrap_or_else(|| "1.0.0".into());

    // Verify the workflow definition exists.
    let def = state
        .backend
        .get_workflow(&body.workflow_id, &version)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("workflow {} v{}", body.workflow_id, version)))?;

    // Determine the start node from the IR.
    let start_node = def
        .ir
        .get("start_node")
        .and_then(|v| v.as_str())
        .unwrap_or("start")
        .to_string();

    let now = Utc::now();
    let input = body.input;
    let execution = WorkflowExecution {
        execution_id: ExecutionId::new(),
        workflow_id: body.workflow_id.clone(),
        workflow_version: version.clone(),
        status: WorkflowStatus::Running,
        initial_input: input.clone(),
        current_state: input.clone(),
        started_at: now,
        updated_at: now,
        completed_at: None,
    };
    let execution_id = execution.execution_id.clone();
    state.backend.create_execution(execution).await?;

    // Append WorkflowStarted event.
    let event = jamjet_state::Event::new(
        execution_id.clone(),
        1,
        jamjet_state::EventKind::WorkflowStarted {
            workflow_id: body.workflow_id.clone(),
            workflow_version: version.clone(),
            initial_input: input.clone(),
        },
    );
    state.backend.append_event(event).await?;

    // Immediately enqueue the start node as a work item.
    let queue_type = "general".to_string();
    let sched_event = jamjet_state::Event::new(
        execution_id.clone(),
        2,
        jamjet_state::EventKind::NodeScheduled {
            node_id: start_node.clone(),
            queue_type: queue_type.clone(),
        },
    );
    state.backend.append_event(sched_event).await?;

    let work_item = WorkItem {
        id: Uuid::new_v4(),
        execution_id: execution_id.clone(),
        node_id: start_node,
        queue_type,
        payload: json!({
            "workflow_id": body.workflow_id,
            "workflow_version": version,
        }),
        attempt: 0,
        max_attempts: 3,
        created_at: now,
        lease_expires_at: None,
        worker_id: None,
    };
    state.backend.enqueue_work_item(work_item).await?;

    Ok((
        StatusCode::CREATED,
        Json(json!({ "execution_id": execution_id.to_string() })),
    ))
}

#[derive(Deserialize)]
struct ListExecutionsQuery {
    status: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

async fn list_executions(
    State(state): State<AppState>,
    Query(params): Query<ListExecutionsQuery>,
) -> Result<Json<Value>, ApiError> {
    let status = params.status.as_deref().and_then(|s| match s {
        "running" => Some(WorkflowStatus::Running),
        "paused" => Some(WorkflowStatus::Paused),
        "completed" => Some(WorkflowStatus::Completed),
        "failed" => Some(WorkflowStatus::Failed),
        _ => None,
    });
    let executions = state
        .backend
        .list_executions(
            status,
            params.limit.unwrap_or(50),
            params.offset.unwrap_or(0),
        )
        .await?;
    Ok(Json(json!({ "executions": executions })))
}

async fn get_execution(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    // Parse the execution id from the URL (format: exec_<uuid_simple>)
    let execution_id = parse_execution_id(&id)?;
    let execution = state
        .backend
        .get_execution(&execution_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("execution {id}")))?;
    Ok(Json(serde_json::to_value(execution).unwrap()))
}

async fn cancel_execution(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let execution_id = parse_execution_id(&id)?;

    // Verify execution exists and is not already terminal.
    let execution = state
        .backend
        .get_execution(&execution_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("execution {id}")))?;

    if execution.status.is_terminal() {
        return Err(ApiError::BadRequest(format!(
            "execution {id} is already in terminal state: {:?}",
            execution.status
        )));
    }

    // Emit WorkflowCancelled event.
    let seq = state.backend.latest_sequence(&execution_id).await? + 1;
    let event = jamjet_state::Event::new(
        execution_id.clone(),
        seq,
        jamjet_state::EventKind::WorkflowCancelled {
            reason: Some("user request".into()),
        },
    );
    state.backend.append_event(event).await?;
    state
        .backend
        .update_execution_status(&execution_id, WorkflowStatus::Cancelled)
        .await?;

    Ok(Json(json!({ "execution_id": id, "status": "cancelled" })))
}

async fn list_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let execution_id = parse_execution_id(&id)?;
    let events = state.backend.get_events(&execution_id).await?;
    Ok(Json(json!({ "events": events })))
}

#[derive(Deserialize)]
struct ApproveRequest {
    decision: String,
    node_id: Option<String>,
    user_id: Option<String>,
    comment: Option<String>,
    state_patch: Option<Value>,
}

async fn approve_execution(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ApproveRequest>,
) -> Result<Json<Value>, ApiError> {
    let execution_id = parse_execution_id(&id)?;

    let decision = match body.decision.as_str() {
        "approved" => jamjet_state::event::ApprovalDecision::Approved,
        "rejected" => jamjet_state::event::ApprovalDecision::Rejected,
        other => return Err(ApiError::BadRequest(format!("unknown decision: {other}"))),
    };

    let seq = state.backend.latest_sequence(&execution_id).await? + 1;
    let event = jamjet_state::Event::new(
        execution_id.clone(),
        seq,
        jamjet_state::EventKind::ApprovalReceived {
            node_id: body.node_id.unwrap_or_default(),
            user_id: body.user_id.unwrap_or_else(|| "anonymous".into()),
            decision,
            comment: body.comment,
            state_patch: body.state_patch,
        },
    );
    state.backend.append_event(event).await?;

    // Resume paused execution.
    if let Ok(Some(exec)) = state.backend.get_execution(&execution_id).await {
        if exec.status == WorkflowStatus::Paused {
            state
                .backend
                .update_execution_status(&execution_id, WorkflowStatus::Running)
                .await?;
        }
    }

    Ok(Json(json!({ "execution_id": id, "accepted": true })))
}

#[derive(Deserialize)]
struct ExternalEventRequest {
    correlation_key: String,
    payload: Value,
}

async fn send_external_event(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ExternalEventRequest>,
) -> Result<Json<Value>, ApiError> {
    let execution_id = parse_execution_id(&id)?;

    let seq = state.backend.latest_sequence(&execution_id).await? + 1;
    let event = jamjet_state::Event::new(
        execution_id.clone(),
        seq,
        jamjet_state::EventKind::ExternalEventReceived {
            correlation_key: body.correlation_key,
            payload: body.payload,
        },
    );
    state.backend.append_event(event).await?;

    Ok(Json(json!({ "execution_id": id, "accepted": true })))
}

// ── Agents ───────────────────────────────────────────────────────────────────

async fn register_agent(
    State(state): State<AppState>,
    Json(body): Json<AgentCard>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let agent_id = state
        .agents
        .register(body)
        .await
        .map_err(ApiError::Internal)?;
    Ok((StatusCode::CREATED, Json(json!({ "agent_id": agent_id }))))
}

#[derive(Deserialize)]
struct ListAgentsQuery {
    status: Option<String>,
    skill: Option<String>,
    protocol: Option<String>,
}

async fn list_agents(
    State(state): State<AppState>,
    Query(params): Query<ListAgentsQuery>,
) -> Result<Json<Value>, ApiError> {
    let status = params.status.as_deref().and_then(|s| match s {
        "registered" => Some(AgentStatus::Registered),
        "active" => Some(AgentStatus::Active),
        "paused" => Some(AgentStatus::Paused),
        "deactivated" => Some(AgentStatus::Deactivated),
        _ => None,
    });
    let filter = AgentFilter {
        status,
        skill: params.skill,
        protocol: params.protocol,
    };
    let agents = state
        .agents
        .find(filter)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "agents": agents })))
}

async fn get_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|_| ApiError::BadRequest(format!("invalid agent id: {id}")))?;
    let agent = state
        .agents
        .get(uuid)
        .await
        .map_err(ApiError::Internal)?
        .ok_or_else(|| ApiError::NotFound(format!("agent {id}")))?;
    Ok(Json(serde_json::to_value(agent).unwrap()))
}

async fn activate_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|_| ApiError::BadRequest(format!("invalid agent id: {id}")))?;
    state
        .agents
        .update_status(uuid, AgentStatus::Active)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "agent_id": id, "status": "active" })))
}

async fn deactivate_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|_| ApiError::BadRequest(format!("invalid agent id: {id}")))?;
    state
        .agents
        .update_status(uuid, AgentStatus::Deactivated)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "agent_id": id, "status": "deactivated" })))
}

// ── Admin ────────────────────────────────────────────────────────────────────

async fn list_workers(State(_state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({ "workers": [] })))
}

// ── Agent discovery ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DiscoverAgentRequest {
    url: String,
}

/// `POST /agents/discover` — fetch remote Agent Card and register it (F2.3).
async fn discover_agent(
    State(state): State<AppState>,
    Json(body): Json<DiscoverAgentRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let agent = state
        .agents
        .discover_remote(&body.url)
        .await
        .map_err(ApiError::Internal)?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(&agent).unwrap()),
    ))
}

/// `POST /agents/:id/heartbeat` — record agent heartbeat (F2.6).
async fn agent_heartbeat(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|_| ApiError::BadRequest(format!("invalid agent id: {id}")))?;
    state
        .agents
        .heartbeat(uuid)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "agent_id": id, "ok": true })))
}

// ── DID Document publishing (I2.2) ───────────────────────────────────────────

/// `GET /.well-known/did.json` — serve the runtime's W3C DID Document.
///
/// Builds a `did:web` document from all active registered agents. Each active
/// agent is listed as an A2A service endpoint. The DID host is derived from
/// `JAMJET_PUBLIC_URL` (preferred) or `JAMJET_BIND`:`JAMJET_PORT`.
async fn serve_did_document(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let agents = state
        .agents
        .find(AgentFilter {
            status: Some(AgentStatus::Active),
            skill: None,
            protocol: None,
        })
        .await
        .map_err(ApiError::Internal)?;

    let public_url = std::env::var("JAMJET_PUBLIC_URL").unwrap_or_else(|_| {
        let bind = std::env::var("JAMJET_BIND").unwrap_or_else(|_| "localhost".into());
        let port = std::env::var("JAMJET_PORT").unwrap_or_else(|_| "7700".into());
        format!("http://{}:{}", bind, port)
    });

    // did:web:<host> or did:web:<host>:<path>
    let did_host = public_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .replace('/', ":");
    let did_id = format!("did:web:{did_host}");

    let services: Vec<Value> = agents
        .iter()
        .map(|agent| {
            let agent_name = &agent.card.name;
            json!({
                "id": format!("#{}", agent.id),
                "type": "A2AService",
                "serviceEndpoint": format!("{}/agents/{}", public_url, agent.id),
                "name": agent_name,
            })
        })
        .collect();

    Ok(Json(json!({
        "@context": ["https://www.w3.org/ns/did/v1"],
        "id": did_id,
        "service": services,
    })))
}

// ── Audit log ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AuditQueryParams {
    execution_id: Option<String>,
    actor_id: Option<String>,
    event_type: Option<String>,
    #[serde(default = "default_audit_limit")]
    limit: u32,
    #[serde(default)]
    offset: u32,
}

fn default_audit_limit() -> u32 {
    50
}

async fn list_audit_log(
    State(state): State<AppState>,
    Query(params): Query<AuditQueryParams>,
) -> Result<Json<Value>, ApiError> {
    let q = AuditQuery {
        execution_id: params.execution_id,
        actor_id: params.actor_id,
        event_type: params.event_type,
        limit: params.limit.min(200),
        offset: params.offset,
        from: None,
        to: None,
    };

    let total = state
        .audit
        .count(&q)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let entries = state
        .audit
        .query(&q)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(json!({
        "items": entries,
        "total": total,
        "limit": q.limit,
        "offset": q.offset,
    })))
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn parse_execution_id(s: &str) -> Result<ExecutionId, ApiError> {
    // Format: exec_<32-char-hex> (UUID simple format)
    let hex = s.strip_prefix("exec_").unwrap_or(s);
    let uuid = uuid::Uuid::parse_str(hex)
        .map_err(|_| ApiError::BadRequest(format!("invalid execution id: {s}")))?;
    Ok(ExecutionId(uuid))
}
