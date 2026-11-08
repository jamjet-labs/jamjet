//! `POST /work-items/claim` with `execution_id` (and `node_id`): a claim for
//! one execution's work, made by its initiator, with a reason when nothing
//! comes back. And `external_fn` nodes opted in with `claim: "by_execution"`,
//! whose items the oldest-first claim never hands out.
//!
//! The Spring integration registers an `external_fn` node per recorded call
//! on a queue the app owns. With oldest-first claiming only, concurrent calls
//! took each other's nodes, any other claimer on the queue could take them
//! too (so a blocked call could run through a backlog), and a lost claim
//! response lost the item.
mod support;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_api::state::AppState;
use jamjet_ir::workflow::WorkflowIr;
use jamjet_state::backend::{StateBackend, WorkItem};
use jamjet_state::{ClaimFilter, EventKind, InMemoryBackend, SqliteBackend};
use serde_json::{json, Value};
use std::sync::Arc;
use support::{make_state, post};
use tower::ServiceExt;

const QUEUE: &str = "app-calls";

/// `first -> second`, both `external_fn` on [`QUEUE`]; `claim` is put on
/// both when given.
fn ir(workflow_id: &str, claim: Option<&str>, policy: Option<Value>) -> Value {
    let node = |id: &str| {
        let mut kind =
            json!({ "type": "external_fn", "queue": QUEUE, "handler": id, "output_schema": "" });
        if let Some(c) = claim {
            kind["claim"] = json!(c);
        }
        json!({
            "id": id, "kind": kind,
            "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
        })
    };
    let mut ir = json!({
        "workflow_id": workflow_id,
        "version": "0.1.0",
        "name": null, "description": null, "state_schema": "",
        "start_node": "first",
        "nodes": { "first": node("first"), "second": node("second") },
        "edges": [
            { "from": "first", "to": "second", "condition": null },
            { "from": "second", "to": "end", "condition": null }
        ],
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    });
    if let Some(p) = policy {
        ir["policy"] = p;
    }
    ir
}

