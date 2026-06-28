use crate::auth::{require_auth, require_write_role, AuthState};
use crate::cron::{create_cron, delete_cron, list_cron};
use crate::error::ApiError;
use crate::state::AppState;
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Extension, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};

/// Maximum request body for `POST /artifacts`. Set EXPLICITLY (rather than
/// inheriting axum's implicit 2 MiB default) so the cap on the content-addressed
/// store is an intentional contract: artifacts are developer-supplied blobs
/// (tool outputs, small files), so a few MiB is the sane ceiling. Bodies over
/// this are rejected with `413 Payload Too Large` before the handler runs.
const ARTIFACT_MAX_BODY_BYTES: usize = 8 * 1024 * 1024; // 8 MiB
use chrono::Utc;
use jamjet_agents::{AgentCard, AgentFilter, AgentStatus};
use jamjet_audit::backend::AuditQuery;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{Tenant, TenantId, TenantStatus, WorkItem, WorkflowDefinition};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

/// Build the Axum router with all API routes.
pub fn build_router(state: AppState) -> Router {
    build_router_with_opts(state, false)
}

/// Build the Axum router, optionally skipping auth for dev mode.
pub fn build_router_with_opts(state: AppState, dev_mode: bool) -> Router {
    let auth_state = AuthState {
        backend: state.backend.clone(),
    };

    // Core API routes.
    let api_routes = Router::new()
        // Workflow definitions
        .route("/workflows", post(create_workflow))
        // Cron schedules (local scheduling)
        .route("/cron", post(create_cron).get(list_cron))
        .route("/cron/:name", delete(delete_cron))
        // Executions
        .route("/executions", post(start_execution).get(list_executions))
        .route("/executions/:id", get(get_execution))
        .route("/executions/:id/cancel", post(cancel_execution))
        .route("/executions/:id/events", get(list_events))
        .route("/executions/:id/approve", post(approve_execution))
        .route(
            "/executions/:id/approvals",
            get(list_approvals_for_execution),
        )
        .route("/executions/:id/external-event", post(send_external_event))
        // Artifacts (content-addressed store) — tenant-scoped developer API.
        // The POST carries an explicit body limit (see ARTIFACT_MAX_BODY_BYTES).
        .route(
            "/artifacts",
            post(put_artifact).layer(DefaultBodyLimit::max(ARTIFACT_MAX_BODY_BYTES)),
        )
        .route("/artifacts/:hash", get(get_artifact))
        // Agents
        .route("/agents", post(register_agent).get(list_agents))
        .route("/agents/discover", post(discover_agent))
        .route("/agents/:id", get(get_agent))
        .route("/agents/:id/activate", post(activate_agent))
        .route("/agents/:id/deactivate", post(deactivate_agent))
        .route("/agents/:id/heartbeat", post(agent_heartbeat))
        // Work items (worker protocol)
        .route("/work-items", post(enqueue_work_item))
        .route("/work-items/claim", post(claim_work_item))
        .route("/work-items/:id/complete", post(complete_work_item))
        .route("/work-items/:id/fail", post(fail_work_item))
        .route("/work-items/:id/heartbeat", post(heartbeat_work_item))
        // Admin
        .route("/workers", get(list_workers))
        // Tenant management (operator-only)
        .route("/tenants", post(create_tenant).get(list_tenants))
        .route("/tenants/:id", get(get_tenant).put(update_tenant))
        // Audit log — immutable, append-only
        .route("/audit", get(list_audit_log))
        // Coordinator decisions and agent search
        .route(
            "/executions/:id/coordinator-decisions",
            get(list_coordinator_decisions),
        )
        .route(
            "/executions/:id/nodes/:node_id/scoring",
            get(get_node_scoring),
        )
        .route(
            "/executions/:id/nodes/:node_id/reasoning",
            get(get_node_reasoning),
        )
        .route("/agents/search", get(search_agents));

    // In dev mode, skip auth and inject a default tenant. In production, require Bearer token.
    let protected = if dev_mode {
        api_routes
            .layer(middleware::from_fn(inject_dev_tenant))
            .with_state(state.clone())
    } else {
        api_routes
            .layer(middleware::from_fn(require_write_role))
            .layer(middleware::from_fn_with_state(auth_state, require_auth))
            .with_state(state.clone())
    };

    // MCP bridge — unauthenticated, local-only.
    let mcp_bridge = crate::mcp_bridge::build_mcp_bridge(state.clone());

    // Public routes — no auth required.
    Router::new()
        .route("/health", get(health))
        .route("/.well-known/did.json", get(serve_did_document))
        .merge(protected)
        .with_state(state)
        .merge(mcp_bridge)
        .fallback(crate::static_files::serve_spa)
}

// ── Dev-mode middleware ──────────────────────────────────────────────────────

