//! MCP bridge — exposes core JamJet operations as MCP tools at `/mcp`.
//!
//! This lets MCP clients (Claude Code, Cursor, etc.) interact with the
//! JamJet runtime directly: run workflows, inspect executions, manage agents.

use crate::state::AppState;
use axum::Router;
use jamjet_agents::{AgentFilter, AgentStatus};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_mcp::server::McpServer;
use jamjet_mcp::types::{McpContent, McpTool};
use jamjet_state::{Event, EventKind, TenantId, WorkItem};
use serde_json::{json, Value};
use std::sync::Arc;
use chrono::Utc;
use uuid::Uuid;

/// Build the MCP bridge router with all JamJet runtime tools.
///
/// The returned router is meant to be merged into the main API router.
/// No auth layer — same security model as `jamjet dev` (local-only).
pub fn build_mcp_bridge(state: AppState) -> Router {
    let st = Arc::new(state);

    let server = McpServer::new("jamjet-runtime", env!("CARGO_PKG_VERSION"), 0);

    // ── jamjet_run_workflow ──────────────────────────────────────────────
    let s = st.clone();
    let server = server.register_tool(
        McpTool {
            name: "jamjet_run_workflow".into(),
            description: Some("Start a workflow execution".into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "workflow_id": { "type": "string", "description": "ID of the workflow to run" },
                    "input": { "type": "object", "description": "Input data for the workflow" },
                    "workflow_version": { "type": "string", "description": "Workflow version (default: 1.0.0)" },
                    "tenant_id": { "type": "string", "description": "Tenant ID (default: default)" }
                },
                "required": ["workflow_id", "input"]
            }),
        },
        move |args: Value| {
            let s = s.clone();
            async move {
                let workflow_id = args.get("workflow_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let input = args.get("input").cloned().unwrap_or(json!({}));
                let version = args.get("workflow_version").and_then(|v| v.as_str()).unwrap_or("1.0.0").to_string();
                let tenant_id = TenantId::from(
                    args.get("tenant_id").and_then(|v| v.as_str()).unwrap_or("default"),
                );

                let backend = s.backend_for(&tenant_id);

                let def = backend
                    .get_workflow(&workflow_id, &version)
                    .await
                    .map_err(|e| format!("failed to get workflow: {e}"))?
                    .ok_or_else(|| format!("workflow {} v{} not found", workflow_id, version))?;

                let start_node = def.ir.get("start_node")
                    .and_then(|v| v.as_str())
                    .unwrap_or("start")
                    .to_string();

                let now = Utc::now();
                let execution = WorkflowExecution {
                    execution_id: ExecutionId::new(),
                    workflow_id: workflow_id.clone(),
                    workflow_version: version.clone(),
                    status: WorkflowStatus::Running,
                    initial_input: input.clone(),
                    current_state: input.clone(),
                    started_at: now,
                    updated_at: now,
                    completed_at: None,
                };
                let eid = execution.execution_id.clone();
                backend.create_execution(execution).await.map_err(|e| format!("{e}"))?;

                let event = Event::new(eid.clone(), 1, EventKind::WorkflowStarted {
                    workflow_id: workflow_id.clone(),
                    workflow_version: version.clone(),
                    initial_input: input.clone(),
                });
                backend.append_event(event).await.map_err(|e| format!("{e}"))?;

                let queue_type = "general".to_string();
                let sched_event = Event::new(eid.clone(), 2, EventKind::NodeScheduled {
                    node_id: start_node.clone(),
                    queue_type: queue_type.clone(),
                });
                backend.append_event(sched_event).await.map_err(|e| format!("{e}"))?;

                let work_item = WorkItem {
                    id: Uuid::new_v4(),
                    execution_id: eid.clone(),
                    node_id: start_node,
                    queue_type,
                    payload: json!({"workflow_id": workflow_id, "workflow_version": version}),
                    attempt: 0,
                    max_attempts: 3,
                    created_at: now,
                    lease_expires_at: None,
                    worker_id: None,
                    tenant_id: tenant_id.0.clone(),
                };
                backend.enqueue_work_item(work_item).await.map_err(|e| format!("{e}"))?;

                Ok(vec![McpContent::Text {
                    text: json!({"execution_id": eid.to_string()}).to_string(),
                }])
            }
        },
    );

    // ── jamjet_get_execution ─────────────────────────────────────────────
    let s = st.clone();
    let server = server.register_tool(
        McpTool {
            name: "jamjet_get_execution".into(),
            description: Some("Get details of a workflow execution".into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "execution_id": { "type": "string", "description": "Execution ID (exec_<uuid> or bare UUID)" },
                    "tenant_id": { "type": "string", "description": "Tenant ID (default: default)" }
                },
                "required": ["execution_id"]
            }),
        },
        move |args: Value| {
            let s = s.clone();
            async move {
                let id_str = args.get("execution_id").and_then(|v| v.as_str()).unwrap_or("");
                let tenant_id = TenantId::from(
                    args.get("tenant_id").and_then(|v| v.as_str()).unwrap_or("default"),
                );
                let eid = parse_execution_id(id_str)?;
                let backend = s.backend_for(&tenant_id);
                let exec = backend.get_execution(&eid).await.map_err(|e| format!("{e}"))?
                    .ok_or_else(|| format!("execution {id_str} not found"))?;
                let text = serde_json::to_string(&exec).map_err(|e| format!("{e}"))?;
                Ok(vec![McpContent::Text { text }])
            }
        },
    );

    // ── jamjet_list_executions ───────────────────────────────────────────
    let s = st.clone();
    let server = server.register_tool(
        McpTool {
            name: "jamjet_list_executions".into(),
            description: Some("List workflow executions with optional filters".into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "status": { "type": "string", "description": "Filter by status: running, paused, completed, failed" },
                    "limit": { "type": "integer", "description": "Max results (default 50)" },
                    "offset": { "type": "integer", "description": "Offset for pagination" },
                    "tenant_id": { "type": "string", "description": "Tenant ID (default: default)" }
                }
            }),
        },
        move |args: Value| {
            let s = s.clone();
            async move {
                let tenant_id = TenantId::from(
                    args.get("tenant_id").and_then(|v| v.as_str()).unwrap_or("default"),
                );
                let status = args.get("status").and_then(|v| v.as_str()).and_then(parse_status);
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as u32;
                let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

                let backend = s.backend_for(&tenant_id);
                let execs = backend.list_executions(status, limit, offset).await.map_err(|e| format!("{e}"))?;
                let text = serde_json::to_string(&json!({"executions": execs})).map_err(|e| format!("{e}"))?;
                Ok(vec![McpContent::Text { text }])
            }
        },
    );

    // ── jamjet_cancel_execution ──────────────────────────────────────────
    let s = st.clone();
    let server = server.register_tool(
        McpTool {
            name: "jamjet_cancel_execution".into(),
            description: Some("Cancel a running workflow execution".into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "execution_id": { "type": "string", "description": "Execution ID" },
                    "tenant_id": { "type": "string", "description": "Tenant ID (default: default)" }
                },
                "required": ["execution_id"]
            }),
        },
        move |args: Value| {
            let s = s.clone();
            async move {
                let id_str = args.get("execution_id").and_then(|v| v.as_str()).unwrap_or("");
                let tenant_id = TenantId::from(
                    args.get("tenant_id").and_then(|v| v.as_str()).unwrap_or("default"),
                );
                let eid = parse_execution_id(id_str)?;
                let backend = s.backend_for(&tenant_id);

                let exec = backend.get_execution(&eid).await.map_err(|e| format!("{e}"))?
                    .ok_or_else(|| format!("execution {id_str} not found"))?;

                if exec.status.is_terminal() {
                    return Err(format!("execution {id_str} is already terminal: {:?}", exec.status));
                }

                let seq = backend.latest_sequence(&eid).await.map_err(|e| format!("{e}"))? + 1;
                let event = Event::new(eid.clone(), seq, EventKind::WorkflowCancelled {
                    reason: Some("cancelled via MCP".into()),
                });
                backend.append_event(event).await.map_err(|e| format!("{e}"))?;
                backend.update_execution_status(&eid, WorkflowStatus::Cancelled).await.map_err(|e| format!("{e}"))?;

                Ok(vec![McpContent::Text {
                    text: json!({"execution_id": id_str, "status": "cancelled"}).to_string(),
                }])
            }
        },
    );

    // ── jamjet_get_events ────────────────────────────────────────────────
    let s = st.clone();
    let server = server.register_tool(
        McpTool {
            name: "jamjet_get_events".into(),
            description: Some("Get the event log for an execution".into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "execution_id": { "type": "string", "description": "Execution ID" },
                    "tenant_id": { "type": "string", "description": "Tenant ID (default: default)" }
                },
                "required": ["execution_id"]
            }),
        },
        move |args: Value| {
            let s = s.clone();
            async move {
                let id_str = args.get("execution_id").and_then(|v| v.as_str()).unwrap_or("");
                let tenant_id = TenantId::from(
                    args.get("tenant_id").and_then(|v| v.as_str()).unwrap_or("default"),
                );
                let eid = parse_execution_id(id_str)?;
                let backend = s.backend_for(&tenant_id);
                let events = backend.get_events(&eid).await.map_err(|e| format!("{e}"))?;
                let text = serde_json::to_string(&json!({"events": events})).map_err(|e| format!("{e}"))?;
                Ok(vec![McpContent::Text { text }])
            }
        },
    );

    // ── jamjet_approve ───────────────────────────────────────────────────
    let s = st.clone();
    let server = server.register_tool(
        McpTool {
            name: "jamjet_approve".into(),
            description: Some("Approve or reject a paused execution".into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "execution_id": { "type": "string", "description": "Execution ID" },
                    "decision": { "type": "string", "description": "approved or rejected" },
                    "node_id": { "type": "string", "description": "Node that requested approval" },
                    "comment": { "type": "string", "description": "Optional comment" },
                    "tenant_id": { "type": "string", "description": "Tenant ID (default: default)" }
                },
                "required": ["execution_id", "decision"]
            }),
        },
        move |args: Value| {
            let s = s.clone();
            async move {
                let id_str = args.get("execution_id").and_then(|v| v.as_str()).unwrap_or("");
                let decision_str = args.get("decision").and_then(|v| v.as_str()).unwrap_or("");
                let node_id = args.get("node_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let comment = args.get("comment").and_then(|v| v.as_str()).map(String::from);
                let tenant_id = TenantId::from(
                    args.get("tenant_id").and_then(|v| v.as_str()).unwrap_or("default"),
                );

                let decision = match decision_str {
                    "approved" => jamjet_state::event::ApprovalDecision::Approved,
                    "rejected" => jamjet_state::event::ApprovalDecision::Rejected,
                    other => return Err(format!("unknown decision: {other}")),
                };

                let eid = parse_execution_id(id_str)?;
                let backend = s.backend_for(&tenant_id);

                let seq = backend.latest_sequence(&eid).await.map_err(|e| format!("{e}"))? + 1;
                let event = Event::new(eid.clone(), seq, EventKind::ApprovalReceived {
                    node_id,
                    user_id: "mcp-client".into(),
                    decision,
                    comment,
                    state_patch: None,
                });
                backend.append_event(event).await.map_err(|e| format!("{e}"))?;

                if let Ok(Some(exec)) = backend.get_execution(&eid).await {
                    if exec.status == WorkflowStatus::Paused {
                        backend.update_execution_status(&eid, WorkflowStatus::Running)
                            .await.map_err(|e| format!("{e}"))?;
                    }
                }

                Ok(vec![McpContent::Text {
                    text: json!({"execution_id": id_str, "accepted": true}).to_string(),
                }])
            }
        },
    );

    // ── jamjet_list_agents ───────────────────────────────────────────────
    let s = st.clone();
    let server = server.register_tool(
        McpTool {
            name: "jamjet_list_agents".into(),
            description: Some("List registered agents with optional filters".into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "status": { "type": "string", "description": "Filter by status: registered, active, paused, deactivated" },
                    "skill": { "type": "string", "description": "Filter by skill" },
                    "protocol": { "type": "string", "description": "Filter by protocol" }
                }
            }),
        },
        move |args: Value| {
            let s = s.clone();
            async move {
                let status = args.get("status").and_then(|v| v.as_str()).and_then(|s| match s {
                    "registered" => Some(AgentStatus::Registered),
                    "active" => Some(AgentStatus::Active),
                    "paused" => Some(AgentStatus::Paused),
                    "deactivated" => Some(AgentStatus::Deactivated),
                    _ => None,
                });
                let filter = AgentFilter {
                    status,
                    skill: args.get("skill").and_then(|v| v.as_str()).map(String::from),
                    protocol: args.get("protocol").and_then(|v| v.as_str()).map(String::from),
                };
                let agents = s.agents.find(filter).await.map_err(|e| format!("{e}"))?;
                let text = serde_json::to_string(&json!({"agents": agents})).map_err(|e| format!("{e}"))?;
                Ok(vec![McpContent::Text { text }])
            }
        },
    );

    // ── jamjet_discover_agent ────────────────────────────────────────────
    let s = st.clone();
    let server = server.register_tool(
        McpTool {
            name: "jamjet_discover_agent".into(),
            description: Some("Discover and register a remote agent by URL".into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL of the remote agent to discover" }
                },
                "required": ["url"]
            }),
        },
        move |args: Value| {
            let s = s.clone();
            async move {
                let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
                let agent = s.agents.discover_remote(url).await.map_err(|e| format!("{e}"))?;
                let text = serde_json::to_string(&agent).map_err(|e| format!("{e}"))?;
                Ok(vec![McpContent::Text { text }])
            }
        },
    );

    server.into_router()
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn parse_execution_id(s: &str) -> Result<ExecutionId, String> {
    let hex = s.strip_prefix("exec_").unwrap_or(s);
    let uuid = Uuid::parse_str(hex).map_err(|_| format!("invalid execution id: {s}"))?;
    Ok(ExecutionId(uuid))
}

fn parse_status(s: &str) -> Option<WorkflowStatus> {
    match s {
        "running" => Some(WorkflowStatus::Running),
        "paused" => Some(WorkflowStatus::Paused),
        "completed" => Some(WorkflowStatus::Completed),
        "failed" => Some(WorkflowStatus::Failed),
        _ => None,
    }
}
