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
use jamjet_state::{FailOutcome, Tenant, TenantId, TenantStatus, WorkItem, WorkflowDefinition};
use jamjet_worker::DispatchGuardOutcome;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{info, warn};
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
    let parsed = serde_json::from_value::<jamjet_ir::WorkflowIr>(body.ir.clone())
        .map_err(|e| ApiError::BadRequest(format!("invalid workflow IR: {e}")))?;

    // ONE rule from `validate_workflow` runs here, deliberately, while the ref
    // rules above do not. An unmarked ADK tool-dispatch node is not a reference
    // problem the runtime can resolve later — it is a policy-enforcement hole,
    // and registration is the only place it is detectable. An IR compiled by a
    // pre-marker SDK deserializes with `agent_tool_dispatch: false`, so the
    // claim route treats it as an ordinary `python_fn` and hands out a whole
    // turn's model-chosen tool calls with no tool policy evaluated at all.
    // Storing it would bake that hole in permanently; rejecting it makes the
    // version skew loud at the one moment the operator can act on it.
    jamjet_ir::validate_agent_tool_dispatch(&parsed)
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

/// The claim response for "you get nothing".
///
/// A refusal MUST be byte-identical to an empty queue. The external worker is
/// untrusted for the enforcement decision, so it must not be able to tell
/// "policy blocked this" from "nothing to do" — otherwise the claim endpoint
/// becomes an oracle for which tools a tenant's policy forbids.
fn nothing_to_claim() -> Json<Value> {
    Json(json!({ "claimed": false }))
}

/// Whether a claimed item may be handed to the caller.
enum ClaimGate {
    /// Hand back the payload.
    Release,
    /// Refuse. The item has already been settled as its outcome requires.
    Withhold,
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
    let Some(wi) = item else {
        return Ok(nothing_to_claim());
    };

    // Enforcement lives HERE, not only in the in-process worker: `python_tool`
    // and `java_tool` are registered with zero in-process workers, so an ADK
    // agent's tool-dispatch node reaches production down this route and never
    // through `Worker::execute_item`. See `gate_claimed_item`.
    if matches!(
        gate_claimed_item(backend.as_ref(), &wi).await,
        ClaimGate::Withhold
    ) {
        return Ok(nothing_to_claim());
    }

    // The idempotency key for THIS node occurrence, computed here because claim
    // time is fire time for the external tier — the moment the payload leaves
    // the engine is the moment the effect becomes possible.
    //
    // Without it, an external tool's result was recorded with
    // `idempotency_key: None`, so nothing ever landed in `tool_effects` and the
    // replay guard covered the in-process tier only. Re-running a node re-fired
    // the tool, on the transport ADK nodes actually take.
    //
    // Best effort: if the state needed to compute it cannot be read, the item is
    // still handed out. That is the behaviour this route had all along, and
    // withholding real work because a replay OPTIMISATION could not be prepared
    // would trade a live queue for a hygiene property.
    let idem_key = compute_idempotency_key(backend.as_ref(), &wi).await;

    Ok(Json(json!({
        "claimed": true,
        "work_item": {
            "id": wi.id.to_string(),
            "execution_id": wi.execution_id.to_string(),
            "node_id": wi.node_id,
            "queue_type": wi.queue_type,
            "payload": wi.payload,
            "attempt": wi.attempt,
            // The lease fence the external worker echoes on complete so the
            // engine can prove the lease is still held (exactly-once-COMMIT).
            "lease_fence": wi.lease_fence,
            // Echoed on complete so the result is recorded against it and a
            // re-run replays instead of re-firing the tool.
            "idempotency_key": idem_key,
        }
    })))
}

/// The idempotency key for a claimed item, or `None` if it cannot be computed.
///
/// Delegates to `jamjet_state::derive_idempotency_key`, the SAME function
/// `Worker::execute_item` uses, so the in-process and external transports
/// cannot drift into filing effects under different keys.
async fn compute_idempotency_key(
    backend: &dyn jamjet_state::backend::StateBackend,
    wi: &WorkItem,
) -> Option<String> {
    jamjet_state::derive_idempotency_key(backend, &wi.execution_id, &wi.node_id)
        .await
        .ok()
}