/// Injects a default tenant extension for dev mode (no auth required).
async fn inject_dev_tenant(
    mut req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    req.extensions_mut()
        .insert(TenantId::from("default".to_string()));
    next.run(req).await
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
    Extension(tenant_id): Extension<TenantId>,
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

    // Reject IR the runtime can't load. Without this, a structurally-broken
    // definition is stored happily and only fails later when the scheduler tries
    // to deserialize it — at which point the execution silently never schedules.
    // (Reference resolution is deliberately left to the runtime: models/tools are
    // resolved against the worker registry, not the IR maps, so we validate the
    // shape here, not `validate_workflow`'s ref rules.)
    serde_json::from_value::<jamjet_ir::WorkflowIr>(body.ir.clone())
        .map_err(|e| ApiError::BadRequest(format!("invalid workflow IR: {e}")))?;

    let backend = state.backend_for(&tenant_id);
    let def = WorkflowDefinition {
        workflow_id: workflow_id.clone(),
        version: version.clone(),
        ir: body.ir,
        created_at: Utc::now(),
        tenant_id: tenant_id.0.clone(),
    };
    backend.store_workflow(def).await?;

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
    Extension(tenant_id): Extension<TenantId>,
    Json(body): Json<StartExecutionRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let backend = state.backend_for(&tenant_id);
    let version = body.workflow_version.unwrap_or_else(|| "1.0.0".into());

    // Verify the workflow definition exists (within this tenant).
    let def = backend
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
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    };
    let execution_id = execution.execution_id.clone();
    backend.create_execution(execution).await?;

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
    backend.append_event(event).await?;

    // Immediately enqueue the start node as a work item, routed to the queue its
    // kind requires (e.g. a Model start node -> the "model" queue), mirroring the
    // scheduler's logic for chained nodes.
    let queue_type = def
        .ir
        .get("nodes")
        .and_then(|nodes| nodes.get(&start_node))
        .and_then(|node| node.get("kind"))
        .and_then(|kind| serde_json::from_value::<jamjet_core::node::NodeKind>(kind.clone()).ok())
        .and_then(|k| serde_json::to_value(k.queue_type()).ok())
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "general".to_string());
    let sched_event = jamjet_state::Event::new(
        execution_id.clone(),
        2,
        jamjet_state::EventKind::NodeScheduled {
            node_id: start_node.clone(),
            queue_type: queue_type.clone(),
        },
    );
    backend.append_event(sched_event).await?;

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
        lease_fence: 0,
        tenant_id: tenant_id.0.clone(),
    };
    backend.enqueue_work_item(work_item).await?;

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
    Extension(tenant_id): Extension<TenantId>,
    Query(params): Query<ListExecutionsQuery>,
) -> Result<Json<Value>, ApiError> {
    let backend = state.backend_for(&tenant_id);
    let status = params.status.as_deref().and_then(|s| match s {
        "running" => Some(WorkflowStatus::Running),
        "paused" => Some(WorkflowStatus::Paused),
        "completed" => Some(WorkflowStatus::Completed),
        "failed" => Some(WorkflowStatus::Failed),
        _ => None,
    });
    let executions = backend
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
    Extension(tenant_id): Extension<TenantId>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let backend = state.backend_for(&tenant_id);
    let execution_id = parse_execution_id(&id)?;
    let execution = backend
        .get_execution(&execution_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("execution {id}")))?;
    Ok(Json(serde_json::to_value(execution).map_err(|e| {
        ApiError::Internal(format!("serialize execution: {e}"))
    })?))
}

async fn cancel_execution(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let backend = state.backend_for(&tenant_id);
    let execution_id = parse_execution_id(&id)?;

    let execution = backend
        .get_execution(&execution_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("execution {id}")))?;

    if execution.status.is_terminal() {
        return Err(ApiError::BadRequest(format!(
            "execution {id} is already in terminal state: {:?}",
            execution.status
        )));
    }

    let seq = backend.latest_sequence(&execution_id).await? + 1;
    let event = jamjet_state::Event::new(
        execution_id.clone(),
        seq,
        jamjet_state::EventKind::WorkflowCancelled {
            reason: Some("user request".into()),
        },
    );
    backend.append_event(event).await?;
    backend
        .update_execution_status(&execution_id, WorkflowStatus::Cancelled)
        .await?;

    Ok(Json(json!({ "execution_id": id, "status": "cancelled" })))
}

