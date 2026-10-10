//! MCP bridge — exposes core JamJet operations as MCP tools at `/mcp`.
//!
//! This lets MCP clients (Claude Code, Cursor, etc.) interact with the
//! JamJet runtime directly: run workflows, inspect executions, manage agents.

use crate::auth::Role;
use crate::state::AppState;
use axum::Router;

use jamjet_agents::{AgentFilter, AgentStatus};
use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use jamjet_mcp::server::{McpServer, ToolCallContext};
use jamjet_mcp::types::{McpContent, McpTool};
use jamjet_state::{ApiToken, Event, EventKind, TenantId};
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

/// Build the MCP bridge router with all JamJet runtime tools.
///
/// The returned router is meant to be merged into the main API router.
///
/// Outside dev mode the caller mounts it behind `require_auth`, and every tool
/// resolves its caller from the authenticated [`ApiToken`] in the request
/// extensions: the tenant is the token's tenant, a `tenant_id` argument that
/// disagrees with it is refused, and each tool checks the token's role. A
/// missing token outside dev mode is refused rather than treated as anonymous,
/// so mounting the bridge without the auth layer fails closed.
///
/// In dev mode (`jamjet dev`, local-only) no token exists: the tenant comes from
/// the `tenant_id` argument (default `"default"`) as before.
pub fn build_mcp_bridge(state: AppState, dev_mode: bool) -> Router {
    let st = Arc::new(state);

    let server = McpServer::new("jamjet-runtime", env!("CARGO_PKG_VERSION"), 0);

    // ── jamjet_run_workflow ──────────────────────────────────────────────
    let s = st.clone();
    let server = server.register_tool_with_context(
        McpTool {
            name: "jamjet_run_workflow".into(),
            description: Some(concat!(
                "Start a new durable workflow execution. ",
                "Use this to kick off a workflow that has already been registered with the runtime. ",
                "Side effects: creates a new execution record, appends WorkflowStarted and NodeScheduled events to the event log, ",
                "and enqueues a work item for the first node — the workflow begins processing immediately. ",
                "Returns a JSON object with the execution_id (format: exec_<uuid>) that you can pass to ",
                "jamjet_get_execution, jamjet_get_events, jamjet_cancel_execution, or jamjet_approve. ",
                "This operation is not reversible — use jamjet_cancel_execution to stop a running workflow. ",
                "Fails if the workflow_id + version combination is not registered. ",
                "Outside dev mode the bridge requires a Bearer API token with the developer or operator role."
            ).into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "workflow_id": {
                        "type": "string",
                        "description": "ID of a registered workflow to execute. Must match a workflow previously loaded into the runtime."
                    },
                    "input": {
                        "type": "object",
                        "description": "Initial state data passed to the workflow's first node. Shape must match the workflow's state_schema."
                    },
                    "workflow_version": {
                        "type": "string",
                        "description": "Semantic version of the workflow to run. Defaults to '1.0.0' if omitted. Use when multiple versions are registered."
                    },
                    "tenant_id": {
                        "type": "string",
                        "description": "Tenant partition for multi-tenant isolation. Outside dev mode the tenant is the API token's tenant and a different value is refused. In dev mode defaults to 'default'."
                    }
                },
                "required": ["workflow_id", "input"]
            }),
        },
        move |args: Value, ctx: ToolCallContext| {
            let s = s.clone();
            async move {
                let workflow_id = args.get("workflow_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let input = args.get("input").cloned().unwrap_or(json!({}));
                let version = args.get("workflow_version").and_then(|v| v.as_str()).unwrap_or("1.0.0").to_string();
                let caller = resolve_caller(&ctx, &args, dev_mode, Role::Developer)?;
                let tenant_id = caller.tenant.clone();

                // The same start path as `POST /executions`: one execution
                // record, `WorkflowStarted` with this token as initiator, and
                // the start item built and routed by the scheduler's own payload
                // builder. A second copy here once enqueued a bare payload on
                // `general` whatever the start node's kind.
                let initiated_by = caller.token.as_ref().map(crate::auth::principal_of);
                let (_, axum::Json(started)) = crate::routes::start_run(
                    &s,
                    &tenant_id,
                    workflow_id,
                    Some(version),
                    input,
                    initiated_by,
                )
                .await
                .map_err(|e| e.to_string())?;

                Ok(vec![McpContent::Text {
                    text: json!({"execution_id": started["execution_id"]}).to_string(),
                }])
            }
        },
    );

    // ── jamjet_get_execution ─────────────────────────────────────────────
    let s = st.clone();
    let server = server.register_tool_with_context(
        McpTool {
            name: "jamjet_get_execution".into(),
            description: Some(concat!(
                "Retrieve the full details of a single workflow execution. Read-only, no side effects. ",
                "Use this to check an execution's current status, inspect its state, or confirm completion after running jamjet_run_workflow. ",
                "Returns a JSON object with: execution_id, workflow_id, workflow_version, status (one of: running, paused, completed, failed, cancelled), ",
                "initial_input, current_state, started_at, updated_at, and completed_at (null if still running). ",
                "Fails with 'execution not found' if the ID does not exist in the specified tenant. ",
                "For the full event history, use jamjet_get_events instead."
            ).into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "execution_id": {
                        "type": "string",
                        "description": "Execution ID returned by jamjet_run_workflow. Accepts either 'exec_<uuid>' or bare UUID format."
                    },
                    "tenant_id": {
                        "type": "string",
                        "description": "Tenant partition to query. Defaults to 'default'. Must match the tenant used when the execution was created. Outside dev mode the tenant is the API token's tenant and a different value is refused."
                    }
                },
                "required": ["execution_id"]
            }),
        },
        move |args: Value, ctx: ToolCallContext| {
            let s = s.clone();
            async move {
                let id_str = args.get("execution_id").and_then(|v| v.as_str()).unwrap_or("");
                let caller = resolve_caller(&ctx, &args, dev_mode, Role::Viewer)?;
                let tenant_id = caller.tenant.clone();
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
    let server = server.register_tool_with_context(
        McpTool {
            name: "jamjet_list_executions".into(),
            description: Some(concat!(
                "List workflow executions with optional status filtering and pagination. Read-only, no side effects. ",
                "Use this to find executions that need attention — for example, filter by 'paused' to find executions awaiting approval via jamjet_approve, ",
                "or filter by 'running' to monitor active workflows. ",
                "Returns a JSON object with an 'executions' array, where each entry has the same fields as jamjet_get_execution. ",
                "Results are ordered by creation time (newest first). Supports offset-based pagination via limit and offset parameters. ",
                "All parameters are optional — calling with no arguments returns the 50 most recent executions across all statuses."
            ).into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "description": "Filter to a specific status. Allowed values: 'running', 'paused', 'completed', 'failed'. Omit to return all statuses.",
                        "enum": ["running", "paused", "completed", "failed"]
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of executions to return. Defaults to 50. Use with offset for pagination through large result sets."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Number of executions to skip before returning results. Defaults to 0. Combine with limit for pagination (e.g., offset=50, limit=50 for page 2)."
                    },
                    "tenant_id": {
                        "type": "string",
                        "description": "Tenant partition to query. Defaults to 'default'. Only executions in this tenant are returned. Outside dev mode the tenant is the API token's tenant and a different value is refused."
                    }
                }
            }),
        },
        move |args: Value, ctx: ToolCallContext| {
            let s = s.clone();
            async move {
                let caller = resolve_caller(&ctx, &args, dev_mode, Role::Viewer)?;
                let tenant_id = caller.tenant.clone();
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
    let server = server.register_tool_with_context(
        McpTool {
            name: "jamjet_cancel_execution".into(),
            description: Some(concat!(
                "Cancel a running or paused workflow execution. This is an irreversible, destructive operation. ",
                "Side effects: appends a WorkflowCancelled event to the execution's event log and sets the status to 'cancelled'. ",
                "The execution cannot be resumed after cancellation — start a new execution with jamjet_run_workflow if needed. ",
                "Use this when a workflow is stuck, no longer needed, or was started with incorrect input. ",
                "Returns a JSON object with execution_id and status 'cancelled'. ",
                "Fails if the execution is already in a terminal state (completed, failed, or cancelled) or if the execution_id is not found."
            ).into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "execution_id": {
                        "type": "string",
                        "description": "Execution ID to cancel. Accepts 'exec_<uuid>' or bare UUID format. The execution must be in 'running' or 'paused' state."
                    },
                    "tenant_id": {
                        "type": "string",
                        "description": "Tenant partition. Defaults to 'default'. Must match the tenant used when the execution was created. Outside dev mode the tenant is the API token's tenant and a different value is refused."
                    }
                },
                "required": ["execution_id"]
            }),
        },
        move |args: Value, ctx: ToolCallContext| {
            let s = s.clone();
            async move {
                let id_str = args
                    .get("execution_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let caller = resolve_caller(&ctx, &args, dev_mode, Role::Developer)?;
                let tenant_id = caller.tenant.clone();
                let eid = parse_execution_id(id_str)?;
                let backend = s.backend_for(&tenant_id);

                let exec = backend
                    .get_execution(&eid)
                    .await
                    .map_err(|e| format!("{e}"))?
                    .ok_or_else(|| format!("execution {id_str} not found"))?;

                if exec.status.is_terminal() {
                    return Err(format!(
                        "execution {id_str} is already terminal: {:?}",
                        exec.status
                    ));
                }

                // Event and status in one write, like the cancel route
                // (PR #140 re-review, I-A).
                let event = Event::new(
                    eid.clone(),
                    0,
                    EventKind::WorkflowCancelled {
                        reason: Some("cancelled via MCP".into()),
                    },
                );
                if let jamjet_state::ExecutionFinish::AlreadyTerminal(status) = backend
                    .finish_execution(event, WorkflowStatus::Cancelled)
                    .await
                    .map_err(|e| format!("{e}"))?
                {
                    return Err(format!(
                        "execution {id_str} is already terminal: {status:?}"
                    ));
                }

                Ok(vec![McpContent::Text {
                    text: json!({"execution_id": id_str, "status": "cancelled"}).to_string(),
                }])
            }
        },
    );

    // ── jamjet_get_events ────────────────────────────────────────────────
    let s = st.clone();
    let server = server.register_tool_with_context(
        McpTool {
            name: "jamjet_get_events".into(),
            description: Some(concat!(
                "Retrieve the full, ordered event log for a workflow execution. Read-only, no side effects. ",
                "Use this to debug execution behavior, understand which nodes ran and in what order, or inspect approval decisions. ",
                "Returns a JSON object with an 'events' array. Each event has: execution_id, sequence (monotonic counter), ",
                "timestamp, and kind (one of: WorkflowStarted, NodeScheduled, NodeStarted, NodeCompleted, NodeFailed, ",
                "ApprovalRequested, ApprovalReceived, WorkflowCompleted, WorkflowCancelled, WorkflowFailed). ",
                "Events are returned in sequence order (oldest first) and represent the complete, immutable audit trail. ",
                "For a high-level status summary, use jamjet_get_execution instead."
            ).into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "execution_id": {
                        "type": "string",
                        "description": "Execution ID to retrieve events for. Accepts 'exec_<uuid>' or bare UUID format."
                    },
                    "tenant_id": {
                        "type": "string",
                        "description": "Tenant partition to query. Defaults to 'default'. Must match the tenant used when the execution was created. Outside dev mode the tenant is the API token's tenant and a different value is refused."
                    }
                },
                "required": ["execution_id"]
            }),
        },
        move |args: Value, ctx: ToolCallContext| {
            let s = s.clone();
            async move {
                let id_str = args
                    .get("execution_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let caller = resolve_caller(&ctx, &args, dev_mode, Role::Viewer)?;
                let tenant_id = caller.tenant.clone();
                let eid = parse_execution_id(id_str)?;
                let backend = s.backend_for(&tenant_id);
                let events = backend.get_events(&eid).await.map_err(|e| format!("{e}"))?;
                let text = serde_json::to_string(&json!({"events": events}))
                    .map_err(|e| format!("{e}"))?;
                Ok(vec![McpContent::Text { text }])
            }
        },
    );

    // ── jamjet_approve ───────────────────────────────────────────────────
    let s = st.clone();
    let server = server.register_tool_with_context(
        McpTool {
            name: "jamjet_approve".into(),
            description: Some(concat!(
                "Submit an approval or rejection decision for a workflow execution that is paused and waiting for human review. ",
                "Use this when jamjet_list_executions shows a 'paused' execution or jamjet_get_events shows an ApprovalRequested event. ",
                "Side effects: appends an ApprovalReceived event to the event log, recording the authenticated API token as the approver ('mcp-client' only in dev mode), and, if the execution is paused, ",
                "resumes it to 'running' status so the next node can proceed. The decision is recorded in the immutable audit trail. ",
                "Returns a JSON object with execution_id and accepted: true. ",
                "Fails if execution_id is not found, if decision is not exactly 'approved' or 'rejected', or if nothing is pending approval (or node_id does not match a pending approval). ",
                "Related: use jamjet_get_events to see the ApprovalRequested event details before deciding."
            ).into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "execution_id": {
                        "type": "string",
                        "description": "Execution ID of the paused workflow awaiting approval. Accepts 'exec_<uuid>' or bare UUID format."
                    },
                    "decision": {
                        "type": "string",
                        "description": "The approval decision. Must be exactly 'approved' or 'rejected'. 'approved' resumes the workflow; 'rejected' records the rejection.",
                        "enum": ["approved", "rejected"]
                    },
                    "node_id": {
                        "type": "string",
                        "description": "ID of the node that requested approval. Helps correlate the decision with the correct approval gate when a workflow has multiple."
                    },
                    "comment": {
                        "type": "string",
                        "description": "Optional free-text comment explaining the decision. Recorded in the audit trail alongside the approval event."
                    },
                    "action_digest": {
                        "type": "string",
                        "description": "The action_digest (or, for an agent tool dispatch, the calls_hash) from the context of the pending approval you reviewed. Required to approve outside dev mode. If the pending request has been superseded the decision is refused, so you never approve an action you did not see."
                    },
                    "tenant_id": {
                        "type": "string",
                        "description": "Tenant partition. Defaults to 'default'. Must match the tenant used when the execution was created. Outside dev mode the tenant is the API token's tenant and a different value is refused."
                    }
                },
                "required": ["execution_id", "decision"]
            }),
        },
        move |args: Value, ctx: ToolCallContext| {
            let s = s.clone();
            async move {
                let id_str = args
                    .get("execution_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let decision_str = args.get("decision").and_then(|v| v.as_str()).unwrap_or("");
                let node_id = args
                    .get("node_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let comment = args
                    .get("comment")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let action_digest = match args.get("action_digest") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(d)) => Some(d.clone()),
                    Some(_) => return Err("action_digest must be a string".into()),
                };
                // Forwarded so the shared submission refuses it exactly as the
                // REST route does; silently dropping it would let a client
                // believe its edit was recorded.
                let state_patch = args.get("state_patch").cloned();
                // Reviewer and above may decide, matching the REST route.
                let caller = resolve_caller(&ctx, &args, dev_mode, Role::Reviewer)?;
                let tenant_id = caller.tenant.clone();

                let decision = match decision_str {
                    "approved" => jamjet_state::event::ApprovalDecision::Approved,
                    "rejected" => jamjet_state::event::ApprovalDecision::Rejected,
                    other => return Err(format!("unknown decision: {other}")),
                };

                let eid = parse_execution_id(id_str)?;
                let backend = s.backend_for(&tenant_id);

                // The approver is the authenticated token, never a
                // client-chosen string. Dev mode has no token.
                let (user_id, principal) = match &caller.token {
                    Some(t) => (t.name.clone(), Some(crate::auth::principal_of(t))),
                    None => ("mcp-client".to_string(), None),
                };
                let actor_id = principal.clone().unwrap_or_else(|| user_id.clone());

                let (resolved_node, event) = crate::approvals::submit_approval(
                    &backend,
                    &eid,
                    crate::approvals::ApprovalSubmission {
                        node_id: if node_id.is_empty() {
                            None
                        } else {
                            Some(node_id)
                        },
                        user_id,
                        principal,
                        decision,
                        comment,
                        state_patch,
                        action_digest,
                        dev_mode,
                    },
                    s.approval_policy,
                )
                .await
                .map_err(|e| e.to_string())?;

                // Seal the approval into the signed, hash-chained audit log,
                // mirroring the REST approve path: best-effort, after the
                // durable event write, off the worker's fenced commit path.
                let audit_ctx = jamjet_audit::RequestContext {
                    actor_id,
                    actor_type: jamjet_audit::ActorType::Human,
                    tenant_id: tenant_id.0.clone(),
                    method: Some("POST".to_string()),
                    path: Some("/mcp/tools/jamjet_approve".to_string()),
                    ..Default::default()
                };
                if let Err(e) = s.enricher.try_enrich_and_append(&event, Some(&audit_ctx)).await {
                    jamjet_state::approvals::record_seal_gap(backend.as_ref(), &event, e).await;
                }

                Ok(vec![McpContent::Text {
                    text: json!({"execution_id": id_str, "node_id": resolved_node, "accepted": true})
                        .to_string(),
                }])
            }
        },
    );

    // ── jamjet_list_agents ───────────────────────────────────────────────
    let s = st.clone();
    let server = server.register_tool_with_context(
        McpTool {
            name: "jamjet_list_agents".into(),
            description: Some(concat!(
                "List all agents registered in the runtime, with optional filters by status, skill, or protocol. Read-only, no side effects. ",
                "Use this to discover which agents are available before routing work, or to check the health/status of registered agents. ",
                "Returns a JSON object with an 'agents' array. Each entry includes the agent's ID, name, description, skills, protocol, status, and Agent Card metadata. ",
                "All filter parameters are optional and can be combined — omit all to list every registered agent. ",
                "Returns an empty array if no agents match the filters. ",
                "Related: use jamjet_discover_agent to register a new remote agent before listing."
            ).into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "description": "Filter agents by lifecycle status. Allowed values: 'registered' (known but not started), 'active' (running and available), 'paused' (temporarily offline), 'deactivated' (permanently removed). Omit to return all statuses.",
                        "enum": ["registered", "active", "paused", "deactivated"]
                    },
                    "skill": {
                        "type": "string",
                        "description": "Filter to agents that declare this skill (e.g., 'data-analysis', 'translation'). Matches against the agent's skills list."
                    },
                    "protocol": {
                        "type": "string",
                        "description": "Filter to agents using this protocol (e.g., 'a2a', 'mcp', 'rest'). Useful for finding agents reachable via a specific communication method."
                    }
                }
            }),
        },
        move |args: Value, ctx: ToolCallContext| {
            let s = s.clone();
            async move {
                resolve_caller(&ctx, &args, dev_mode, Role::Viewer)?;
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
                let agents = s.agents.find(filter).await.map_err(|e| e.to_string())?;
                let text = serde_json::to_string(&json!({"agents": agents})).map_err(|e| format!("{e}"))?;
                Ok(vec![McpContent::Text { text }])
            }
        },
    );

    // ── jamjet_discover_agent ────────────────────────────────────────────
    let s = st.clone();
    let server = server.register_tool_with_context(
        McpTool {
            name: "jamjet_discover_agent".into(),
            description: Some(concat!(
                "Discover and register a remote agent by fetching its Agent Card from the given URL. ",
                "Side effects: makes an outbound HTTP request to the URL to retrieve the agent's metadata (Agent Card), ",
                "then registers the agent in the local runtime registry so it becomes available for routing and invocation. ",
                "Use this to onboard external agents (A2A, MCP, or REST) before they can appear in jamjet_list_agents or be routed to by a Coordinator. ",
                "Returns the full JSON Agent Card of the newly registered agent, including its ID, name, skills, protocol, and endpoint. ",
                "Fails if the URL is unreachable, does not serve a valid Agent Card, or if a network error occurs. ",
                "This operation is idempotent — discovering the same URL again updates the existing registration."
            ).into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "HTTPS URL of the remote agent to discover. The agent must serve an Agent Card (A2A/.well-known/agent.json or equivalent metadata endpoint). Example: 'https://agents.example.com/research-agent'."
                    }
                },
                "required": ["url"]
            }),
        },
        move |args: Value, ctx: ToolCallContext| {
            let s = s.clone();
            async move {
                resolve_caller(&ctx, &args, dev_mode, Role::Developer)?;
                let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
                let agent = s.agents.discover_remote(url).await.map_err(|e| e.to_string())?;
                let text = serde_json::to_string(&agent).map_err(|e| format!("{e}"))?;
                Ok(vec![McpContent::Text { text }])
            }
        },
    );

    server.into_router()
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Who is calling a bridge tool, resolved from the transport.
struct Caller {
    tenant: TenantId,
    /// The authenticated token. `None` only in dev mode.
    token: Option<ApiToken>,
}