/// Run the shared agent-dispatch guard over a freshly claimed item.
///
/// An ADK agent compiles a whole turn's model-chosen tool calls into ONE
/// `python_fn` / `java_fn` node, so the tool names are invisible to the ordinary
/// node-kind policy path (review finding C1). The decision itself lives in
/// `jamjet_worker::dispatch_guard` so this route and the in-process worker
/// enforce the IDENTICAL policy; this function owns only the settle, which is
/// transport-specific. Nothing here re-implements a policy rule — a second copy
/// would drift, and a drifted copy of an enforcement rule is a hole.
///
/// Enforcement is server-side on purpose: the external worker is untrusted, so
/// a stale or hostile `jamjet worker` build cannot opt out of it.
///
/// KNOWN LIMIT — duplicate approval requests. `guard_dispatch`'s `NotRequested`
/// arm reads "is there an open request?" and then appends one, with no
/// compare-and-set between. Two claims that reach it concurrently for the SAME
/// node can each append, and because `node_approval_status` resets to `Pending`
/// on every new request, a human's settled decision would refer to a superseded
/// request and could never stick.
///
/// This is NOT serialised here, deliberately. `claim_work_item` is atomic, so
/// one work item goes to exactly one caller; reaching the race needs two
/// distinct work items for the same node, which is the same exposure the
/// in-process worker already has (a lease serialises one ITEM, not one NODE).
/// A process-local mutex would not serialise the multiple `jamjet-server`
/// processes a real deployment runs, so it would buy nothing but false
/// confidence. Closing it properly needs an "append iff no open request for this
/// node" primitive on `StateBackend`, which is a backend change, not a route
/// change. Fail-closed holds either way: both racers return `Held`, so nothing
/// runs unapproved.
///
/// `Held` also conflates "awaiting a decision" with "a human rejected this".
/// That distinction is deliberately unobservable here — every refusal is the
/// same `{"claimed": false}` — so acting on it would create exactly the signal
/// the uniform response exists to deny the caller.
async fn gate_claimed_item(backend: &dyn jamjet_state::StateBackend, wi: &WorkItem) -> ClaimGate {
    // AUTHORITY. The coordinates that select the policy chain come from the
    // EXECUTION record, never from the payload.
    //
    // `POST /work-items` copies a caller-supplied `payload` and `node_id`
    // verbatim into the queue behind the same write role as this route. If the
    // payload chose the workflow, that caller could point a `python_tool` item
    // at an unpoliced workflow and drop the workflow layer out of the chain, or
    // omit the version and have v1.0.0's rules evaluated against a v2.0.0
    // execution. The execution record is engine-written, so it is the only
    // trustworthy answer to "which policy governs this item".
    let execution = match backend.get_execution(&wi.execution_id).await {
        // Infrastructure — see the `get_workflow` Err arm below.
        Err(e) => {
            warn!(
                execution_id = %wi.execution_id,
                node_id = %wi.node_id,
                error = %e,
                "claim: execution lookup failed; withholding the payload"
            );
            return ClaimGate::Withhold;
        }
        Ok(None) => {
            return fail_closed(backend, wi, "execution not found".to_string()).await;
        }
        Ok(Some(e)) => e,
    };
    let workflow_id = execution.workflow_id.as_str();
    let workflow_version = execution.workflow_version.as_str();

    // Tripwire. `Worker::execute_item` still resolves its IR from the payload
    // (`parse_payload`), so a payload that disagrees with its own execution
    // would make the two enforcement seams evaluate different policy for the
    // same item. There is no legitimate producer of that shape — the scheduler
    // builds every payload from these very coordinates
    // (`runtime/scheduler/src/runner.rs`) — so a disagreement means one of the
    // two is lying and we cannot tell which. Refuse rather than pick.
    //
    // Absent coordinates are NOT a mismatch: nothing is claimed, so nothing can
    // conflict, and the execution's values are used regardless. There is no
    // default here to exploit — the old `unwrap_or("unknown")` /
    // `unwrap_or("1.0.0")` fallbacks are gone.
    let payload_disagrees = |key: &str, authoritative: &str| {
        wi.payload
            .get(key)
            .and_then(|v| v.as_str())
            .is_some_and(|claimed| claimed != authoritative)
    };
    if payload_disagrees("workflow_id", workflow_id)
        || payload_disagrees("workflow_version", workflow_version)
    {
        return fail_closed(
            backend,
            wi,
            format!(
                "work item payload coordinates disagree with execution {workflow_id} \
                 v{workflow_version}"
            ),
        )
        .await;
    }

    let ir = match backend.get_workflow(workflow_id, workflow_version).await {
        // Infrastructure, not a decision: the engine could not read its own
        // catalogue. Settle NOTHING — failing the item would kill work that was
        // never evaluated, and completing it would silently drop it. The lease
        // expires, the reclaimer requeues, and the next claim decides properly.
        Err(e) => {
            warn!(
                execution_id = %wi.execution_id,
                node_id = %wi.node_id,
                error = %e,
                "claim: workflow lookup failed; withholding the payload"
            );
            return ClaimGate::Withhold;
        }
        // Permanent, and identical to the worker's `ExecutorError::Fatal` for
        // the same condition. Fail closed: without the IR the
        // `agent_tool_dispatch` marker cannot be read, so whether this item
        // needs policy evaluation is UNKNOWN. Handing it out would make "point
        // the payload at a workflow that does not exist" a complete bypass.
        Ok(None) => {
            return fail_closed(
                backend,
                wi,
                format!("workflow {workflow_id} v{workflow_version} not found"),
            )
            .await;
        }
        Ok(Some(def)) => match serde_json::from_value::<jamjet_ir::WorkflowIr>(def.ir) {
            Ok(ir) => ir,
            Err(e) => {
                return fail_closed(backend, wi, format!("failed to load IR: {e}")).await;
            }
        },
    };

    // The node id selects both the policy set and the dispatch marker, so a
    // node that is not in the IR has neither and cannot be evaluated.
    let Some(node_def) = ir.node(&wi.node_id) else {
        return fail_closed(backend, wi, format!("node {} not found in IR", wi.node_id)).await;
    };

    // NARROWNESS. This route serves every queue. Everything that is not an agent
    // tool dispatch — every model node, tool node, condition, ordinary python_fn
    // — leaves here having had zero POLICY evaluation, and is returned exactly
    // as before.
    //
    // Not free, though: reaching this line already cost `get_execution`,
    // `get_workflow` and a full `WorkflowIr` parse, and this is the hot path
    // every external worker polls. Those lookups are what make the decision
    // trustworthy — the execution record is the only engine-written answer to
    // "which policy governs this item" — so they cannot simply move below this
    // check. A `wi.queue_type` gate above them would skip the whole function for
    // queues that can never hold a dispatch node, but it would also skip the
    // coordinate tripwire and the execution-not-found fail-closed for those
    // queues, which is a behaviour change and not one to make inside a security
    // fix. Tracked in #116.
    if !jamjet_worker::dispatch_guard::is_agent_tool_dispatch(&node_def.kind) {
        return ClaimGate::Release;
    }

    let input = jamjet_worker::dispatch_guard::payload_input(&wi.payload);
    match jamjet_worker::dispatch_guard::guard_dispatch(
        backend,
        &wi.execution_id,
        &wi.node_id,
        &wi.tenant_id,
        &ir,
        node_def,
        &input,
    )
    .await
    {
        DispatchGuardOutcome::Allow => ClaimGate::Release,

        // The guard already recorded the `PolicyViolation`. Failing the item is
        // what stops a blocked dispatch from being re-claimed forever: on a
        // durable backend `fail_work_item` is terminal.
        //
        // KNOWN ASYMMETRY — this seam and the worker seam disagree on
        // terminality, and this is the outcome of EVERY successful enforcement
        // here, not an edge case.
        //
        // `SqliteBackend::fail_work_item` sets `status = 'failed'`
        // unconditionally, so the reclaimer never returns the item and NOTHING
        // emits `NodeFailed`. The execution therefore stays `Running` with the
        // node still in the scheduler fold's `scheduled` set — it stalls rather
        // than going terminal. `Worker::execute_item` does emit the terminal
        // event, via the fenced `commit_turn`, so the same policy denial ends
        // the execution on the in-process path and hangs it here.
        //
        // Safety is unaffected: the tool does not run and the denial is on the
        // Prove surface either way. Closing the gap means emitting a
        // fence-committed terminal event from the claim route, which is a
        // scheduler-interaction change and is deliberately NOT done inside this
        // security fix. It needs its own task.
        DispatchGuardOutcome::Blocked { reason } => {
            warn!(
                execution_id = %wi.execution_id,
                node_id = %wi.node_id,
                %reason,
                "claim: policy blocked an agent tool dispatch"
            );
            settle(
                backend
                    .fail_work_item(wi.id, &format!("policy blocked: {reason}"))
                    .await,
                wi,
            )
        }

        // An outstanding `ToolApprovalRequired` exists for the node. Settle the
        // item cleanly so its lease never expires into the retry path; the node
        // stays parked in the scheduler fold's `scheduled` set until a human
        // decides.
        //
        // Fenced, unlike the worker's plain `complete_work_item`: we hold a
        // fence minted by the claim we just made, so we can prove the lease is
        // still ours. A lost fence means something else already settled or
        // reclaimed the item, which is not ours to overwrite — and the payload
        // is withheld either way.
        DispatchGuardOutcome::Held { gated } => {
            info!(
                execution_id = %wi.execution_id,
                node_id = %wi.node_id,
                gated = ?gated,
                "claim: agent tool dispatch awaiting approval"
            );
            match backend
                .complete_work_item_fenced(wi.id, wi.lease_fence)
                .await
            {
                Ok(true) => ClaimGate::Withhold,
                Ok(false) => {
                    warn!(
                        execution_id = %wi.execution_id,
                        node_id = %wi.node_id,
                        "claim: lease fence lost while settling a held work item"
                    );
                    ClaimGate::Withhold
                }
                Err(e) => settle(Err(e), wi),
            }
        }

        // NOT a denial — no policy was ever consulted, so nothing is audited and
        // nothing is settled. See the `Err` arm above for why leaving the lease
        // alone is the right infrastructure-failure behaviour.
        DispatchGuardOutcome::Unavailable { reason } => {
            warn!(
                execution_id = %wi.execution_id,
                node_id = %wi.node_id,
                %reason,
                "claim: could not decide an agent tool dispatch; withholding the payload"
            );
            ClaimGate::Withhold
        }
    }
}