async fn list_events(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let backend = state.backend_for(&tenant_id);
    let execution_id = parse_execution_id(&id)?;
    let mut events = backend.get_events(&execution_id).await?;

    // Resolve ArtifactRef sentinels in NodeCompleted.output before returning to
    // the client.  The write path (2i-2) spills large outputs to the artifact
    // store before commit_turn and replaces the inline value with a
    // {"$artifact": {...}} sentinel; here we fetch and restore the original.
    //
    // A missing artifact (dangling ref) is impossible by the put-then-commit
    // write-order invariant, but is handled gracefully: the sentinel is kept and
    // an "unresolved": true flag is added inside the $artifact object.  Never
    // panics; logs a WARN so the anomaly is visible.
    for event in &mut events {
        if let jamjet_state::EventKind::NodeCompleted {
            output, node_id, ..
        } = &mut event.kind
        {
            let resolved = jamjet_state::resolve_value(output, &*backend).await;
            match resolved {
                Ok(v) => *output = v,
                Err(e) => {
                    tracing::warn!(
                        node_id = %node_id,
                        error = %e,
                        "artifact resolve failed for NodeCompleted output; \
                         returning sentinel with unresolved=true"
                    );
                    // Add unresolved=true inside the $artifact inner object so
                    // callers can detect the anomaly without a 5xx.
                    if let Some(inner) = output.get_mut(jamjet_state::ARTIFACT_SENTINEL_KEY) {
                        if let Some(obj) = inner.as_object_mut() {
                            obj.insert("unresolved".to_string(), Value::Bool(true));
                        }
                    }
                }
            }
        }
    }

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
    Extension(tenant_id): Extension<TenantId>,
    Path(id): Path<String>,
    Json(body): Json<ApproveRequest>,
) -> Result<Json<Value>, ApiError> {
    let backend = state.backend_for(&tenant_id);
    let execution_id = parse_execution_id(&id)?;

    let decision = match body.decision.as_str() {
        "approved" => jamjet_state::event::ApprovalDecision::Approved,
        "rejected" => jamjet_state::event::ApprovalDecision::Rejected,
        other => return Err(ApiError::BadRequest(format!("unknown decision: {other}"))),
    };

    let (node_id, event) = crate::approvals::submit_approval(
        &backend,
        &execution_id,
        crate::approvals::ApprovalSubmission {
            node_id: body.node_id,
            user_id: body.user_id.unwrap_or_else(|| "anonymous".into()),
            decision,
            comment: body.comment,
            state_patch: body.state_patch,
        },
    )
    .await
    .map_err(|e| match e {
        crate::approvals::SubmitError::MultiplePending(_) => ApiError::BadRequest(e.to_string()),
        crate::approvals::SubmitError::NoPending
        | crate::approvals::SubmitError::NodeNotPending(_)
        | crate::approvals::SubmitError::ExecutionTerminal(_) => ApiError::Conflict(e.to_string()),
        crate::approvals::SubmitError::Backend(msg) => ApiError::Internal(msg),
    })?;

    // Seal this approval into the signed, hash-chained audit log. This runs
    // *after* the event is durably appended, never fails the request
    // (`enrich_and_append` warns-and-continues on a write error), and is off
    // the worker's fenced commit hot path — so it adds tamper-evident audit
    // without touching the durability invariant. The per-tool-call / per-node
    // worker events emitted through the fenced `commit_turn` path are not yet
    // routed through the enricher (that needs the enricher threaded into the
    // worker pool); tracked as F-t3-audit-emit.
    let ctx = jamjet_audit::RequestContext {
        actor_type: jamjet_audit::ActorType::Human,
        tenant_id: tenant_id.0.clone(),
        method: Some("POST".to_string()),
        path: Some(format!("/executions/{id}/approve")),
        ..Default::default()
    };
    state.enricher.enrich_and_append(&event, Some(&ctx)).await;

    Ok(Json(
        json!({ "execution_id": id, "node_id": node_id, "accepted": true }),
    ))
}