/// A request through the router, with authentication on (`token`) or in dev
/// mode (`None`).
async fn send(
    state: &AppState,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (u16, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = build_router_with_opts(state.clone(), token.is_none())
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn register(state: &AppState, token: Option<&str>, ir: Value) {
    let (st, body) = send(state, "POST", "/workflows", token, json!({ "ir": ir })).await;
    assert!((200..300).contains(&st), "{st} {body}");
}

async fn start(state: &AppState, token: Option<&str>, workflow_id: &str) -> String {
    let (st, body) = send(
        state,
        "POST",
        "/executions",
        token,
        json!({ "workflow_id": workflow_id, "workflow_version": "0.1.0", "input": {} }),
    )
    .await;
    assert!((200..300).contains(&st), "{st} {body}");
    body["execution_id"].as_str().unwrap().to_string()
}

async fn claim(state: &AppState, token: Option<&str>, worker: &str, extra: Value) -> (u16, Value) {
    let mut req = json!({ "worker_id": worker, "queue_types": [QUEUE] });
    for (k, v) in extra.as_object().unwrap() {
        req[k] = v.clone();
    }
    send(state, "POST", "/work-items/claim", token, req).await
}

/// Enqueue `second` for `execution_id`, as the scheduler would once `first`
/// completed, so one execution holds two items on the queue.
async fn enqueue_second(backend: &Arc<dyn StateBackend>, workflow_ir: Value, execution_id: &str) {
    let ir: WorkflowIr = serde_json::from_value(workflow_ir).unwrap();
    let eid = jamjet_core::workflow::ExecutionId(
        uuid::Uuid::parse_str(execution_id.trim_start_matches("exec_")).unwrap(),
    );
    let node = ir.node("second").unwrap();
    let payload = jamjet_scheduler::payload::build_work_item_payload(
        &ir,
        "second",
        node,
        &serde_json::Map::new(),
        &ir.workflow_id,
        "0.1.0",
    );
    backend
        .enqueue_work_item(WorkItem {
            id: uuid::Uuid::new_v4(),
            execution_id: eid,
            node_id: "second".into(),
            queue_type: QUEUE.into(),
            payload,
            attempt: 0,
            max_attempts: 3,
            created_at: chrono::Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            tenant_id: "default".into(),
            lease_fence: 0,
        })
        .await
        .unwrap();
}

fn execution_id(raw: &str) -> jamjet_core::workflow::ExecutionId {
    jamjet_core::workflow::ExecutionId(
        uuid::Uuid::parse_str(raw.trim_start_matches("exec_")).unwrap(),
    )
}

async fn event_kinds(backend: &Arc<dyn StateBackend>, raw: &str) -> Vec<EventKind> {
    backend
        .get_events(&execution_id(raw))
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect()
}

fn sqlite_url() -> (String, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("jjclaim-{}.db", uuid::Uuid::new_v4()));
    (format!("sqlite://{}", path.display()), path)
}

// ── The filter, in dev mode, on every backend ────────────────────────────────

async fn filter_basics(backend: Arc<dyn StateBackend>) {
    let state = make_state(backend.clone());
    register(&state, None, ir("wf-calls", None, None)).await;
    let a = start(&state, None, "wf-calls").await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let b = start(&state, None, "wf-calls").await;
    enqueue_second(&backend, ir("wf-calls", None, None), &a).await;

    // The later execution's item, though A's is older.
    let (st, body) = claim(&state, None, "w", json!({ "execution_id": b })).await;
    assert_eq!(st, 200);
    assert_eq!(body["claimed"], true, "{body}");
    assert_eq!(body["work_item"]["execution_id"], json!(b));

    // One node of an execution that has two items on the queue.
    let (_, body) = claim(
        &state,
        None,
        "w2",
        json!({ "execution_id": a, "node_id": "second" }),
    )
    .await;
    assert_eq!(body["claimed"], true, "{body}");
    assert_eq!(body["work_item"]["node_id"], "second");

    // No such execution.
    let foreign = format!("exec_{}", uuid::Uuid::new_v4().simple());
    let (st, body) = claim(&state, None, "w", json!({ "execution_id": foreign })).await;
    assert_eq!(st, 200);
    assert_eq!(body, json!({ "claimed": false, "reason": "not_found" }));

    // No filter: the oldest item, as before (A's `first`), and an empty queue
    // still answers exactly `{"claimed": false}`.
    let (_, body) = claim(&state, None, "w3", json!({})).await;
    assert_eq!(body["claimed"], true, "{body}");
    assert_eq!(body["work_item"]["execution_id"], json!(a));
    assert_eq!(body["work_item"]["node_id"], "first");
    let (_, body) = claim(&state, None, "w3", json!({})).await;
    assert_eq!(body, json!({ "claimed": false }));

    // A node filter needs an execution, and an execution id must parse.
    let (st, _) = claim(&state, None, "w", json!({ "node_id": "first" })).await;
    assert_eq!(st, 400);
    let (st, _) = claim(&state, None, "w", json!({ "execution_id": "not-an-id" })).await;
    assert_eq!(st, 400);
}

#[tokio::test]
async fn the_filter_on_memory() {
    filter_basics(Arc::new(InMemoryBackend::new())).await;
}

#[tokio::test]
async fn the_filter_on_sqlite() {
    let (url, path) = sqlite_url();
    filter_basics(Arc::new(SqliteBackend::open(&url).await.unwrap())).await;
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn the_filter_on_the_tenant_scoped_backend() {
    let (url, path) = sqlite_url();
    let base = SqliteBackend::open(&url).await.unwrap();
    filter_basics(Arc::new(
        base.for_tenant(jamjet_state::TenantId::from("default")),
    ))
    .await;
    let _ = std::fs::remove_file(path);
}

// ── The opt-in: never handed out oldest-first ───────────────────────────────

async fn opted_in_items_stay_with_their_execution(backend: Arc<dyn StateBackend>) {
    let state = make_state(backend.clone());
    register(&state, None, ir("wf-own", Some("by_execution"), None)).await;
    let e = start(&state, None, "wf-own").await;
    let (_, body) = claim(&state, None, "other-worker", json!({})).await;
    assert_eq!(
        body,
        json!({ "claimed": false }),
        "the oldest-first claim must not take an opted-in item"
    );
    let (_, body) = claim(&state, None, "owner", json!({ "execution_id": e })).await;
    assert_eq!(body["claimed"], true, "{body}");
    assert_eq!(body["work_item"]["payload"]["claim"], "by_execution");
}

#[tokio::test]
async fn opted_in_items_on_memory() {
    opted_in_items_stay_with_their_execution(Arc::new(InMemoryBackend::new())).await;
}

#[tokio::test]
async fn opted_in_items_on_sqlite() {
    let (url, path) = sqlite_url();
    opted_in_items_stay_with_their_execution(Arc::new(SqliteBackend::open(&url).await.unwrap()))
        .await;
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn opted_in_items_on_the_tenant_scoped_backend() {
    let (url, path) = sqlite_url();
    let base = SqliteBackend::open(&url).await.unwrap();
    opted_in_items_stay_with_their_execution(Arc::new(
        base.for_tenant(jamjet_state::TenantId::from("default")),
    ))
    .await;
    let _ = std::fs::remove_file(path);
}

/// The opt-in is validated when the workflow is registered.
#[tokio::test]
async fn an_unknown_claim_mode_is_refused_at_registration() {
    let state = make_state(Arc::new(InMemoryBackend::new()));
    let (st, body) = send(
        &state,
        "POST",
        "/workflows",
        None,
        json!({ "ir": ir("wf-bad", Some("whoever"), None) }),
    )
    .await;
    assert!((400..500).contains(&st), "{st} {body}");
}

// ── Who may claim by id ─────────────────────────────────────────────────────

/// Only the run's initiator may claim its work by id; another principal is
/// told `not_found`, as for a run that does not exist. A run that recorded no
/// initiator can be claimed by an operator only.
#[tokio::test]
async fn only_the_initiator_claims_by_execution() {
    let (url, path) = sqlite_url();
    let sqlite = Arc::new(SqliteBackend::open(&url).await.unwrap());
    let (alice, _) = sqlite.create_token("alice", "developer").await.unwrap();
    let (bob, _) = sqlite.create_token("bob", "developer").await.unwrap();
    let (ops, _) = sqlite.create_token("ops", "operator").await.unwrap();
    let backend: Arc<dyn StateBackend> = sqlite;
    let state = make_state(backend);
    register(
        &state,
        Some(&alice),
        ir("wf-mine", Some("by_execution"), None),
    )
    .await;

    let e = start(&state, Some(&alice), "wf-mine").await;
    let (_, body) = claim(
        &state,
        Some(&bob),
        "bob-worker",
        json!({ "execution_id": e }),
    )
    .await;
    assert_eq!(body, json!({ "claimed": false, "reason": "not_found" }));
    let (_, body) = claim(
        &state,
        Some(&ops),
        "ops-worker",
        json!({ "execution_id": e }),
    )
    .await;
    assert_eq!(
        body,
        json!({ "claimed": false, "reason": "not_found" }),
        "a recorded initiator binds operators too"
    );
    let (_, body) = claim(
        &state,
        Some(&alice),
        "alice-worker",
        json!({ "execution_id": e }),
    )
    .await;
    assert_eq!(body["claimed"], true, "{body}");

    // Started in dev mode: no initiator. A developer gets nothing, an
    // operator gets the item.
    let legacy = start(&state, None, "wf-mine").await;
    let (_, body) = claim(
        &state,
        Some(&alice),
        "alice-worker",
        json!({ "execution_id": legacy }),
    )
    .await;
    assert_eq!(body, json!({ "claimed": false, "reason": "not_found" }));
    let (_, body) = claim(
        &state,
        Some(&ops),
        "ops-worker",
        json!({ "execution_id": legacy }),
    )
    .await;
    assert_eq!(body["claimed"], true, "{body}");
    let _ = std::fs::remove_file(path);
}

// ── Reasons, waiting, and the same lease ─────────────────────────────────────

/// A blocked node is refused with `blocked`, through the same policy gate as
/// the oldest-first claim, and its payload is not handed out.
#[tokio::test]
async fn a_blocked_node_is_refused_with_its_reason() {
    let state = make_state(Arc::new(InMemoryBackend::new()));
    let policy =
        json!({ "blocked_tools": ["first"], "require_approval_for": [], "model_allowlist": [] });
    register(
        &state,
        None,
        ir("wf-block", Some("by_execution"), Some(policy)),
    )
    .await;
    let e = start(&state, None, "wf-block").await;
    let (_, body) = claim(&state, None, "w", json!({ "execution_id": e })).await;
    assert_eq!(body, json!({ "claimed": false, "reason": "blocked" }));
}

#[tokio::test]
async fn a_gated_node_is_refused_with_approval_required() {
    let state = make_state(Arc::new(InMemoryBackend::new()));
    let policy =
        json!({ "blocked_tools": [], "require_approval_for": ["first"], "model_allowlist": [] });
    register(
        &state,
        None,
        ir("wf-gate", Some("by_execution"), Some(policy)),
    )
    .await;
    let e = start(&state, None, "wf-gate").await;
    let (_, body) = claim(&state, None, "w", json!({ "execution_id": e })).await;
    assert_eq!(
        body,
        json!({ "claimed": false, "reason": "approval_required" })
    );
}

/// Another worker's lease: `claimed_by_other`. The same worker again: the
/// same item and the same lease fence, so a lost claim response does not
/// lose the item.
#[tokio::test]
async fn the_same_worker_gets_its_lease_again_and_another_is_told_why() {
    let state = make_state(Arc::new(InMemoryBackend::new()));
    register(&state, None, ir("wf-lease", Some("by_execution"), None)).await;
    let e = start(&state, None, "wf-lease").await;
    let (_, first) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
    assert_eq!(first["claimed"], true, "{first}");
    let (_, again) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
    assert_eq!(again["claimed"], true, "{again}");
    assert_eq!(again["work_item"]["id"], first["work_item"]["id"]);
    assert_eq!(
        again["work_item"]["lease_fence"],
        first["work_item"]["lease_fence"]
    );
    let (_, other) = claim(&state, None, "w2", json!({ "execution_id": e })).await;
    assert_eq!(
        other,
        json!({ "claimed": false, "reason": "claimed_by_other" })
    );
}

/// A lease handed out again is checked against the policy again: a node
/// blocked since the first claim is not handed out a second time.
#[tokio::test]
async fn a_lease_handed_out_again_is_checked_again() {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let state = make_state(backend.clone());
    register(&state, None, ir("wf-recheck", Some("by_execution"), None)).await;
    let e = start(&state, None, "wf-recheck").await;
    let (_, first) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
    assert_eq!(first["claimed"], true, "{first}");
    // The memory backend starts with no tenant record: create it, then set
    // its policy.
    let (st, body) = post(
        &state,
        "/tenants",
        json!({ "id": "default", "name": "default" }),
    )
    .await;
    assert!(st.is_success() || st.as_u16() == 409, "{st} {body}");
    let (st, body) = send(
        &state,
        "PUT",
        "/tenants/default",
        None,
        json!({ "policy": { "blocked_tools": ["first"], "require_approval_for": [], "model_allowlist": [] } }),
    )
    .await;
    assert!((200..300).contains(&st), "{st} {body}");
    let (_, again) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
    assert_eq!(again, json!({ "claimed": false, "reason": "blocked" }));
    // Recorded as any block is, and the node failed for good.
    let kinds = event_kinds(&backend, &e).await;
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, EventKind::PolicyViolation { node_id, .. } if node_id == "first")),
        "no PolicyViolation: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| matches!(k, EventKind::NodeFailed { node_id, retryable: false, .. } if node_id == "first")),
        "node not failed: {kinds:?}"
    );
}