/// Terminally fail an item the engine cannot evaluate, and withhold it.
///
/// No `PolicyViolation` is recorded: these are structural failures (missing
/// workflow, unparseable IR, unknown node), not policy denials, and auditing
/// them as denials would put a refusal on the Prove surface that no policy made.
/// The worker treats the identical conditions as `ExecutorError::Fatal`.
async fn fail_closed(
    backend: &dyn jamjet_state::StateBackend,
    wi: &WorkItem,
    reason: String,
) -> ClaimGate {
    warn!(
        execution_id = %wi.execution_id,
        node_id = %wi.node_id,
        %reason,
        "claim: cannot evaluate policy for this item; withholding the payload"
    );
    settle(backend.fail_work_item(wi.id, &reason).await, wi)
}

/// Withhold regardless of whether the settle landed.
///
/// A settle that fails leaves the item leased until the lease expires, which is
/// a liveness cost, never a safety one: the payload is withheld either way.
/// Fail closed, never open.
fn settle(result: jamjet_state::backend::BackendResult<()>, wi: &WorkItem) -> ClaimGate {
    if let Err(e) = result {
        warn!(
            execution_id = %wi.execution_id,
            node_id = %wi.node_id,
            error = %e,
            "claim: failed to settle a withheld work item"
        );
    }
    ClaimGate::Withhold
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
    /// Lease fence echoed from the claim response. When present, the completion is
    /// gated on it: a stale/forged fence is rejected (409) and NO `NodeCompleted`
    /// is emitted. When absent, the legacy unfenced settle-by-id path is used
    /// (backward-compat for callers not yet echoing the fence).
    #[serde(default)]
    lease_fence: Option<i64>,
    /// Idempotency key echoed from the claim response.
    ///
    /// With it, `commit_turn` records the result in `tool_effects` in the same
    /// transaction, so a re-run of this node replays the recorded output instead
    /// of firing the tool again. Without it nothing is recorded and the replay
    /// guard simply does not cover this node — which was the case for EVERY
    /// external tool effect until now.
    #[serde(default)]
    idempotency_key: Option<String>,
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
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let item_id = Uuid::parse_str(&id)
        .map_err(|_| ApiError::BadRequest(format!("invalid work item id: {id}")))?;
    let backend = state.backend_for(&tenant_id);

    let node_completed = |execution_id: &ExecutionId| {
        jamjet_state::Event::new(
            execution_id.clone(),
            // Sequence is assigned inside `commit_turn`'s transaction; this
            // placeholder is never the number that lands.
            0,
            jamjet_state::EventKind::NodeCompleted {
                node_id: body.node_id.clone().unwrap_or_default(),
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
                idempotency_key: body.idempotency_key.clone(),
            },
        )
    };

    // Settle AND emit in ONE transaction.
    //
    // These used to be separate statements: settle the item, then append the
    // NodeCompleted. A crash in that window left the item settled with no
    // terminal event, so the scheduler fold kept the node in `scheduled` and the
    // execution never finished — permanently, since the fold is a replay of the
    // log. `commit_turn` is the primitive the in-process worker already uses for
    // exactly this, and it is fence-guarded, so a stale worker writes nothing.
    // A lease fence is a MINTED token (`term * 2^32 + epoch`), so it is always
    // positive. Zero is what a never-claimed row carries and what a defaulted or
    // forged body sends, so it must never reach a fenced settle — treat it as
    // absent rather than as a fence that happens to match every pending item.
    if body.lease_fence.is_some_and(|f| f <= 0) {
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({
                "completed": false,
                "reason": "stale or invalid lease fence",
            })),
        ));
    }

    // An empty key is never something this engine issued — every key it mints is
    // a sha256 hex digest. Recording an effect under "" would file a row no
    // reader can ever derive, so reject it rather than accumulate junk that
    // looks like a recorded effect.
    if body.idempotency_key.as_deref().is_some_and(str::is_empty) {
        return Err(ApiError::BadRequest(
            "idempotency_key must not be empty".to_string(),
        ));
    }

    let committed_atomically = match (
        body.lease_fence,
        body.execution_id.as_deref(),
        body.node_id.as_deref(),
    ) {
        (Some(fence), Some(exec_id_str), Some(_)) => {
            let execution_id = parse_execution_id(exec_id_str)?;
            match backend
                .commit_turn(item_id, fence, node_completed(&execution_id), true)
                .await
            {
                Ok(_) => true,
                Err(jamjet_state::StateBackendError::FenceLost(_)) => {
                    return Ok((
                        StatusCode::CONFLICT,
                        Json(json!({
                            "completed": false,
                            "reason": "stale or invalid lease fence",
                        })),
                    ));
                }
                Err(e) => return Err(e.into()),
            }
        }
        // Fenced, but with no coordinates to build a terminal event from. Settle
        // only — the same shape as before, and still fence-guarded.
        (Some(fence), _, _) => {
            let settled = backend.complete_work_item_fenced(item_id, fence).await?;
            if !settled {
                return Ok((
                    StatusCode::CONFLICT,
                    Json(json!({
                        "completed": false,
                        "reason": "stale or invalid lease fence",
                    })),
                ));
            }
            false
        }
        // Legacy unfenced path, deprecated. Kept so callers that predate the
        // fence keep working, but it cannot be made atomic: `commit_turn` is
        // fence-guarded by construction, and without a fence we cannot show the
        // item is ours to settle.
        (None, _, _) => {
            warn!(
                work_item_id = %id,
                "complete: unfenced legacy path — settle and terminal event are \
                 not atomic; echo lease_fence to make them one transaction"
            );
            backend.complete_work_item(item_id).await?;
            false
        }
    };

    // The unfenced and no-coordinate paths still emit separately.
    if !committed_atomically {
        if let (Some(exec_id_str), Some(_)) = (&body.execution_id, &body.node_id) {
            let execution_id = parse_execution_id(exec_id_str)?;
            let seq = backend.latest_sequence(&execution_id).await? + 1;
            let mut event = node_completed(&execution_id);
            event.sequence = seq;
            backend.append_event(event).await?;
        }
    }

    // Gated on BOTH coordinates, matching the terminal event. Refreshing the read
    // model when no `NodeCompleted` was emitted would let the column drift in a
    // way the event log cannot explain — and the log is what the materializer
    // rebuilds from, so the two would simply disagree with no way to tell which
    // is right.
    if let (Some(exec_id_str), Some(_)) = (&body.execution_id, &body.node_id) {
        let execution_id = parse_execution_id(exec_id_str)?;
        // Denormalised read-model refresh, best effort by design: the
        // authoritative state is the event log plus the snapshot `commit_turn`
        // wrote inside the transaction, and the materializer recomputes from
        // those. Losing this write costs a stale convenience column, not state.
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

    Ok((
        StatusCode::OK,
        Json(json!({ "completed": true, "work_item_id": id })),
    ))
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
    /// Lease fence echoed from the claim response. Gates the failure on still
    /// holding the lease, exactly as `/complete` does.
    ///
    /// Absent means the legacy unfenced path: the item is settled `'failed'` and
    /// NO event is emitted, which strands the node. It is kept only so callers
    /// that predate the fence keep working, and it is deprecated — a caller that
    /// echoes the fence gets retry semantics and a terminal event instead.
    #[serde(default)]
    lease_fence: Option<i64>,
}