async fn list_approvals_for_execution(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let backend = state.backend_for(&tenant_id);
    let execution_id = parse_execution_id(&id)?;

    // Fast path: serve from the durable projection (eventually-consistent; lags
    // the write path by up to one projector tick, ~500 ms).
    //
    // Fallback to event-log replay when the projection is empty.  This covers:
    // - Running executions not yet visited by the projector (no tick yet).
    // - Terminal executions that completed before a projector tick (projector
    //   only scans Running; their events were never folded into proj_approvals).
    // - Non-default-tenant executions (the projector runs on the base backend
    //   and writes proj_approvals with tenant_id='default'; reading via
    //   backend_for(&tenant_id) returns nothing for the real tenant).
    // - Genuinely-no-approval executions: event replay also returns empty, correct.
    //
    // Follow-up: F-2h-tenant (thread tenant into projector writes) + F-2h-terminal
    // (project terminal executions) will close the coverage gap so the fallback
    // shrinks to a thin cold-start edge case.
    let rows = backend.get_approval_projection(&execution_id).await?;

    if !rows.is_empty() {
        // Projection fast path: build the response from the durable read model.
        let mut pending: Vec<serde_json::Value> = Vec::new();
        let mut decided: Vec<serde_json::Value> = Vec::new();

        for row in &rows {
            match row.status.as_str() {
                "pending" => {
                    pending.push(serde_json::json!({
                        "node_id":   row.node_id,
                        "tool_name": row.tool_name,
                        "approver":  row.approver,
                        "context":   row.context,
                        "sequence":  row.last_sequence,
                    }));
                }
                "approved" => {
                    decided.push(serde_json::json!({
                        "node_id":  row.node_id,
                        "status":   "approved",
                        "user_id":  row.user_id,
                        "sequence": row.last_sequence,
                    }));
                }
                "rejected" => {
                    decided.push(serde_json::json!({
                        "node_id":  row.node_id,
                        "status":   "rejected",
                        "user_id":  row.user_id,
                        "comment":  row.comment,
                        "sequence": row.last_sequence,
                    }));
                }
                _ => {
                    // Unknown status: skip rather than panic.
                }
            }
        }

        Ok(Json(
            serde_json::json!({ "pending": pending, "decided": decided }),
        ))
    } else {
        // Event-log fallback: guarantees the endpoint is never worse than before
        // the projection switch (2h-3).  Covers unprojected, terminal, and
        // non-default-tenant executions until F-2h-tenant + F-2h-terminal land.
        let events = backend.get_events(&execution_id).await?;
        Ok(Json(crate::approvals::approvals_view(&events)))
    }
}

#[derive(Deserialize)]
struct ExternalEventRequest {
    correlation_key: String,
    payload: Value,
}

async fn send_external_event(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
    Path(id): Path<String>,
    Json(body): Json<ExternalEventRequest>,
) -> Result<Json<Value>, ApiError> {
    let backend = state.backend_for(&tenant_id);
    let execution_id = parse_execution_id(&id)?;

    let seq = backend.latest_sequence(&execution_id).await? + 1;
    let event = jamjet_state::Event::new(
        execution_id.clone(),
        seq,
        jamjet_state::EventKind::ExternalEventReceived {
            correlation_key: body.correlation_key,
            payload: body.payload,
        },
    );
    backend.append_event(event).await?;

    Ok(Json(json!({ "execution_id": id, "accepted": true })))
}

// ── Artifacts (content-addressed store) ──────────────────────────────────────

#[derive(Deserialize)]
struct PutArtifactQuery {
    /// Optional media type override. When present it wins over the
    /// `Content-Type` request header.
    media_type: Option<String>,
}

/// `POST /artifacts` — store raw bytes in the tenant-scoped content-addressed
/// store and return the resulting `ArtifactRef` as JSON.
///
/// The request body is the raw artifact bytes. The media type is taken from the
/// `?media_type=` query parameter if present, otherwise from the `Content-Type`
/// request header. Writes go through the TENANT-SCOPED backend
/// (`backend_for(&tenant_id)`) so artifacts are isolated per tenant — never the
/// `'default'`-pinned base path.
///
/// Returns `200 { "hash": <sha256 hex>, "size": <bytes>, "media_type": <type|null> }`.
///
/// The route carries an explicit `DefaultBodyLimit` of `ARTIFACT_MAX_BODY_BYTES`;
/// bodies larger than that are rejected with `413` before this handler runs.
async fn put_artifact(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
    Query(query): Query<PutArtifactQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let media_type = query.media_type.or_else(|| {
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    });
    let backend = state.backend_for(&tenant_id);
    let artifact_ref = backend.put_artifact(&body, media_type.as_deref()).await?;
    Ok(Json(json!({
        "hash": artifact_ref.hash,
        "size": artifact_ref.size,
        "media_type": artifact_ref.media_type,
    })))
}