/// A lease that no gate decided (an infra error left it behind) is decided
/// when the same worker claims again: the approval request is raised, and
/// the node is not failed.
#[tokio::test]
async fn a_lease_left_undecided_is_decided_on_the_next_claim() {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let state = make_state(backend.clone());
    let policy =
        json!({ "blocked_tools": [], "require_approval_for": ["first"], "model_allowlist": [] });
    register(
        &state,
        None,
        ir("wf-undecided", Some("by_execution"), Some(policy)),
    )
    .await;
    let e = start(&state, None, "wf-undecided").await;
    // Leased to w1 with no gate run, as a lookup error leaves it.
    let leased = backend
        .claim_work_item_filtered(
            "w1",
            &[QUEUE],
            Some(60),
            &ClaimFilter {
                execution_id: Some(execution_id(&e)),
                node_id: None,
            },
        )
        .await
        .unwrap();
    assert!(leased.is_some());

    let (_, body) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
    assert_eq!(
        body,
        json!({ "claimed": false, "reason": "approval_required" })
    );
    let kinds = event_kinds(&backend, &e).await;
    assert!(
        kinds.iter().any(
            |k| matches!(k, EventKind::ToolApprovalRequired { node_id, .. } if node_id == "first")
        ),
        "no approval request raised: {kinds:?}"
    );
    assert!(
        !kinds
            .iter()
            .any(|k| matches!(k, EventKind::NodeFailed { .. })),
        "node failed: {kinds:?}"
    );
}