/// Resolve and authorise the caller of a bridge tool.
///
/// Outside dev mode the identity is the [`ApiToken`] that `require_auth` put in
/// the request extensions. Every branch that cannot establish it refuses:
/// no token (the auth layer is missing or was bypassed), a role below `need`,
/// a `tenant_id` argument that is not a string, or one that names a tenant
/// other than the token's. The argument is only ever allowed to agree.
fn resolve_caller(
    ctx: &ToolCallContext,
    args: &Value,
    dev_mode: bool,
    need: Role,
) -> Result<Caller, String> {
    let arg_tenant = match args.get("tenant_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(t)) => Some(t.as_str()),
        Some(_) => return Err("tenant_id must be a string".into()),
    };

    if dev_mode {
        return Ok(Caller {
            tenant: TenantId::from(arg_tenant.unwrap_or("default")),
            token: None,
        });
    }

    let Some(token) = ctx.extensions.get::<ApiToken>().cloned() else {
        return Err("unauthenticated: a Bearer API token is required".into());
    };
    // `require_auth` already refuses these; checked again so the bridge stays
    // closed if it is ever mounted without that layer.
    let token = crate::auth::admit_token(token, crate::auth::tenant_tokens_as_default())
        .map_err(str::to_string)?;
    if Role::parse_role(&token.role) < need {
        return Err(format!(
            "insufficient role '{}' for this tool; {:?} or above required",
            token.role, need
        ));
    }
    if let Some(t) = arg_tenant {
        if t != token.tenant_id {
            return Err(format!(
                "tenant_id '{t}' does not match the API token's tenant"
            ));
        }
    }
    Ok(Caller {
        tenant: TenantId::from(token.tenant_id.clone()),
        token: Some(token),
    })
}

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