/// `GET /artifacts/:hash` — fetch artifact bytes from the tenant-scoped store.
///
/// Returns `200` with the raw bytes or `404` when no artifact with that hash
/// exists for the caller's tenant. Reads go through `backend_for(&tenant_id)`
/// so a tenant can only read its own artifacts.
///
/// The response `Content-Type` is `application/octet-stream`: `get_artifact`
/// yields only the bytes, not the stored media type (that is returned on the
/// `POST /artifacts` response). Surfacing the media type on GET is a follow-up
/// (F-2i-media-type) that would need the backend to return it alongside bytes.
async fn get_artifact(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
    Path(hash): Path<String>,
) -> Result<Response, ApiError> {
    let backend = state.backend_for(&tenant_id);
    match backend.get_artifact(&hash).await? {
        // `Vec<u8>` renders as `application/octet-stream` by default.
        Some(bytes) => Ok(bytes.into_response()),
        None => Err(ApiError::NotFound(format!("artifact {hash}"))),
    }
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
    Ok(Json(serde_json::to_value(agent).map_err(|e| {
        ApiError::Internal(format!("serialize agent: {e}"))
    })?))
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
        Json(
            serde_json::to_value(&agent)
                .map_err(|e| ApiError::Internal(format!("serialize agent: {e}")))?,
        ),
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
    Extension(tenant_id): Extension<TenantId>,
    Query(params): Query<AuditQueryParams>,
) -> Result<Json<Value>, ApiError> {
    let q = AuditQuery {
        execution_id: params.execution_id,
        actor_id: params.actor_id,
        event_type: params.event_type,
        tenant_id: Some(tenant_id.0),
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

// ── Tenant management ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateTenantRequest {
    id: String,
    name: String,
}

async fn create_tenant(
    State(state): State<AppState>,
    Json(body): Json<CreateTenantRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let now = Utc::now();
    let tenant = Tenant {
        id: TenantId::from(body.id.clone()),
        name: body.name,
        status: TenantStatus::Active,
        policy: None,
        limits: None,
        created_at: now,
        updated_at: now,
    };
    // Use any scoped backend (tenant CRUD is cross-tenant).
    let backend = state.backend_for(&TenantId::default());
    backend.create_tenant(tenant).await?;
    Ok((StatusCode::CREATED, Json(json!({ "tenant_id": body.id }))))
}

async fn list_tenants(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let backend = state.backend_for(&TenantId::default());
    let tenants = backend.list_tenants().await?;
    Ok(Json(json!({ "tenants": tenants })))
}

async fn get_tenant(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let backend = state.backend_for(&TenantId::default());
    let tenant = backend
        .get_tenant(&TenantId::from(id.as_str()))
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("tenant {id}")))?;
    Ok(Json(serde_json::to_value(tenant).map_err(|e| {
        ApiError::Internal(format!("serialize tenant: {e}"))
    })?))
}

#[derive(Deserialize)]
struct UpdateTenantRequest {
    name: Option<String>,
    status: Option<String>,
    policy: Option<Value>,
    limits: Option<Value>,
}

async fn update_tenant(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateTenantRequest>,
) -> Result<Json<Value>, ApiError> {
    let backend = state.backend_for(&TenantId::default());
    let tid = TenantId::from(id.as_str());
    let existing = backend
        .get_tenant(&tid)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("tenant {id}")))?;

    let limits = body
        .limits
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| ApiError::BadRequest(format!("invalid limits: {e}")))?;

    let updated = Tenant {
        id: tid.clone(),
        name: body.name.unwrap_or(existing.name),
        status: body
            .status
            .as_deref()
            .map(TenantStatus::parse)
            .unwrap_or(existing.status),
        policy: body.policy.or(existing.policy),
        limits: limits.or(existing.limits),
        created_at: existing.created_at,
        updated_at: Utc::now(),
    };
    backend.update_tenant(updated).await?;
    Ok(Json(json!({ "tenant_id": id, "updated": true })))
}

// ── Work items (worker protocol) ─────────────────────────────────────────────

#[derive(Deserialize)]
struct ClaimWorkItemRequest {
    worker_id: String,
    queue_types: Vec<String>,
}

/// `POST /work-items/claim` — claim the next available work item.
async fn claim_work_item(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
    Json(body): Json<ClaimWorkItemRequest>,
) -> Result<Json<Value>, ApiError> {
    let backend = state.backend_for(&tenant_id);
    let queue_refs: Vec<&str> = body.queue_types.iter().map(|s| s.as_str()).collect();
    let item = backend
        .claim_work_item(&body.worker_id, &queue_refs)
        .await?;
    match item {
        Some(wi) => Ok(Json(json!({
            "claimed": true,
            "work_item": {
                "id": wi.id.to_string(),
                "execution_id": wi.execution_id.to_string(),
                "node_id": wi.node_id,
                "queue_type": wi.queue_type,
                "payload": wi.payload,
                "attempt": wi.attempt,
            }
        }))),
        None => Ok(Json(json!({ "claimed": false }))),
    }
}

#[derive(Deserialize)]
struct CompleteWorkItemRequest {
    /// Execution ID for event emission (optional for backwards compat).
    execution_id: Option<String>,
    /// Node ID for event emission.
    node_id: Option<String>,
    output: Value,
    state_patch: Value,
    #[serde(default)]
    duration_ms: u64,
    // ── GenAI telemetry (forwarded from the python_tool worker or other callers) ──
    /// AI provider system (e.g. "anthropic", "openai").
    #[serde(default)]
    gen_ai_system: Option<String>,
    /// Model name used.
    #[serde(default)]
    gen_ai_model: Option<String>,
    /// Input tokens consumed.
    #[serde(default)]
    input_tokens: Option<u64>,
    /// Output tokens generated.
    #[serde(default)]
    output_tokens: Option<u64>,
    /// Finish reason (e.g. "stop", "length", "tool_calls").
    #[serde(default)]
    finish_reason: Option<String>,
}