/// The same for a node acting for an agent: a re-check admits only a step
/// an earlier claim admitted, so a lease no gate decided is refused there.
/// It must be decided again (the step admitted, the item handed out), not
/// failed.
#[tokio::test]
async fn an_undecided_agent_step_is_admitted_not_failed() {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let state = make_state(backend.clone());
    let agent = "jamjet://acme/caller";
    let (st, body) = post(
        &state,
        "/agents",
        json!({
            "id": agent, "uri": agent, "name": "caller", "description": "",
            "version": "1.0.0",
            "capabilities": { "skills": [], "protocols": [], "tools_provided": [], "tools_consumed": [] },
            "autonomy": "fully_autonomous",
            "constraints": { "max_iterations": 5 },
            "auth": { "type": "none" }
        }),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    let mut workflow = ir("wf-agent", Some("by_execution"), None);
    workflow["agent_ref"] = json!(agent);
    register(&state, None, workflow).await;
    let e = start(&state, None, "wf-agent").await;
    let leased = backend
        .claim_work_item_filtered(
            "w1",
            &[QUEUE],
            Some(60),
            &ClaimFilter {
                execution_id: Some(execution_id(&e)),
                node_id: None,
            },
        )
        .await
        .unwrap();
    assert!(leased.is_some());

    let (_, body) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
    assert_eq!(body["claimed"], true, "{body}");
    let kinds = event_kinds(&backend, &e).await;
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, EventKind::AgentStepAdmitted { .. })),
        "step not admitted: {kinds:?}"
    );
    assert!(
        !kinds
            .iter()
            .any(|k| matches!(k, EventKind::NodeFailed { .. })),
        "node failed: {kinds:?}"
    );
}