/// `POST /work-items/:id/fail` — a worker reports that its node failed.
///
/// The fenced path applies the SAME retry, backoff and dead-letter rules that
/// lease reclamation applies, and emits the SAME events, so a node whose worker
/// reported a failure and a node whose worker died converge on one state machine.
///
/// Before this, the whole endpoint was `fail_work_item` and nothing else: no
/// fence, so any caller holding an id could settle someone else's item, and no
/// event, so the scheduler fold kept the node in `scheduled` while the row sat in
/// a `'failed'` status that no sweep ever selects. Every legitimate tool failure
/// stranded its execution as `Running`, permanently.
async fn fail_work_item(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
    Path(id): Path<String>,
    Json(body): Json<FailWorkItemRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let item_id = Uuid::parse_str(&id)
        .map_err(|_| ApiError::BadRequest(format!("invalid work item id: {id}")))?;
    let backend = state.backend_for(&tenant_id);

    let Some(fence) = body.lease_fence else {
        // Legacy unfenced path, deprecated. Preserved verbatim so existing
        // callers do not break, and deliberately NOT given the new event
        // emission: without a fence we cannot show the item was ours to fail, and
        // emitting a terminal event on an item another worker may now hold is
        // worse than the stranding this path already causes.
        warn!(
            work_item_id = %id,
            "fail: unfenced legacy path — the node will be stranded; echo lease_fence to get retry semantics"
        );
        backend.fail_work_item(item_id, &body.error).await?;
        return Ok((
            StatusCode::OK,
            Json(json!({
                "failed": true,
                "work_item_id": id,
                "retryable": false,
                "warning": "unfenced fail: no NodeFailed emitted and the node is not rescheduled; echo lease_fence",
            })),
        ));
    };

    let Some(outcome) = backend
        .fail_work_item_fenced(item_id, fence, &body.error)
        .await?
    else {
        // Someone else owns it now — reclaimed, already settled, or a forged
        // fence. Emit nothing: the holder decides this item's fate.
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({
                "failed": false,
                "work_item_id": id,
                "reason": "stale or invalid lease fence",
            })),
        ));
    };

    // Emit exactly what `SchedulerRunner::reclaim_expired_leases` emits for the
    // same transition, so the fold cannot tell the two apart.
    let (item, retryable, delay_ms) = match &outcome {
        FailOutcome::Retryable { item, delay_ms } => (item, true, Some(*delay_ms)),
        FailOutcome::Exhausted { item } => (item, false, None),
    };

    let seq = backend.latest_sequence(&item.execution_id).await? + 1;
    backend
        .append_event(jamjet_state::Event::new(
            item.execution_id.clone(),
            seq,
            jamjet_state::EventKind::NodeFailed {
                node_id: item.node_id.clone(),
                error: body.error.clone(),
                // Retryable reports the attempt that just failed; exhausted
                // reports the final count. Mirrors the reclaimer's arithmetic.
                attempt: if retryable {
                    item.attempt.saturating_sub(1)
                } else {
                    item.attempt
                },
                retryable,
            },
        ))
        .await?;

    if let Some(delay_ms) = delay_ms {
        let seq = backend.latest_sequence(&item.execution_id).await? + 1;
        backend
            .append_event(jamjet_state::Event::new(
                item.execution_id.clone(),
                seq,
                jamjet_state::EventKind::RetryScheduled {
                    node_id: item.node_id.clone(),
                    attempt: item.attempt,
                    delay_ms,
                },
            ))
            .await?;
    }

    warn!(
        execution_id = %item.execution_id,
        node_id = %item.node_id,
        attempt = item.attempt,
        retryable,
        "fail: worker reported a node failure"
    );

    Ok((
        StatusCode::OK,
        Json(json!({
            "failed": true,
            "work_item_id": id,
            "retryable": retryable,
            "attempt": item.attempt,
        })),
    ))
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