/// `POST /work-items/:id/complete` — mark a work item as completed and emit NodeCompleted event.
async fn complete_work_item(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
    Path(id): Path<String>,
    Json(body): Json<CompleteWorkItemRequest>,
) -> Result<Json<Value>, ApiError> {
    let item_id = Uuid::parse_str(&id)
        .map_err(|_| ApiError::BadRequest(format!("invalid work item id: {id}")))?;
    let backend = state.backend_for(&tenant_id);

    // Mark the work item as completed.
    backend.complete_work_item(item_id).await?;

    // Emit NodeCompleted event if execution_id and node_id are provided.
    if let (Some(exec_id_str), Some(node_id)) = (&body.execution_id, &body.node_id) {
        let execution_id = parse_execution_id(exec_id_str)?;
        let seq = backend.latest_sequence(&execution_id).await? + 1;
        let event = jamjet_state::Event::new(
            execution_id.clone(),
            seq,
            jamjet_state::EventKind::NodeCompleted {
                node_id: node_id.clone(),
                output: body.output.clone(),
                state_patch: body.state_patch.clone(),
                duration_ms: body.duration_ms,
                gen_ai_system: body.gen_ai_system.clone(),
                gen_ai_model: body.gen_ai_model.clone(),
                input_tokens: body.input_tokens,
                output_tokens: body.output_tokens,
                finish_reason: body.finish_reason.clone(),
                cost_usd: None,
                provenance: None,
                idempotency_key: None,
            },
        );
        backend.append_event(event).await?;

        // Apply state_patch to the execution's current_state.
        if let Ok(Some(mut exec)) = backend.get_execution(&execution_id).await {
            if let Some(state_obj) = exec.current_state.as_object_mut() {
                if let Some(patch_obj) = body.state_patch.as_object() {
                    for (k, v) in patch_obj {
                        state_obj.insert(k.clone(), v.clone());
                    }
                }
            }
            let _ = backend
                .update_execution_current_state(&execution_id, &exec.current_state)
                .await;
        }
    }

    Ok(Json(json!({ "completed": true, "work_item_id": id })))
}

#[derive(Deserialize)]
struct EnqueueWorkItemRequest {
    execution_id: String,
    node_id: String,
    #[serde(default = "default_queue_type")]
    queue_type: String,
    #[serde(default)]
    payload: Value,
}

fn default_queue_type() -> String {
    "general".to_string()
}

/// `POST /work-items` — enqueue a new work item for a node.
async fn enqueue_work_item(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
    Json(body): Json<EnqueueWorkItemRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let execution_id = parse_execution_id(&body.execution_id)?;
    let backend = state.backend_for(&tenant_id);

    // Emit NodeScheduled event.
    let seq = backend.latest_sequence(&execution_id).await? + 1;
    backend
        .append_event(jamjet_state::Event::new(
            execution_id.clone(),
            seq,
            jamjet_state::EventKind::NodeScheduled {
                node_id: body.node_id.clone(),
                queue_type: body.queue_type.clone(),
            },
        ))
        .await?;

    // Enqueue the work item.
    let item = WorkItem {
        id: Uuid::new_v4(),
        execution_id,
        node_id: body.node_id,
        queue_type: body.queue_type,
        payload: body.payload,
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: tenant_id.0.clone(),
    };
    let item_id = backend.enqueue_work_item(item).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "work_item_id": item_id.to_string() })),
    ))
}

#[derive(Deserialize)]
struct FailWorkItemRequest {
    error: String,
}

/// `POST /work-items/:id/fail` — mark a work item as failed.
async fn fail_work_item(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
    Path(id): Path<String>,
    Json(body): Json<FailWorkItemRequest>,
) -> Result<Json<Value>, ApiError> {
    let item_id = Uuid::parse_str(&id)
        .map_err(|_| ApiError::BadRequest(format!("invalid work item id: {id}")))?;
    let backend = state.backend_for(&tenant_id);
    backend.fail_work_item(item_id, &body.error).await?;
    Ok(Json(json!({ "failed": true, "work_item_id": id })))
}

#[derive(Deserialize)]
struct HeartbeatRequest {
    worker_id: String,
    /// The lease fence the worker received when it claimed the item. A renew
    /// presenting a stale fence (lease stolen / failed over) fails closed.
    #[serde(default)]
    lease_fence: i64,
}

/// `POST /work-items/:id/heartbeat` — renew the lease on a claimed work item.
async fn heartbeat_work_item(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
    Path(id): Path<String>,
    Json(body): Json<HeartbeatRequest>,
) -> Result<Json<Value>, ApiError> {
    let item_id = Uuid::parse_str(&id)
        .map_err(|_| ApiError::BadRequest(format!("invalid work item id: {id}")))?;
    let backend = state.backend_for(&tenant_id);
    backend
        .renew_lease(item_id, &body.worker_id, body.lease_fence)
        .await?;
    Ok(Json(json!({ "renewed": true, "work_item_id": id })))
}