/// `not_ready` when the item is not queued yet, and `wait_ms` waits for it.
#[tokio::test]
async fn not_ready_and_a_bounded_wait() {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let state = make_state(backend.clone());
    let workflow = ir("wf-wait", Some("by_execution"), None);
    register(&state, None, workflow.clone()).await;
    let e = start(&state, None, "wf-wait").await;
    let second = json!({ "execution_id": e, "node_id": "second" });
    let (_, body) = claim(&state, None, "w", second.clone()).await;
    assert_eq!(body, json!({ "claimed": false, "reason": "not_ready" }));

    // The item appears while the claim waits.
    let b2 = backend.clone();
    let e2 = e.clone();
    let wf2 = workflow.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        enqueue_second(&b2, wf2, &e2).await;
    });
    let mut waiting = second.clone();
    waiting["wait_ms"] = json!(3000);
    let started = std::time::Instant::now();
    let (_, body) = claim(&state, None, "w", waiting).await;
    assert_eq!(body["claimed"], true, "{body}");
    assert_eq!(body["work_item"]["node_id"], "second");
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
}

/// A wait longer than the cap is cut to the cap (10 s), so a claim cannot
/// hold a request open indefinitely.
#[tokio::test(start_paused = true)]
async fn a_long_wait_is_cut_to_the_cap() {
    let state = make_state(Arc::new(InMemoryBackend::new()));
    register(&state, None, ir("wf-cap", Some("by_execution"), None)).await;
    let e = start(&state, None, "wf-cap").await;
    let started = tokio::time::Instant::now();
    let (_, body) = claim(
        &state,
        None,
        "w",
        json!({ "execution_id": e, "node_id": "second", "wait_ms": 3_600_000 }),
    )
    .await;
    assert_eq!(body, json!({ "claimed": false, "reason": "not_ready" }));
    let waited = started.elapsed();
    let cap = std::time::Duration::from_millis(jamjet_api::routes::MAX_CLAIM_WAIT_MS);
    assert!(
        waited >= cap && waited < cap + std::time::Duration::from_secs(1),
        "{waited:?}"
    );
}