// ── Coordinator decisions ────────────────────────────────────────────────────

/// `GET /executions/:id/coordinator-decisions`
///
/// Returns all coordinator events (discovery, scoring, decision) for an execution,
/// in sequence order.
async fn list_coordinator_decisions(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let backend = state.backend_for(&tenant_id);
    let execution_id = parse_execution_id(&id)?;
    let events = backend.get_events(&execution_id).await?;

    let coordinator_events: Vec<&jamjet_state::Event> = events
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                jamjet_state::EventKind::CoordinatorDiscovery { .. }
                    | jamjet_state::EventKind::CoordinatorScoring { .. }
                    | jamjet_state::EventKind::CoordinatorDecision { .. }
            )
        })
        .collect();

    Ok(Json(json!({ "events": coordinator_events })))
}

/// `GET /executions/:id/nodes/:node_id/scoring`
///
/// Returns the coordinator scoring breakdown for a specific node.
async fn get_node_scoring(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
    Path((id, node_id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let backend = state.backend_for(&tenant_id);
    let execution_id = parse_execution_id(&id)?;
    let events = backend.get_events(&execution_id).await?;

    let scoring: Vec<&jamjet_state::Event> = events
        .iter()
        .filter(|e| {
            if let jamjet_state::EventKind::CoordinatorScoring {
                node_id: ref nid, ..
            } = e.kind
            {
                nid.as_str() == node_id
            } else {
                false
            }
        })
        .collect();

    if scoring.is_empty() {
        return Err(ApiError::NotFound(format!(
            "no scoring events for node {node_id} in execution {id}"
        )));
    }

    Ok(Json(json!({ "node_id": node_id, "scoring": scoring })))
}

/// `GET /executions/:id/nodes/:node_id/reasoning`
///
/// Returns the coordinator decision/reasoning for a specific node.
async fn get_node_reasoning(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
    Path((id, node_id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let backend = state.backend_for(&tenant_id);
    let execution_id = parse_execution_id(&id)?;
    let events = backend.get_events(&execution_id).await?;

    let decisions: Vec<&jamjet_state::Event> = events
        .iter()
        .filter(|e| {
            if let jamjet_state::EventKind::CoordinatorDecision {
                node_id: ref nid, ..
            } = e.kind
            {
                nid.as_str() == node_id
            } else {
                false
            }
        })
        .collect();

    if decisions.is_empty() {
        return Err(ApiError::NotFound(format!(
            "no decision events for node {node_id} in execution {id}"
        )));
    }

    Ok(Json(json!({ "node_id": node_id, "decisions": decisions })))
}

// ── Agent search ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SearchAgentsQuery {
    /// Comma-separated list of required skill names.
    skills: Option<String>,
    /// Trust domain label to filter by (matches `labels["trust_domain"]`).
    trust_domain: Option<String>,
}

/// `GET /agents/search`
///
/// Search agents by skills and/or trust domain. `skills` is a comma-separated
/// list; an agent must possess all listed skills to be included. `trust_domain`
/// is matched against the agent's `labels["trust_domain"]` label.
async fn search_agents(
    State(state): State<AppState>,
    Query(params): Query<SearchAgentsQuery>,
) -> Result<Json<Value>, ApiError> {
    // Start with all active agents (no status filter means all statuses).
    let all_agents = state
        .agents
        .find(AgentFilter {
            status: None,
            skill: None,
            protocol: None,
        })
        .await
        .map_err(ApiError::Internal)?;

    let required_skills: Vec<String> = params
        .skills
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let agents: Vec<_> = all_agents
        .into_iter()
        .filter(|agent| {
            // Filter by skills: agent must have all required skills.
            if !required_skills.is_empty() {
                let agent_skills: Vec<&str> = agent
                    .card
                    .capabilities
                    .skills
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect();
                if !required_skills
                    .iter()
                    .all(|req| agent_skills.contains(&req.as_str()))
                {
                    return false;
                }
            }

            // Filter by trust_domain label.
            if let Some(ref td) = params.trust_domain {
                if agent.card.labels.get("trust_domain").map(|v| v.as_str()) != Some(td.as_str()) {
                    return false;
                }
            }

            true
        })
        .collect();

    Ok(Json(json!({ "agents": agents })))
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn parse_execution_id(s: &str) -> Result<ExecutionId, ApiError> {
    // Format: exec_<32-char-hex> (UUID simple format)
    let hex = s.strip_prefix("exec_").unwrap_or(s);
    let uuid = uuid::Uuid::parse_str(hex)
        .map_err(|_| ApiError::BadRequest(format!("invalid execution id: {s}")))?;
    Ok(ExecutionId(uuid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use jamjet_agents::InMemoryAgentRegistry;
    use jamjet_audit::{AuditEnricher, NoopAuditBackend};
    use jamjet_state::InMemoryBackend;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Build an `AppState` whose `backend_for` returns a DISTINCT in-memory
    /// backend per tenant id (created on first use). This lets a test prove the
    /// artifact routes are genuinely tenant-scoped: the real
    /// `TenantScopedSqliteBackend` binds the tenant inside its SQL, and here a
    /// per-tenant backend stands in so bytes written under one tenant are
    /// invisible to another. The dev/prod HTTP middleware pins a single tenant,
    /// so the handlers are exercised directly with chosen `TenantId`s.
    fn tenant_routing_state() -> AppState {
        let backends: Arc<Mutex<HashMap<String, Arc<InMemoryBackend>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let backends_for = backends.clone();
        let audit: Arc<dyn jamjet_audit::AuditBackend> = Arc::new(NoopAuditBackend);
        let enricher = Arc::new(AuditEnricher::new(Arc::clone(&audit)));
        let base = Arc::new(InMemoryBackend::new());
        AppState {
            backend: base as Arc<dyn jamjet_state::StateBackend>,
            backend_for_fn: Arc::new(move |tenant_id: &TenantId| {
                let mut map = backends_for.lock().unwrap();
                let backend = map
                    .entry(tenant_id.0.clone())
                    .or_insert_with(|| Arc::new(InMemoryBackend::new()));
                backend.clone() as Arc<dyn jamjet_state::StateBackend>
            }),
            agents: Arc::new(InMemoryAgentRegistry::new()),
            audit,
            enricher,
            protocols: crate::state::default_protocol_registry(),
            cron_store: None,
        }
    }

    /// An artifact stored under tenant A must NOT be readable as tenant B, and
    /// the routes must thread the request's tenant into `backend_for` (not the
    /// `'default'` base path). Also covers the put -> get happy path and the
    /// `media_type` round-trip on the `POST` response.
    #[tokio::test]
    async fn artifacts_are_tenant_isolated() {
        let state = tenant_routing_state();
        let tenant_a = TenantId::from("tenant-a");
        let tenant_b = TenantId::from("tenant-b");

        // Store bytes under tenant A.
        let put = put_artifact(
            State(state.clone()),
            Extension(tenant_a.clone()),
            Query(PutArtifactQuery {
                media_type: Some("text/plain".into()),
            }),
            HeaderMap::new(),
            Bytes::from_static(b"secret-a"),
        )
        .await
        .expect("put under tenant A");
        let hash = put.0["hash"].as_str().expect("hash present").to_string();
        assert_eq!(hash.len(), 64, "hash is sha256 hex");
        assert_eq!(put.0["size"].as_u64(), Some(8));
        assert_eq!(put.0["media_type"], "text/plain");

        // Tenant A reads its own bytes back (happy path).
        let got_a = get_artifact(
            State(state.clone()),
            Extension(tenant_a.clone()),
            Path(hash.clone()),
        )
        .await
        .expect("tenant A reads its artifact");
        let body_a = got_a.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body_a[..], b"secret-a");

        // Tenant B must not see tenant A's artifact -> 404 (NotFound).
        let got_b = get_artifact(
            State(state.clone()),
            Extension(tenant_b.clone()),
            Path(hash.clone()),
        )
        .await;
        assert!(
            matches!(got_b, Err(ApiError::NotFound(_))),
            "tenant B must not read tenant A's artifact (tenant isolation)"
        );
    }

    /// `POST /artifacts` enforces the explicit body limit: a body over
    /// `ARTIFACT_MAX_BODY_BYTES` is rejected with `413`, while a small body is
    /// accepted — exercised through the real router so the `DefaultBodyLimit`
    /// layer (not just the handler) is on the path.
    #[tokio::test]
    async fn put_artifact_enforces_explicit_body_limit() {
        use tower::ServiceExt; // for `oneshot`

        let app = build_router_with_opts(tenant_routing_state(), /* dev_mode */ true);

        // Just over the cap -> 413 Payload Too Large.
        let oversized = vec![0u8; ARTIFACT_MAX_BODY_BYTES + 1];
        let too_big_req = axum::http::Request::builder()
            .method("POST")
            .uri("/artifacts")
            .header("content-type", "application/octet-stream")
            .body(axum::body::Body::from(oversized))
            .unwrap();
        let resp = app.clone().oneshot(too_big_req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "oversized artifact body must be rejected with 413"
        );

        // A small body still succeeds (the limit didn't break the happy path).
        let small_req = axum::http::Request::builder()
            .method("POST")
            .uri("/artifacts")
            .header("content-type", "text/plain")
            .body(axum::body::Body::from("ok"))
            .unwrap();
        let resp = app.oneshot(small_req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "a small artifact body must still be accepted"
        );
    }
}
