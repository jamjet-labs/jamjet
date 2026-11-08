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

/// [`send`], with the status and the `Retry-After` header.
async fn send_with_headers(
    state: &AppState,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (u16, Option<String>, Value) {
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
    let retry_after = resp
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        retry_after,
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
    enqueue(backend, workflow_ir, execution_id, "second", json!({})).await;
}

/// Enqueue `node` for `execution_id` as the scheduler would, its payload
/// built from the run state `state`.
async fn enqueue(
    backend: &Arc<dyn StateBackend>,
    workflow_ir: Value,
    execution_id: &str,
    node_id: &str,
    state: Value,
) {
    let ir: WorkflowIr = serde_json::from_value(workflow_ir).unwrap();
    let eid = jamjet_core::workflow::ExecutionId(
        uuid::Uuid::parse_str(execution_id.trim_start_matches("exec_")).unwrap(),
    );
    let node = ir.node(node_id).unwrap();
    let payload = jamjet_scheduler::payload::build_work_item_payload(
        &ir,
        node_id,
        node,
        state.as_object().unwrap(),
        &ir.workflow_id,
        "0.1.0",
    );
    backend
        .enqueue_work_item(WorkItem {
            id: uuid::Uuid::new_v4(),
            execution_id: eid,
            node_id: node_id.into(),
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
    // Checked whatever the queues: naming only an engine queue used to
    // answer `{"claimed": false}` before the check ran.
    let (st, _) = claim(
        &state,
        None,
        "w",
        json!({ "node_id": "first", "queue_types": ["general"] }),
    )
    .await;
    assert_eq!(st, 400);
    let (_, body) = claim(
        &state,
        None,
        "w",
        json!({ "execution_id": foreign, "queue_types": ["general"] }),
    )
    .await;
    assert_eq!(body, json!({ "claimed": false, "reason": "not_found" }));
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
    // Nor a claim that names the node but not the execution.
    let by_node = backend
        .claim_work_item_filtered(
            "other-worker",
            &[QUEUE],
            None,
            &ClaimFilter {
                execution_id: None,
                node_id: Some("first".into()),
            },
        )
        .await;
    assert!(
        !matches!(by_node, Ok(Some(_))),
        "a node-only filter took an opted-in item"
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

// ── Reasons, waiting, and the same lease, on every backend ───────────────────

/// One backend of each kind, with the file to remove after.
async fn backends() -> Vec<(
    &'static str,
    Arc<dyn StateBackend>,
    Option<std::path::PathBuf>,
)> {
    backends_with_pools()
        .await
        .into_iter()
        .map(|(n, b, p, _)| (n, b, p))
        .collect()
}

/// [`backends`], with the plain SQLite backend's pool: that backend manages
/// no tenants through the API.
#[allow(clippy::type_complexity)]
async fn backends_with_pools() -> Vec<(
    &'static str,
    Arc<dyn StateBackend>,
    Option<std::path::PathBuf>,
    Option<sqlx::SqlitePool>,
)> {
    let (url, path) = sqlite_url();
    let plain = SqliteBackend::open(&url).await.unwrap();
    let pool = plain.pool();
    let sqlite: Arc<dyn StateBackend> = Arc::new(plain);
    let (url2, path2) = sqlite_url();
    let scoped: Arc<dyn StateBackend> = Arc::new(
        SqliteBackend::open(&url2)
            .await
            .unwrap()
            .for_tenant(jamjet_state::TenantId::from("default")),
    );
    vec![
        ("memory", Arc::new(InMemoryBackend::new()), None, None),
        ("sqlite", sqlite, Some(path), Some(pool)),
        ("tenant_scoped", scoped, Some(path2), None),
    ]
}

fn cleanup(path: Option<std::path::PathBuf>) {
    if let Some(p) = path {
        let _ = std::fs::remove_file(p);
    }
}

fn policy(blocked: &[&str], gated: &[&str]) -> Value {
    json!({ "blocked_tools": blocked, "require_approval_for": gated, "model_allowlist": [] })
}

fn count(kinds: &[EventKind], pred: impl Fn(&EventKind) -> bool) -> usize {
    kinds.iter().filter(|k| pred(k)).count()
}

/// A blocked node is refused with `blocked`, through the same policy gate as
/// the oldest-first claim, and its payload is not handed out. A gated one
/// with `approval_required`.
#[tokio::test]
async fn blocked_and_gated_nodes_are_refused_with_their_reason() {
    for (name, backend, path) in backends().await {
        let state = make_state(backend.clone());
        register(
            &state,
            None,
            ir(
                "wf-block",
                Some("by_execution"),
                Some(policy(&["first"], &[])),
            ),
        )
        .await;
        let e = start(&state, None, "wf-block").await;
        let (_, body) = claim(&state, None, "w", json!({ "execution_id": e })).await;
        assert_eq!(
            body,
            json!({ "claimed": false, "reason": "blocked" }),
            "{name}"
        );

        register(
            &state,
            None,
            ir(
                "wf-gate",
                Some("by_execution"),
                Some(policy(&[], &["first"])),
            ),
        )
        .await;
        let e = start(&state, None, "wf-gate").await;
        let (_, body) = claim(&state, None, "w", json!({ "execution_id": e })).await;
        assert_eq!(
            body,
            json!({ "claimed": false, "reason": "approval_required" }),
            "{name}"
        );
        cleanup(path);
    }
}

/// Another worker's lease: `claimed_by_other`. The same worker again: the
/// same item and the same lease fence, so a lost claim response does not
/// lose the item. The lease is renewed, and the answer gives the length it
/// holds, not the one the retry asked for.
#[tokio::test]
async fn the_same_worker_gets_its_lease_again_and_another_is_told_why() {
    for (name, backend, path) in backends().await {
        let state = make_state(backend.clone());
        register(&state, None, ir("wf-lease", Some("by_execution"), None)).await;
        let e = start(&state, None, "wf-lease").await;
        let (_, first) = claim(
            &state,
            None,
            "w1",
            json!({ "execution_id": e, "lease_secs": 60 }),
        )
        .await;
        assert_eq!(first["claimed"], true, "{name}: {first}");
        let id = uuid::Uuid::parse_str(first["work_item"]["id"].as_str().unwrap()).unwrap();
        let before = backend
            .get_work_item(id)
            .await
            .unwrap()
            .unwrap()
            .lease_expires_at;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let (_, again) = claim(
            &state,
            None,
            "w1",
            json!({ "execution_id": e, "lease_secs": 300 }),
        )
        .await;
        assert_eq!(again["claimed"], true, "{name}: {again}");
        assert_eq!(again["work_item"]["id"], first["work_item"]["id"], "{name}");
        assert_eq!(
            again["work_item"]["lease_fence"], first["work_item"]["lease_fence"],
            "{name}"
        );
        assert_eq!(first["work_item"]["lease_secs"], 60, "{name}: {first}");
        assert_eq!(again["work_item"]["lease_secs"], 60, "{name}: {again}");
        let after = backend
            .get_work_item(id)
            .await
            .unwrap()
            .unwrap()
            .lease_expires_at;
        assert!(
            after > before,
            "{name}: the lease was not renewed: {before:?} {after:?}"
        );

        let (_, other) = claim(&state, None, "w2", json!({ "execution_id": e })).await;
        assert_eq!(
            other,
            json!({ "claimed": false, "reason": "claimed_by_other" }),
            "{name}"
        );
        cleanup(path);
    }
}

/// Set the default tenant's policy: through `PUT /tenants` where the
/// backend manages tenants, else in its table (the plain SQLite backend does
/// not; its `default` tenant row comes from the migrations).
async fn set_tenant_policy(
    state: &AppState,
    backend: &Arc<dyn StateBackend>,
    pool: Option<&sqlx::SqlitePool>,
    name: &str,
    p: Value,
) {
    let default = jamjet_state::TenantId::from("default");
    if backend.get_tenant(&default).await.unwrap().is_none() {
        let (st, body) = post(
            state,
            "/tenants",
            json!({ "id": "default", "name": "default" }),
        )
        .await;
        assert!(st.is_success(), "{name}: {st} {body}");
    }
    let (st, body) = send(
        state,
        "PUT",
        "/tenants/default",
        None,
        json!({ "policy": p.clone() }),
    )
    .await;
    if (200..300).contains(&st) {
        return;
    }
    assert!(
        body.to_string().contains("not supported"),
        "{name}: {st} {body}"
    );
    sqlx::query("UPDATE tenants SET policy_json = ? WHERE id = 'default'")
        .bind(p.to_string())
        .execute(pool.expect("the plain SQLite backend's pool"))
        .await
        .unwrap();
}

/// A lease handed out again is checked against the policy again: a node
/// blocked since the first claim is not handed out a second time, the block
/// is recorded once, and the node failed.
#[tokio::test]
async fn a_lease_handed_out_again_is_checked_again() {
    for (name, backend, path, pool) in backends_with_pools().await {
        let state = make_state(backend.clone());
        register(&state, None, ir("wf-recheck", Some("by_execution"), None)).await;
        let e = start(&state, None, "wf-recheck").await;
        let (_, first) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(first["claimed"], true, "{name}: {first}");
        set_tenant_policy(
            &state,
            &backend,
            pool.as_ref(),
            name,
            policy(&["first"], &[]),
        )
        .await;
        let (_, again) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(
            again,
            json!({ "claimed": false, "reason": "blocked" }),
            "{name}"
        );
        let kinds = event_kinds(&backend, &e).await;
        assert_eq!(
            count(
                &kinds,
                |k| matches!(k, EventKind::PolicyViolation { node_id, .. } if node_id == "first")
            ),
            1,
            "{name}: {kinds:?}"
        );
        assert_eq!(
            count(
                &kinds,
                |k| matches!(k, EventKind::NodeFailed { node_id, retryable: false, .. } if node_id == "first")
            ),
            1,
            "{name}: {kinds:?}"
        );
        cleanup(path);
    }
}

/// Lease `node` of `e` to `worker` with its gate undecided, as a gate that
/// could not decide (a lookup error) leaves it.
async fn lease_undecided(backend: &Arc<dyn StateBackend>, e: &str, node: &str, worker: &str) {
    let leased = lease_gating(backend, e, node, worker).await;
    assert!(backend
        .swap_gate(
            leased.id,
            leased.lease_fence,
            jamjet_state::GateMark::Gating,
            jamjet_state::GateMark::Undecided
        )
        .await
        .unwrap());
}

/// Lease `node` of `e` to `worker` as a claim by execution does: marked as
/// gating in the same statement, before any gate runs.
async fn lease_gating(
    backend: &Arc<dyn StateBackend>,
    e: &str,
    node: &str,
    worker: &str,
) -> WorkItem {
    backend
        .claim_work_item_filtered(
            worker,
            &[QUEUE],
            Some(60),
            &ClaimFilter {
                execution_id: Some(execution_id(e)),
                node_id: Some(node.into()),
            },
        )
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("nothing to lease for {node}"))
}

/// A lease no gate decided is decided when the same worker claims again:
/// the approval request is raised, and the node is not failed.
#[tokio::test]
async fn a_lease_left_undecided_is_decided_on_the_next_claim() {
    for (name, backend, path) in backends().await {
        let state = make_state(backend.clone());
        register(
            &state,
            None,
            ir(
                "wf-undecided",
                Some("by_execution"),
                Some(policy(&[], &["first"])),
            ),
        )
        .await;
        let e = start(&state, None, "wf-undecided").await;
        lease_undecided(&backend, &e, "first", "w1").await;

        let (_, body) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(
            body,
            json!({ "claimed": false, "reason": "approval_required" }),
            "{name}"
        );
        let kinds = event_kinds(&backend, &e).await;
        assert_eq!(
            count(
                &kinds,
                |k| matches!(k, EventKind::ToolApprovalRequired { node_id, .. } if node_id == "first")
            ),
            1,
            "{name}: {kinds:?}"
        );
        assert_eq!(
            count(&kinds, |k| matches!(k, EventKind::NodeFailed { .. })),
            0,
            "{name}"
        );
        cleanup(path);
    }
}

/// The same for a node acting for an agent: a completion-style re-check
/// admits only a step an earlier claim admitted, so a lease no gate decided
/// must be decided again (the step admitted once, the item handed out), not
/// failed.
#[tokio::test]
async fn an_undecided_agent_step_is_admitted_not_failed() {
    for (name, backend, path) in backends().await {
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
        assert!(st.is_success(), "{name}: {st} {body}");
        let mut workflow = ir("wf-agent", Some("by_execution"), None);
        workflow["agent_ref"] = json!(agent);
        register(&state, None, workflow).await;
        let e = start(&state, None, "wf-agent").await;
        lease_undecided(&backend, &e, "first", "w1").await;

        let (_, body) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(body["claimed"], true, "{name}: {body}");
        let kinds = event_kinds(&backend, &e).await;
        assert_eq!(
            count(&kinds, |k| matches!(k, EventKind::AgentStepAdmitted { .. })),
            1,
            "{name}: {kinds:?}"
        );
        assert_eq!(
            count(&kinds, |k| matches!(k, EventKind::NodeFailed { .. })),
            0,
            "{name}"
        );
        cleanup(path);
    }
}

/// A gated node approved and dispatched again: the claim releases it under
/// the approval, and the same worker claiming again gets the same lease.
#[tokio::test]
async fn an_approved_node_is_released_and_handed_out_again() {
    for (name, backend, path) in backends().await {
        let state = make_state(backend.clone());
        let workflow = ir(
            "wf-approved",
            Some("by_execution"),
            Some(policy(&[], &["first"])),
        );
        register(&state, None, workflow.clone()).await;
        let e = start(&state, None, "wf-approved").await;
        let (_, body) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(body["reason"], "approval_required", "{name}: {body}");
        let (st, body) = send(
            &state,
            "POST",
            &format!("/executions/{e}/approve"),
            None,
            json!({ "decision": "approved", "node_id": "first" }),
        )
        .await;
        assert_eq!(st, 200, "{name}: {body}");
        // The scheduler dispatches the approved node again, same input.
        enqueue(&backend, workflow.clone(), &e, "first", json!({})).await;

        let (_, first) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(first["claimed"], true, "{name}: {first}");
        let (_, again) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(again["claimed"], true, "{name}: {again}");
        assert_eq!(
            again["work_item"]["lease_fence"], first["work_item"]["lease_fence"],
            "{name}"
        );
        cleanup(path);
    }
}

/// A lease no gate released is not re-checked as a completion. Completion
/// checking accepts an approval for the node's current occurrence whatever
/// payload it was asked for, so an approval of one payload would release
/// another one: here the run state moved between the request and the
/// dispatch after the approval, and the interrupted claim's lease must raise
/// a new request.
#[tokio::test]
async fn an_approval_of_another_payload_does_not_release_an_undecided_lease() {
    for (name, backend, path) in backends().await {
        let state = make_state(backend.clone());
        let workflow = ir(
            "wf-moved",
            Some("by_execution"),
            Some(policy(&[], &["first"])),
        );
        register(&state, None, workflow.clone()).await;
        let e = start(&state, None, "wf-moved").await;
        let (_, body) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(body["reason"], "approval_required", "{name}: {body}");
        let (st, body) = send(
            &state,
            "POST",
            &format!("/executions/{e}/approve"),
            None,
            json!({ "decision": "approved", "node_id": "first" }),
        )
        .await;
        assert_eq!(st, 200, "{name}: {body}");
        // Dispatched again with another input, and the claim of it cut short.
        enqueue(
            &backend,
            workflow.clone(),
            &e,
            "first",
            json!({ "amount": 9_000_000 }),
        )
        .await;
        lease_undecided(&backend, &e, "first", "w1").await;

        let (_, body) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(
            body,
            json!({ "claimed": false, "reason": "approval_required" }),
            "{name}: released under an approval of another payload"
        );
        let kinds = event_kinds(&backend, &e).await;
        assert_eq!(
            count(
                &kinds,
                |k| matches!(k, EventKind::ToolApprovalRequired { node_id, .. } if node_id == "first")
            ),
            2,
            "{name}: {kinds:?}"
        );
        cleanup(path);
    }
}

/// `not_ready` when the item is not queued yet, and `wait_ms` waits for it.
#[tokio::test]
async fn not_ready_and_a_bounded_wait() {
    for (name, backend, path) in backends().await {
        let state = make_state(backend.clone());
        let workflow = ir("wf-wait", Some("by_execution"), None);
        register(&state, None, workflow.clone()).await;
        let e = start(&state, None, "wf-wait").await;
        let second = json!({ "execution_id": e, "node_id": "second" });
        let (_, body) = claim(&state, None, "w", second.clone()).await;
        assert_eq!(
            body,
            json!({ "claimed": false, "reason": "not_ready" }),
            "{name}"
        );

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
        assert_eq!(body["claimed"], true, "{name}: {body}");
        assert_eq!(body["work_item"]["node_id"], "second", "{name}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "{name}"
        );
        cleanup(path);
    }
}

/// A run that ends while a claim waits for it answers `not_found` then, not
/// `not_ready` at the end of the wait.
#[tokio::test]
async fn a_wait_ends_when_the_run_does() {
    for (name, backend, path) in backends().await {
        let state = make_state(backend.clone());
        register(&state, None, ir("wf-ends", Some("by_execution"), None)).await;
        let e = start(&state, None, "wf-ends").await;
        let state2 = state.clone();
        let e2 = e.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let (st, body) = send(
                &state2,
                "POST",
                &format!("/executions/{e2}/cancel"),
                None,
                json!({}),
            )
            .await;
            assert!((200..300).contains(&st), "{st} {body}");
        });
        let started = std::time::Instant::now();
        let (_, body) = claim(
            &state,
            None,
            "w",
            json!({ "execution_id": e, "node_id": "second", "wait_ms": 5000 }),
        )
        .await;
        assert_eq!(
            body,
            json!({ "claimed": false, "reason": "not_found", "status": "cancelled" }),
            "{name}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "{name}"
        );
        cleanup(path);
    }
}

/// The items of a run that ended are settled by a claim for it: the
/// oldest-first claim, which settles other ended runs' items, never takes
/// these.
#[tokio::test]
async fn an_ended_runs_items_are_settled() {
    for (name, backend, path) in backends().await {
        let state = make_state(backend.clone());
        register(&state, None, ir("wf-ended", Some("by_execution"), None)).await;
        let e = start(&state, None, "wf-ended").await;
        let (st, body) = send(
            &state,
            "POST",
            &format!("/executions/{e}/cancel"),
            None,
            json!({}),
        )
        .await;
        assert!((200..300).contains(&st), "{name}: {st} {body}");
        let filter = ClaimFilter {
            execution_id: Some(execution_id(&e)),
            node_id: None,
        };
        assert!(
            backend.has_claimable(&[QUEUE], &filter).await.unwrap(),
            "{name}"
        );
        let (_, body) = claim(&state, None, "w", json!({ "execution_id": e })).await;
        assert_eq!(
            body,
            json!({ "claimed": false, "reason": "not_found", "status": "cancelled" }),
            "{name}"
        );
        assert!(
            !backend.has_claimable(&[QUEUE], &filter).await.unwrap(),
            "{name}: the ended run's item is still pending"
        );
        cleanup(path);
    }
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

/// One principal holds at most a few waiting claims; the next one answers at
/// once, with a 429 and `Retry-After`, not a `not_ready` that reads like a
/// wait that ran out. With its own token: the count is per process, and the
/// other tests wait as `dev`.
#[tokio::test]
async fn waiting_claims_are_capped_per_principal() {
    let (url, path) = sqlite_url();
    let sqlite = Arc::new(SqliteBackend::open(&url).await.unwrap());
    let (token, _) = sqlite.create_token("waiter", "developer").await.unwrap();
    let backend: Arc<dyn StateBackend> = sqlite;
    let state = make_state(backend);
    register(
        &state,
        Some(&token),
        ir("wf-many", Some("by_execution"), None),
    )
    .await;
    let e = start(&state, Some(&token), "wf-many").await;
    let req = json!({ "execution_id": e, "node_id": "second", "wait_ms": 2000 });
    let mut waiters = Vec::new();
    for i in 0..jamjet_api::routes::MAX_WAITING_CLAIMS_PER_PRINCIPAL {
        let (state, req, token) = (state.clone(), req.clone(), token.clone());
        waiters.push(tokio::spawn(async move {
            claim(&state, Some(&token), &format!("w{i}"), req).await
        }));
    }
    // Let them all start waiting.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    let mut over = req.clone();
    over["worker_id"] = json!("one-more");
    over["queue_types"] = json!([QUEUE]);
    let (st, retry_after, body) =
        send_with_headers(&state, "POST", "/work-items/claim", Some(&token), over).await;
    assert_eq!(st, 429, "{body}");
    assert_eq!(retry_after.as_deref(), Some("1"), "{body}");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(1000),
        "the claim past the cap waited {:?}",
        started.elapsed()
    );
    for w in waiters {
        let (_, body) = w.await.unwrap();
        assert_eq!(body, json!({ "claimed": false, "reason": "not_ready" }));
    }
    let _ = std::fs::remove_file(path);
}

/// A retry that arrives while the first claim's gate is still deciding (its
/// client gave up early) waits for it: deciding the same item beside a
/// running gate wrote the gate's events twice.
#[tokio::test]
async fn a_lease_whose_gate_is_running_is_not_decided_twice() {
    for (name, backend, path) in backends().await {
        let state = make_state(backend.clone());
        register(
            &state,
            None,
            ir(
                "wf-gating",
                Some("by_execution"),
                Some(policy(&[], &["first"])),
            ),
        )
        .await;
        let e = start(&state, None, "wf-gating").await;
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
            .unwrap()
            .unwrap();
        assert!(backend
            .mark_gate(
                leased.id,
                leased.lease_fence,
                jamjet_state::GateMark::Gating
            )
            .await
            .unwrap());

        let (_, body) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(
            body,
            json!({ "claimed": false, "reason": "not_ready" }),
            "{name}"
        );
        let kinds = event_kinds(&backend, &e).await;
        assert_eq!(
            count(&kinds, |k| matches!(
                k,
                EventKind::ToolApprovalRequired { .. }
            )),
            0,
            "{name}: decided beside the running gate: {kinds:?}"
        );
        let still = backend.get_work_item(leased.id).await.unwrap().unwrap();
        assert_eq!(
            still.lease_fence, leased.lease_fence,
            "{name}: the lease was taken"
        );
        cleanup(path);
    }
}

/// Makes one claim as `w1` from inside the first full read of the run's log
/// after it is armed: a retry landing while the first claim's gate decides.
struct RetryDuringGate {
    state: std::sync::OnceLock<AppState>,
    execution: std::sync::OnceLock<String>,
    fired: std::sync::atomic::AtomicBool,
    answer: std::sync::Mutex<Option<Value>>,
}

#[async_trait::async_trait]
impl support::hooked::AfterWrite for RetryDuringGate {
    async fn after(&self, write: support::hooked::Write) {
        let support::hooked::Write::GetEvents(_) = write else {
            return;
        };
        let (Some(state), Some(e)) = (self.state.get(), self.execution.get()) else {
            return;
        };
        if self.fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let (_, body) = claim(state, None, "w1", json!({ "execution_id": e })).await;
        *self.answer.lock().unwrap() = Some(body);
    }
}

/// A retry from the same worker while the first claim's gate is deciding
/// (a client that gave up early) waits for that gate. Deciding the item
/// beside it admitted the agent's step twice for one item.
#[tokio::test]
async fn a_retry_during_the_gate_waits_for_it() {
    let hook = Arc::new(RetryDuringGate {
        state: std::sync::OnceLock::new(),
        execution: std::sync::OnceLock::new(),
        fired: std::sync::atomic::AtomicBool::new(false),
        answer: std::sync::Mutex::new(None),
    });
    let inner: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let backend: Arc<dyn StateBackend> = Arc::new(support::hooked::HookedBackend::new(
        inner.clone(),
        hook.clone(),
    ));
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
    let mut workflow = ir("wf-race", Some("by_execution"), None);
    workflow["agent_ref"] = json!(agent);
    register(&state, None, workflow).await;
    let e = start(&state, None, "wf-race").await;
    hook.state.set(state.clone()).ok().unwrap();
    hook.execution.set(e.clone()).unwrap();

    let (_, first) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
    let retry = hook.answer.lock().unwrap().clone().expect("the retry ran");
    assert_eq!(
        retry,
        json!({ "claimed": false, "reason": "not_ready" }),
        "{first}"
    );
    assert_eq!(first["claimed"], true, "{first}");
    let kinds = event_kinds(&inner, &e).await;
    assert_eq!(
        count(&kinds, |k| matches!(k, EventKind::AgentStepAdmitted { .. })),
        1,
        "{kinds:?}"
    );
}

/// A lease the oldest-first claim handed out is on record as released, so
/// the same worker claiming it again by execution gets it back, not a second
/// decision that moves its fence.
#[tokio::test]
async fn an_oldest_first_lease_is_handed_out_again_as_released() {
    for (name, backend, path) in backends().await {
        let state = make_state(backend.clone());
        register(&state, None, ir("wf-mixed", None, None)).await;
        let e = start(&state, None, "wf-mixed").await;
        let (_, first) = claim(&state, None, "w1", json!({})).await;
        assert_eq!(first["claimed"], true, "{name}: {first}");
        let (_, again) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(again["claimed"], true, "{name}: {again}");
        assert_eq!(
            again["work_item"]["lease_fence"], first["work_item"]["lease_fence"],
            "{name}"
        );
        cleanup(path);
    }
}

// ── Enqueue by hand, and acting on an item ───────────────────────────────────

/// `POST /work-items` takes the marker from the IR, not the caller: sent
/// without it, an opted-in node's item is still never handed out
/// oldest-first, and a caller cannot mark another node's item.
#[tokio::test]
async fn a_hand_enqueued_item_is_marked_from_the_ir() {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let state = make_state(backend.clone());
    register(&state, None, ir("wf-hand", Some("by_execution"), None)).await;
    let e = start(&state, None, "wf-hand").await;
    // Take the start item out of the way.
    let (_, body) = claim(&state, None, "w", json!({ "execution_id": e })).await;
    assert_eq!(body["claimed"], true, "{body}");
    let (st, body) = send(
        &state,
        "POST",
        "/work-items",
        None,
        json!({ "execution_id": e, "node_id": "second", "queue_type": QUEUE, "payload": { "handler": "second" } }),
    )
    .await;
    assert!((200..300).contains(&st), "{st} {body}");
    let (_, body) = claim(&state, None, "oldest-first", json!({})).await;
    assert_eq!(body, json!({ "claimed": false }));
    let (_, body) = claim(
        &state,
        None,
        "w",
        json!({ "execution_id": e, "node_id": "second" }),
    )
    .await;
    assert_eq!(
        body["work_item"]["payload"]["claim"], "by_execution",
        "{body}"
    );

    register(&state, None, ir("wf-plain", None, None)).await;
    let p = start(&state, None, "wf-plain").await;
    let (_, body) = claim(&state, None, "w", json!({ "execution_id": p })).await;
    assert_eq!(body["claimed"], true, "{body}");
    let (st, body) = send(
        &state,
        "POST",
        "/work-items",
        None,
        json!({ "execution_id": p, "node_id": "second", "queue_type": QUEUE, "payload": { "claim": "by_execution" } }),
    )
    .await;
    assert!((200..300).contains(&st), "{st} {body}");
    let (_, body) = claim(&state, None, "oldest-first", json!({})).await;
    assert_eq!(body["claimed"], true, "{body}");
    assert!(
        body["work_item"]["payload"].get("claim").is_none(),
        "{body}"
    );
}

/// Completing, failing or heartbeating an opted-in item takes the same right
/// as claiming it: another principal is answered as for an item that does not
/// exist, though it knows the id and the fence (a fence is a counter, often
/// 1, not a credential).
#[tokio::test]
async fn only_the_initiator_completes_fails_or_heartbeats() {
    let (url, path) = sqlite_url();
    let sqlite = Arc::new(SqliteBackend::open(&url).await.unwrap());
    let (alice, _) = sqlite.create_token("alice", "developer").await.unwrap();
    let (bob, _) = sqlite.create_token("bob", "developer").await.unwrap();
    let backend: Arc<dyn StateBackend> = sqlite;
    let state = make_state(backend);
    register(
        &state,
        Some(&alice),
        ir("wf-act", Some("by_execution"), None),
    )
    .await;
    let e = start(&state, Some(&alice), "wf-act").await;
    let (_, body) = claim(
        &state,
        Some(&alice),
        "alice-worker",
        json!({ "execution_id": e }),
    )
    .await;
    assert_eq!(body["claimed"], true, "{body}");
    let id = body["work_item"]["id"].as_str().unwrap().to_string();
    let fence = body["work_item"]["lease_fence"].clone();

    let (st, body) = send(
        &state,
        "POST",
        &format!("/work-items/{id}/heartbeat"),
        Some(&bob),
        json!({ "worker_id": "alice-worker", "lease_fence": fence }),
    )
    .await;
    assert_eq!(st, 404, "{body}");
    let (st, body) = send(
        &state,
        "POST",
        &format!("/work-items/{id}/fail"),
        Some(&bob),
        json!({ "error": "no", "lease_fence": fence }),
    )
    .await;
    assert_eq!(st, 404, "{body}");
    for with_fence in [true, false] {
        let mut req = json!({ "output": { "forged": true }, "state_patch": {} });
        if with_fence {
            req["lease_fence"] = fence.clone();
        }
        let (st, body) = send(
            &state,
            "POST",
            &format!("/work-items/{id}/complete"),
            Some(&bob),
            req,
        )
        .await;
        assert_eq!(st, 404, "fence {with_fence}: {body}");
    }
    let (st, body) = send(
        &state,
        "POST",
        &format!("/work-items/{id}/complete"),
        Some(&alice),
        json!({ "output": {}, "state_patch": {}, "lease_fence": fence }),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    let _ = std::fs::remove_file(path);
}

/// `claim` means something on `external_fn` only; anywhere else it is
/// refused at registration rather than silently ignored.
#[tokio::test]
async fn claim_on_another_kind_is_refused_at_registration() {
    let state = make_state(Arc::new(InMemoryBackend::new()));
    let mut workflow = ir("wf-wrong", None, None);
    workflow["nodes"]["second"]["kind"] = json!({
        "type": "python_fn", "module": "m", "function": "f", "output_schema": "",
        "claim": "by_execution"
    });
    let (st, body) = send(
        &state,
        "POST",
        "/workflows",
        None,
        json!({ "ir": workflow }),
    )
    .await;
    assert_eq!(st, 400, "{body}");
    assert!(body.to_string().contains("external_fn"), "{body}");
}

// ── Second review: the gap after a claim, stable reasons, and the gate ───────

type Action = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
type Trigger = fn(&support::hooked::Write) -> bool;

/// Runs one action, once, on the first write that matches its trigger after
/// it is armed.
#[derive(Default)]
struct Once {
    armed: std::sync::Mutex<Option<(Trigger, Action)>>,
}

impl Once {
    fn arm(
        &self,
        trigger: Trigger,
        action: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        *self.armed.lock().unwrap() = Some((trigger, Box::pin(action)));
    }
}

#[async_trait::async_trait]
impl support::hooked::AfterWrite for Once {
    async fn after(&self, write: support::hooked::Write) {
        let action = {
            let mut armed = self.armed.lock().unwrap();
            match armed.as_ref() {
                Some((trigger, _)) if trigger(&write) => armed.take().map(|(_, a)| a),
                _ => None,
            }
        };
        if let Some(action) = action {
            action.await;
        }
    }
}

/// Each backend behind a [`Once`] hook, with the inner backend and a way to
/// backdate a lease on it.
#[allow(clippy::type_complexity)]
async fn hooked_backends() -> Vec<(
    &'static str,
    Arc<dyn StateBackend>,
    Arc<dyn StateBackend>,
    Arc<Once>,
    Expire,
    Option<std::path::PathBuf>,
)> {
    let mut out = Vec::new();
    let memory = Arc::new(InMemoryBackend::new());
    let (url, path) = sqlite_url();
    let sqlite: Arc<dyn StateBackend> = Arc::new(SqliteBackend::open(&url).await.unwrap());
    let (url2, path2) = sqlite_url();
    let scoped: Arc<dyn StateBackend> = Arc::new(
        SqliteBackend::open(&url2)
            .await
            .unwrap()
            .for_tenant(jamjet_state::TenantId::from("default")),
    );
    for (name, inner, expire, path) in [
        (
            "memory",
            memory.clone() as Arc<dyn StateBackend>,
            Expire::Memory(memory),
            None,
        ),
        ("sqlite", sqlite, Expire::Sql(url), Some(path)),
        ("tenant_scoped", scoped, Expire::Sql(url2), Some(path2)),
    ] {
        let hook = Arc::new(Once::default());
        let hooked: Arc<dyn StateBackend> = Arc::new(support::hooked::HookedBackend::new(
            inner.clone(),
            hook.clone(),
        ));
        out.push((name, hooked, inner, hook, expire, path));
    }
    out
}

/// Backdates a lease so the next sweep finds it expired.
#[derive(Clone)]
enum Expire {
    Memory(Arc<InMemoryBackend>),
    Sql(String),
}

impl Expire {
    async fn sql(url: &str, sql: &str, id: uuid::Uuid, at: Option<String>) {
        let pool = sqlx::SqlitePool::connect(url).await.unwrap();
        let mut q = sqlx::query(sql);
        if let Some(at) = at {
            q = q.bind(at);
        }
        q.bind(id.to_string()).execute(&pool).await.unwrap();
        pool.close().await;
    }

    /// Set the lease's expiry to `at`.
    async fn lease_until(&self, id: uuid::Uuid, at: chrono::DateTime<chrono::Utc>) {
        match self {
            Expire::Memory(b) => b.set_lease_expiry_for_test(id, at),
            Expire::Sql(url) => {
                let sql = "UPDATE work_items SET lease_expires_at = ? WHERE id = ?";
                Self::sql(url, sql, id, Some(at.to_rfc3339())).await
            }
        }
    }

    /// Make a requeued item claimable now (the sweep backs off a retry).
    async fn ready(&self, id: uuid::Uuid) {
        if let Expire::Sql(url) = self {
            let sql = "UPDATE work_items SET retry_after = NULL WHERE id = ?";
            Self::sql(url, sql, id, None).await
        }
    }

    async fn lease(&self, id: uuid::Uuid) {
        match self {
            Expire::Memory(b) => b.force_lease_expired_for_test(id),
            Expire::Sql(url) => {
                let pool = sqlx::SqlitePool::connect(url).await.unwrap();
                sqlx::query(
                    "UPDATE work_items SET lease_expires_at = '2020-01-01T00:00:00+00:00' WHERE id = ?",
                )
                .bind(id.to_string())
                .execute(&pool)
                .await
                .unwrap();
                pool.close().await;
            }
        }
    }
}

async fn register_agent(state: &AppState) -> &'static str {
    let agent = "jamjet://acme/caller";
    let (st, body) = post(
        state,
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
    agent
}

fn by_this(e: &str) -> ClaimFilter {
    ClaimFilter {
        execution_id: Some(execution_id(e)),
        node_id: None,
    }
}

/// A retry from the same worker that lands right after the first claim's
/// lease, before anything else of that claim ran, waits for it. The lease
/// used to be marked as gating in a write of its own after the claim, and a
/// retry in between decided the item beside the first claim and handed it
/// out a second time.
#[tokio::test]
async fn a_retry_right_after_the_lease_is_not_handed_the_item_too() {
    for (name, backend, inner, hook, _, path) in hooked_backends().await {
        let state = make_state(backend.clone());
        let agent = register_agent(&state).await;
        let mut workflow = ir("wf-gap", Some("by_execution"), None);
        workflow["agent_ref"] = json!(agent);
        register(&state, None, workflow).await;
        let e = start(&state, None, "wf-gap").await;

        let answer = Arc::new(std::sync::Mutex::new(None));
        let (st2, e2, a2) = (state.clone(), e.clone(), answer.clone());
        hook.arm(
            |w| matches!(w, support::hooked::Write::Claimed(_)),
            async move {
                let (_, body) = claim(&st2, None, "w1", json!({ "execution_id": e2 })).await;
                *a2.lock().unwrap() = Some(body);
            },
        );
        let (_, first) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        let retry = answer.lock().unwrap().clone().expect("the retry ran");
        assert_eq!(
            retry,
            json!({ "claimed": false, "reason": "not_ready" }),
            "{name}: {first}"
        );
        assert_eq!(first["claimed"], true, "{name}: {first}");
        let kinds = event_kinds(&inner, &e).await;
        assert_eq!(
            count(&kinds, |k| matches!(k, EventKind::AgentStepAdmitted { .. })),
            1,
            "{name}: {kinds:?}"
        );
        cleanup(path);
    }
}

/// A refusal is the same however often it is asked, and whoever asks: the
/// gate settles the item it refuses, and a retry used to find nothing and
/// say `not_ready` after a `blocked`. A run that ended says so, with its
/// status.
#[tokio::test]
async fn a_refusal_is_the_same_on_every_retry() {
    for (name, backend, path) in backends().await {
        let state = make_state(backend.clone());
        for (wf, p, reason) in [
            ("wf-stable-block", policy(&["first"], &[]), "blocked"),
            (
                "wf-stable-gate",
                policy(&[], &["first"]),
                "approval_required",
            ),
        ] {
            register(&state, None, ir(wf, Some("by_execution"), Some(p))).await;
            let e = start(&state, None, wf).await;
            for worker in ["w1", "w1", "w2"] {
                let (_, body) = claim(&state, None, worker, json!({ "execution_id": e })).await;
                assert_eq!(
                    body,
                    json!({ "claimed": false, "reason": reason }),
                    "{name}: {wf} as {worker}"
                );
            }
            let (_, body) = claim(
                &state,
                None,
                "w2",
                json!({ "execution_id": e, "node_id": "first" }),
            )
            .await;
            assert_eq!(
                body,
                json!({ "claimed": false, "reason": reason }),
                "{name}: {wf} by node"
            );
            if reason == "approval_required" {
                let (st, body) = send(
                    &state,
                    "POST",
                    &format!("/executions/{e}/cancel"),
                    None,
                    json!({}),
                )
                .await;
                assert!((200..300).contains(&st), "{name}: {st} {body}");
                let (_, body) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
                assert_eq!(
                    body,
                    json!({ "claimed": false, "reason": "not_found", "status": "cancelled" }),
                    "{name}"
                );
            }
        }
        cleanup(path);
    }
}

/// A client that gives up while the gate decides does not leave the lease
/// half decided: the gate runs to its end in a task of its own, and the same
/// worker claiming again gets the released lease. It used to stay marked as
/// gating, answering `not_ready` until the lease expired.
#[tokio::test]
async fn a_gate_finishes_when_its_client_gives_up() {
    for (name, backend, inner, hook, _, path) in hooked_backends().await {
        let state = make_state(backend.clone());
        let agent = register_agent(&state).await;
        let mut workflow = ir("wf-drop", Some("by_execution"), None);
        workflow["agent_ref"] = json!(agent);
        register(&state, None, workflow).await;
        let e = start(&state, None, "wf-drop").await;
        hook.arm(
            |w| matches!(w, support::hooked::Write::GetEvents(_)),
            tokio::time::sleep(std::time::Duration::from_millis(400)),
        );
        let gave_up = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            claim(&state, None, "w1", json!({ "execution_id": e })),
        )
        .await;
        assert!(gave_up.is_err(), "{name}: the gate did not stall");
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        let leased = inner.leased_items(&[QUEUE], &by_this(&e)).await.unwrap();
        assert_eq!(leased.len(), 1, "{name}");
        assert_eq!(
            leased[0].gate,
            jamjet_state::GateMark::Released,
            "{name}: the gate stopped with its request"
        );
        let (_, again) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(again["claimed"], true, "{name}: {again}");
        assert_eq!(
            again["work_item"]["lease_fence"],
            json!(leased[0].item.lease_fence),
            "{name}"
        );
        let kinds = event_kinds(&inner, &e).await;
        assert_eq!(
            count(&kinds, |k| matches!(k, EventKind::AgentStepAdmitted { .. })),
            1,
            "{name}: {kinds:?}"
        );
        cleanup(path);
    }
}

/// A lease that expires while still marked as gating is charged like any
/// other: the gate now always finishes (a task of its own) or clears the mark
/// when it dies, so a mark left behind is a crash, and a crash every try must
/// end in the dead-letter queue, not loop at attempt 0. And a re-handed lease
/// is marked gating while it may already be running.
#[tokio::test]
async fn a_gating_lease_that_expires_is_charged_like_any_other() {
    for (name, backend, _inner, _hook, expire, path) in hooked_backends().await {
        let state = make_state(backend.clone());
        register(&state, None, ir("wf-expire", Some("by_execution"), None)).await;
        let e = start(&state, None, "wf-expire").await;

        // The sweep.
        let leased = lease_gating(&backend, &e, "first", "w1").await;
        expire.lease(leased.id).await;
        let swept = backend.reclaim_expired_leases().await.unwrap();
        assert_eq!(swept.retryable.len(), 1, "{name}: {swept:?}");
        let item = backend.get_work_item(leased.id).await.unwrap().unwrap();
        assert_eq!(item.attempt, 1, "{name}: no attempt was charged");
        assert!(item.worker_id.is_none(), "{name}: still leased");

        // A claim's own expiry pass.
        expire.ready(leased.id).await;
        let leased = lease_gating(&backend, &e, "first", "w1").await;
        expire.lease(leased.id).await;
        let (_, body) = claim(&state, None, "w2", json!({ "execution_id": e })).await;
        assert_eq!(body["claimed"], true, "{name}: {body}");
        let item = backend.get_work_item(leased.id).await.unwrap().unwrap();
        assert_eq!(item.attempt, 2, "{name}: no attempt was charged");
        cleanup(path);
    }
}

/// A worker's own lease, taken by a request of its own while this one looked,
/// is its lease: handed back, not `claimed_by_other`, even when another
/// worker's lease on the same run was seen first.
#[tokio::test]
async fn ones_own_lease_is_not_claimed_by_other() {
    for (name, backend, inner, hook, _, path) in hooked_backends().await {
        let state = make_state(backend.clone());
        let workflow = ir("wf-own", Some("by_execution"), None);
        register(&state, None, workflow.clone()).await;
        let e = start(&state, None, "wf-own").await;
        lease_gating(&inner, &e, "first", "w2").await;
        enqueue_second(&inner, workflow, &e).await;

        let fence = Arc::new(std::sync::Mutex::new(None));
        let (i2, e2, f2) = (inner.clone(), e.clone(), fence.clone());
        hook.arm(
            |w| matches!(w, support::hooked::Write::LeasedItems),
            async move {
                // The worker's other request: it leased `second` and its gate
                // released it.
                let mine = lease_gating(&i2, &e2, "second", "w1").await;
                assert!(i2
                    .swap_gate(
                        mine.id,
                        mine.lease_fence,
                        jamjet_state::GateMark::Gating,
                        jamjet_state::GateMark::Released
                    )
                    .await
                    .unwrap());
                *f2.lock().unwrap() = Some(mine.lease_fence);
            },
        );
        let (_, body) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(body["claimed"], true, "{name}: {body}");
        assert_eq!(body["work_item"]["node_id"], "second", "{name}");
        assert_eq!(
            body["work_item"]["lease_fence"],
            json!(fence.lock().unwrap().expect("the other request ran")),
            "{name}"
        );
        cleanup(path);
    }
}

/// A retry that saw the lease undecided does not go on once another has
/// taken it to gating: the move to gating is a compare-and-swap on the mark
/// the retry saw. Without it both decided the same item.
#[tokio::test]
async fn a_retry_with_a_stale_view_of_the_mark_stops() {
    for (name, backend, inner, hook, _, path) in hooked_backends().await {
        let state = make_state(backend.clone());
        register(&state, None, ir("wf-stale", Some("by_execution"), None)).await;
        let e = start(&state, None, "wf-stale").await;
        lease_undecided(&inner, &e, "first", "w1").await;
        let (i2, e2) = (inner.clone(), e.clone());
        hook.arm(
            |w| matches!(w, support::hooked::Write::LeasedItems),
            async move {
                // Another retry took it to gating, and is about to decide.
                let l = i2.leased_items(&[QUEUE], &by_this(&e2)).await.unwrap();
                assert!(i2
                    .swap_gate(
                        l[0].item.id,
                        l[0].item.lease_fence,
                        jamjet_state::GateMark::Undecided,
                        jamjet_state::GateMark::Gating
                    )
                    .await
                    .unwrap());
            },
        );
        let (_, body) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(
            body,
            json!({ "claimed": false, "reason": "not_ready" }),
            "{name}"
        );
        cleanup(path);
    }
}

/// A gate whose lease was swept as gating while it decided (it ran past the
/// lease) does not hand the item out: the move to released is a
/// compare-and-swap, and the lease is gone.
#[tokio::test]
async fn a_gate_whose_lease_was_swept_hands_nothing_out() {
    for (name, backend, inner, hook, expire, path) in hooked_backends().await {
        let state = make_state(backend.clone());
        let agent = register_agent(&state).await;
        let mut workflow = ir("wf-swept", Some("by_execution"), None);
        workflow["agent_ref"] = json!(agent);
        register(&state, None, workflow).await;
        let e = start(&state, None, "wf-swept").await;
        let (i2, e2, x2) = (inner.clone(), e.clone(), expire.clone());
        hook.arm(
            |w| matches!(w, support::hooked::Write::GetEvents(_)),
            async move {
                let l = i2.leased_items(&[QUEUE], &by_this(&e2)).await.unwrap();
                x2.lease(l[0].item.id).await;
                i2.reclaim_expired_leases().await.unwrap();
            },
        );
        let (_, body) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(
            body,
            json!({ "claimed": false, "reason": "not_ready" }),
            "{name}"
        );
        cleanup(path);
    }
}

/// A released lease re-attached by a heartbeat after an expiry stays
/// released under its new fence: the same worker claiming again gets that
/// lease back. It used to read as undecided and be decided again under a
/// third fence, while the worker was already running it.
#[tokio::test]
async fn a_reattached_lease_stays_released() {
    for (name, backend, inner, _hook, expire, path) in hooked_backends().await {
        let state = make_state(backend.clone());
        register(&state, None, ir("wf-reattach", Some("by_execution"), None)).await;
        let e = start(&state, None, "wf-reattach").await;
        let (_, first) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(first["claimed"], true, "{name}: {first}");
        let id = first["work_item"]["id"].as_str().unwrap().to_string();
        expire.lease(uuid::Uuid::parse_str(&id).unwrap()).await;
        inner.reclaim_expired_leases().await.unwrap();
        let (st, beat) = send(
            &state,
            "POST",
            &format!("/work-items/{id}/heartbeat"),
            None,
            json!({ "worker_id": "w1", "lease_fence": first["work_item"]["lease_fence"] }),
        )
        .await;
        assert_eq!(st, 200, "{name}: {beat}");
        assert_eq!(beat["reattached"], true, "{name}: {beat}");
        let (_, again) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(again["claimed"], true, "{name}: {again}");
        assert_eq!(
            again["work_item"]["lease_fence"], beat["lease_fence"],
            "{name}"
        );
        cleanup(path);
    }
}

/// An item of a node claimable only by its execution that lost its marker is
/// not handed out oldest-first. It is put back marked, so it does not sit at
/// the head of the queue for every poll, and the claim by execution gets it.
#[tokio::test]
async fn an_unmarked_item_of_a_by_execution_node_is_refused_oldest_first() {
    for (name, backend, path) in backends().await {
        let state = make_state(backend.clone());
        let workflow = ir("wf-unmarked", Some("by_execution"), None);
        register(&state, None, workflow.clone()).await;
        let e = start(&state, None, "wf-unmarked").await;
        // Take the start item out of the way.
        let (_, body) = claim(&state, None, "w", json!({ "execution_id": e })).await;
        assert_eq!(body["claimed"], true, "{name}: {body}");
        // `second`, enqueued without the marker, then a plain run's item.
        let wf: WorkflowIr = serde_json::from_value(workflow).unwrap();
        let mut payload = jamjet_scheduler::payload::build_work_item_payload(
            &wf,
            "second",
            wf.node("second").unwrap(),
            &serde_json::Map::new(),
            &wf.workflow_id,
            "0.1.0",
        );
        payload.as_object_mut().unwrap().remove("claim");
        let unmarked = uuid::Uuid::new_v4();
        backend
            .enqueue_work_item(WorkItem {
                id: unmarked,
                execution_id: execution_id(&e),
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
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        register(&state, None, ir("wf-behind", None, None)).await;
        let behind = start(&state, None, "wf-behind").await;

        // Three polls: whatever order a backend hands items out in, each of
        // the two pending items is looked at.
        let mut handed = Vec::new();
        for _ in 0..3 {
            let (_, body) = claim(&state, None, "oldest-first", json!({})).await;
            if body["claimed"] == true {
                handed.push(body["work_item"]["execution_id"].clone());
            }
        }
        assert_eq!(handed, vec![json!(behind)], "{name}");
        let item = backend.get_work_item(unmarked).await.unwrap().unwrap();
        assert_eq!(
            item.payload["claim"], "by_execution",
            "{name}: put back unmarked, it heads the queue for every poll"
        );
        assert!(item.worker_id.is_none(), "{name}");
        let (_, body) = claim(
            &state,
            None,
            "w",
            json!({ "execution_id": e, "node_id": "second" }),
        )
        .await;
        assert_eq!(body["claimed"], true, "{name}: {body}");
        assert_eq!(body["work_item"]["attempt"], json!(0), "{name}: {body}");
        cleanup(path);
    }
}

/// A heartbeat whose read of the item fails is a 503 the worker retries,
/// with `Retry-After`, not a 500 it takes for a lost lease.
#[tokio::test]
async fn a_heartbeat_that_cannot_read_the_item_is_unavailable() {
    for (name, _, inner, _, _, path) in hooked_backends().await {
        let hooked = Arc::new(support::hooked::HookedBackend::new(
            inner.clone(),
            Arc::new(Once::default()),
        ));
        let backend: Arc<dyn StateBackend> = hooked.clone();
        let state = make_state(backend);
        register(&state, None, ir("wf-beat", Some("by_execution"), None)).await;
        let e = start(&state, None, "wf-beat").await;
        let (_, first) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(first["claimed"], true, "{name}: {first}");
        let id = first["work_item"]["id"].as_str().unwrap().to_string();
        hooked
            .fail_item_reads
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (st, retry_after, body) = send_with_headers(
            &state,
            "POST",
            &format!("/work-items/{id}/heartbeat"),
            None,
            json!({ "worker_id": "w1", "lease_fence": first["work_item"]["lease_fence"] }),
        )
        .await;
        assert_eq!(st, 503, "{name}: {body}");
        assert_eq!(retry_after.as_deref(), Some("1"), "{name}");
        cleanup(path);
    }
}

// ── Third review: a gate that dies, a committed reattach, the release ────────

/// A gate that panics on every try ends in the dead-letter queue with the
/// node failed, as any item does that dies three times. Its lease used to
/// stay marked as gating, which the sweep requeued at attempt 0 with nothing
/// recorded, so the run stayed `Running` forever.
#[tokio::test]
async fn a_gate_that_panics_every_time_is_dead_lettered() {
    for (name, _, inner, _, expire, path) in hooked_backends().await {
        let hooked = Arc::new(support::hooked::HookedBackend::new(
            inner.clone(),
            Arc::new(Once::default()),
        ));
        let backend: Arc<dyn StateBackend> = hooked.clone();
        let state = make_state(backend.clone());
        register(&state, None, ir("wf-poison", Some("by_execution"), None)).await;
        let e = start(&state, None, "wf-poison").await;
        let scheduler = jamjet_scheduler::Scheduler::new(inner.clone());
        for round in 0..3 {
            hooked
                .panic_workflow_reads
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let (st, body) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
            hooked
                .panic_workflow_reads
                .store(false, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(st, 500, "{name} round {round}: {body}");
            let leased = inner.leased_items(&[QUEUE], &by_this(&e)).await.unwrap();
            assert_eq!(leased.len(), 1, "{name} round {round}");
            assert_eq!(
                leased[0].gate,
                jamjet_state::GateMark::Undecided,
                "{name} round {round}: the dead gate's mark was left"
            );
            expire.lease(leased[0].item.id).await;
            scheduler.tick().await.unwrap();
            expire.ready(leased[0].item.id).await;
        }
        let kinds = event_kinds(&inner, &e).await;
        assert!(
            kinds.iter().any(|k| matches!(
                k,
                EventKind::NodeFailed { node_id, retryable: false, .. } if node_id == "first"
            )),
            "{name}: never dead-lettered: {kinds:?}"
        );
        assert!(
            !inner.has_claimable(&[QUEUE], &by_this(&e)).await.unwrap(),
            "{name}: still claimable"
        );
        cleanup(path);
    }
}

/// A heartbeat that re-attached a lease is answered with the new fence even
/// when the cancel read after it fails. A 503 there could not be retried: the
/// retry carries the old fence, which the reattach already replaced, so it
/// got a 409 and the worker dropped a live lease.
#[tokio::test]
async fn a_committed_reattach_is_answered_although_the_cancel_read_fails() {
    for (name, _, inner, _, expire, path) in hooked_backends().await {
        let hook = Arc::new(Once::default());
        let hooked = Arc::new(support::hooked::HookedBackend::new(
            inner.clone(),
            hook.clone(),
        ));
        let backend: Arc<dyn StateBackend> = hooked.clone();
        let state = make_state(backend);
        register(&state, None, ir("wf-reread", Some("by_execution"), None)).await;
        let e = start(&state, None, "wf-reread").await;
        let (_, first) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(first["claimed"], true, "{name}: {first}");
        let id = first["work_item"]["id"].as_str().unwrap().to_string();
        expire.lease(uuid::Uuid::parse_str(&id).unwrap()).await;
        inner.reclaim_expired_leases().await.unwrap();

        let fail = hooked.fail_item_reads.clone();
        hook.arm(
            |w| matches!(w, support::hooked::Write::HeartbeatLease),
            async move { fail.store(true, std::sync::atomic::Ordering::SeqCst) },
        );
        let (st, beat) = send(
            &state,
            "POST",
            &format!("/work-items/{id}/heartbeat"),
            None,
            json!({ "worker_id": "w1", "lease_fence": first["work_item"]["lease_fence"] }),
        )
        .await;
        hooked
            .fail_item_reads
            .store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(st, 200, "{name}: {beat}");
        assert_eq!(beat["reattached"], true, "{name}: {beat}");
        assert_eq!(beat["cancel_requested"], false, "{name}: {beat}");
        assert_ne!(beat["lease_fence"], first["work_item"]["lease_fence"]);
        // The new fence is the one that holds the lease.
        let (st, again) = send(
            &state,
            "POST",
            &format!("/work-items/{id}/heartbeat"),
            None,
            json!({ "worker_id": "w1", "lease_fence": beat["lease_fence"] }),
        )
        .await;
        assert_eq!(st, 200, "{name}: {again}");
        cleanup(path);
    }
}

/// A gate that outlives its lease does not hand the lease out: the sweep is
/// about to take it back, and the node would run a second time.
#[tokio::test]
async fn a_gate_that_outlives_its_lease_hands_nothing_out() {
    for (name, backend, inner, hook, expire, path) in hooked_backends().await {
        let state = make_state(backend.clone());
        // No agent: no step to admit, whose own lease check would stop it
        // first. The release alone must refuse.
        register(&state, None, ir("wf-late", Some("by_execution"), None)).await;
        let e = start(&state, None, "wf-late").await;
        let (i2, e2, x2) = (inner.clone(), e.clone(), expire.clone());
        // The lease runs out between the claim and the release.
        hook.arm(
            |w| matches!(w, support::hooked::Write::Claimed(_)),
            async move {
                let l = i2.leased_items(&[QUEUE], &by_this(&e2)).await.unwrap();
                x2.lease(l[0].item.id).await;
            },
        );
        let (_, body) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(
            body,
            json!({ "claimed": false, "reason": "not_ready" }),
            "{name}"
        );
        cleanup(path);
    }
}

/// The release renews the lease for its whole length: the worker gets the
/// lease it was granted, not what the gate left of it.
#[tokio::test]
async fn a_release_renews_the_lease() {
    for (name, backend, inner, hook, expire, path) in hooked_backends().await {
        let state = make_state(backend.clone());
        let agent = register_agent(&state).await;
        let mut workflow = ir("wf-renew", Some("by_execution"), None);
        workflow["agent_ref"] = json!(agent);
        register(&state, None, workflow).await;
        let e = start(&state, None, "wf-renew").await;
        let (i2, e2, x2) = (inner.clone(), e.clone(), expire.clone());
        hook.arm(
            |w| matches!(w, support::hooked::Write::GetEvents(_)),
            async move {
                // The gate took all but two seconds of the lease.
                let l = i2.leased_items(&[QUEUE], &by_this(&e2)).await.unwrap();
                x2.lease_until(
                    l[0].item.id,
                    chrono::Utc::now() + chrono::Duration::seconds(2),
                )
                .await;
            },
        );
        let (_, body) = claim(
            &state,
            None,
            "w1",
            json!({ "execution_id": e, "lease_secs": 60 }),
        )
        .await;
        assert_eq!(body["claimed"], true, "{name}: {body}");
        let id = uuid::Uuid::parse_str(body["work_item"]["id"].as_str().unwrap()).unwrap();
        let left = inner
            .get_work_item(id)
            .await
            .unwrap()
            .unwrap()
            .lease_expires_at
            .unwrap()
            - chrono::Utc::now();
        assert!(
            left > chrono::Duration::seconds(30),
            "{name}: {left:?} left of the lease"
        );
        cleanup(path);
    }
}

/// A named retry from the same worker during an oldest-first gate re-hands
/// the lease under a new fence; the oldest-first claim then hands out
/// nothing. Its release was a blind mark, and the item went out twice.
#[tokio::test]
async fn an_oldest_first_gate_beside_a_named_retry_hands_out_once() {
    for (name, backend, inner, hook, _, path) in hooked_backends().await {
        let state = make_state(backend.clone());
        register(&state, None, ir("wf-both", None, None)).await;
        let e = start(&state, None, "wf-both").await;
        let answer = Arc::new(std::sync::Mutex::new(None));
        let (st2, e2, a2) = (state.clone(), e.clone(), answer.clone());
        // Right after the oldest-first claim leased the item (its check that
        // the run is live), the same worker claims it by execution.
        hook.arm(
            |w| matches!(w, support::hooked::Write::GetExecution),
            async move {
                let (_, body) = claim(&st2, None, "w1", json!({ "execution_id": e2 })).await;
                *a2.lock().unwrap() = Some(body);
            },
        );
        let (_, oldest) = claim(&state, None, "w1", json!({})).await;
        let named = answer.lock().unwrap().clone().expect("the named retry ran");
        assert_eq!(named["claimed"], true, "{name}: {named}");
        assert_eq!(
            oldest,
            json!({ "claimed": false }),
            "{name}: handed out twice"
        );
        let leased = inner.leased_items(&[QUEUE], &by_this(&e)).await.unwrap();
        assert_eq!(
            json!(leased[0].item.lease_fence),
            named["work_item"]["lease_fence"],
            "{name}"
        );
        cleanup(path);
    }
}

/// A gate whose lease was swept while it decided does not spend the agent's
/// step on an item it never hands out: with one step allowed, the next claim
/// still gets the item.
#[tokio::test]
async fn a_swept_gate_does_not_spend_a_step() {
    for (name, backend, inner, hook, expire, path) in hooked_backends().await {
        let state = make_state(backend.clone());
        let agent = "jamjet://acme/one-step";
        let (st, body) = post(
            &state,
            "/agents",
            json!({
                "id": agent, "uri": agent, "name": "one-step", "description": "",
                "version": "1.0.0",
                "capabilities": { "skills": [], "protocols": [], "tools_provided": [], "tools_consumed": [] },
                "autonomy": "fully_autonomous",
                "constraints": { "max_iterations": 1 },
                "auth": { "type": "none" }
            }),
        )
        .await;
        assert!(st.is_success(), "{name}: {st} {body}");
        let mut workflow = ir("wf-onestep", Some("by_execution"), None);
        workflow["agent_ref"] = json!(agent);
        register(&state, None, workflow).await;
        let e = start(&state, None, "wf-onestep").await;
        let (i2, e2, x2) = (inner.clone(), e.clone(), expire.clone());
        hook.arm(
            |w| matches!(w, support::hooked::Write::GetEvents(_)),
            async move {
                let l = i2.leased_items(&[QUEUE], &by_this(&e2)).await.unwrap();
                x2.lease(l[0].item.id).await;
                i2.reclaim_expired_leases().await.unwrap();
                x2.ready(l[0].item.id).await;
            },
        );
        let (_, body) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(body["claimed"], false, "{name}: {body}");
        let (_, again) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(
            again["claimed"], true,
            "{name}: the step was spent: {again}"
        );
        let kinds = event_kinds(&inner, &e).await;
        assert_eq!(
            count(&kinds, |k| matches!(k, EventKind::AgentStepAdmitted { .. })),
            1,
            "{name}: {kinds:?}"
        );
        cleanup(path);
    }
}

/// An item withheld as not the engine's (its payload names another workflow)
/// is answered `refused`, on record, the same on every retry. It answered
/// `blocked`, though no policy refused the node, and then `not_ready`.
#[tokio::test]
async fn a_stray_item_is_refused_the_same_on_every_retry() {
    for (name, backend, path) in backends().await {
        let state = make_state(backend.clone());
        register(&state, None, ir("wf-stray", Some("by_execution"), None)).await;
        let e = start(&state, None, "wf-stray").await;
        let (_, body) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(body["claimed"], true, "{name}: {body}");
        backend
            .enqueue_work_item(WorkItem {
                id: uuid::Uuid::new_v4(),
                execution_id: execution_id(&e),
                node_id: "second".into(),
                queue_type: QUEUE.into(),
                payload: json!({
                    "workflow_id": "some-other-workflow", "workflow_version": "0.1.0",
                    "node_id": "second", "claim": "by_execution"
                }),
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
        for worker in ["w1", "w1", "w2"] {
            let (_, body) = claim(
                &state,
                None,
                worker,
                json!({ "execution_id": e, "node_id": "second" }),
            )
            .await;
            assert_eq!(
                body,
                json!({ "claimed": false, "reason": "refused" }),
                "{name}: {worker}"
            );
        }
        let kinds = event_kinds(&backend, &e).await;
        assert!(
            !kinds
                .iter()
                .any(|k| matches!(k, EventKind::NodeFailed { node_id, .. } if node_id == "second")),
            "{name}: the node was failed over a stray item"
        );
        cleanup(path);
    }
}

/// A claim with no `node_id` answers for the nodes open now, not for every
/// node the run ever scheduled: a node refused earlier, that the run has
/// moved past, made it say `blocked` while the run waited on the next node.
#[tokio::test]
async fn a_reason_is_for_the_nodes_open_now() {
    for (name, backend, path) in backends().await {
        let state = make_state(backend.clone());
        register(&state, None, ir("wf-past", Some("by_execution"), None)).await;
        let e = start(&state, None, "wf-past").await;
        let (_, body) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(body["claimed"], true, "{name}: {body}");
        let id = uuid::Uuid::parse_str(body["work_item"]["id"].as_str().unwrap()).unwrap();
        backend
            .withdraw_work_item_fenced(id, body["work_item"]["lease_fence"].as_i64().unwrap())
            .await
            .unwrap();
        for kind in [
            EventKind::NodeFailed {
                node_id: "first".into(),
                error: "policy blocked: earlier".into(),
                attempt: 0,
                retryable: false,
            },
            EventKind::NodeScheduled {
                node_id: "second".into(),
                queue_type: QUEUE.into(),
            },
        ] {
            backend
                .append_event(jamjet_state::Event::new(execution_id(&e), 0, kind))
                .await
                .unwrap();
        }
        let (_, body) = claim(&state, None, "w1", json!({ "execution_id": e })).await;
        assert_eq!(
            body,
            json!({ "claimed": false, "reason": "not_ready" }),
            "{name}"
        );
        // Asked for the refused node by name, it still says so.
        let (_, body) = claim(
            &state,
            None,
            "w1",
            json!({ "execution_id": e, "node_id": "first" }),
        )
        .await;
        assert_eq!(
            body,
            json!({ "claimed": false, "reason": "blocked" }),
            "{name}"
        );
        cleanup(path);
    }
}

/// A claim answers why from the log the initiator check already read: one
/// read of the run's log, not two.
#[tokio::test]
async fn a_claim_reads_the_log_once_to_say_why() {
    struct Count(std::sync::atomic::AtomicUsize);
    #[async_trait::async_trait]
    impl support::hooked::AfterWrite for Count {
        async fn after(&self, write: support::hooked::Write) {
            if let support::hooked::Write::GetEvents(_) = write {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
    let (url, path) = sqlite_url();
    let sqlite = Arc::new(SqliteBackend::open(&url).await.unwrap());
    let (token, _) = sqlite.create_token("reader", "developer").await.unwrap();
    let count = Arc::new(Count(std::sync::atomic::AtomicUsize::new(0)));
    let backend: Arc<dyn StateBackend> =
        Arc::new(support::hooked::HookedBackend::new(sqlite, count.clone()));
    let state = make_state(backend);
    register(
        &state,
        Some(&token),
        ir(
            "wf-once",
            Some("by_execution"),
            Some(policy(&["first"], &[])),
        ),
    )
    .await;
    let e = start(&state, Some(&token), "wf-once").await;
    let (_, body) = claim(&state, Some(&token), "w1", json!({ "execution_id": e })).await;
    assert_eq!(body["reason"], "blocked", "{body}");
    count.0.store(0, std::sync::atomic::Ordering::SeqCst);
    let (_, body) = claim(&state, Some(&token), "w1", json!({ "execution_id": e })).await;
    assert_eq!(body, json!({ "claimed": false, "reason": "blocked" }));
    assert_eq!(count.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    let _ = std::fs::remove_file(path);
}

/// An item of a node claimable only by its execution whose payload is not an
/// object cannot carry the marker. The oldest-first claim fails it: put back,
/// it headed the queue for every poll.
#[tokio::test]
async fn an_unmarkable_item_is_failed_not_put_back() {
    for (name, backend, path) in backends().await {
        let state = make_state(backend.clone());
        register(&state, None, ir("wf-scalar", Some("by_execution"), None)).await;
        let e = start(&state, None, "wf-scalar").await;
        let (_, body) = claim(&state, None, "w", json!({ "execution_id": e })).await;
        assert_eq!(body["claimed"], true, "{name}: {body}");
        backend
            .enqueue_work_item(WorkItem {
                id: uuid::Uuid::new_v4(),
                execution_id: execution_id(&e),
                node_id: "second".into(),
                queue_type: QUEUE.into(),
                payload: json!("not an object"),
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
        let (_, body) = claim(&state, None, "oldest-first", json!({})).await;
        assert_eq!(body, json!({ "claimed": false }), "{name}");
        let second = ClaimFilter {
            execution_id: Some(execution_id(&e)),
            node_id: Some("second".into()),
        };
        assert!(
            !backend.has_claimable(&[QUEUE], &second).await.unwrap(),
            "{name}: put back"
        );
        let kinds = event_kinds(&backend, &e).await;
        assert!(
            kinds.iter().any(|k| matches!(
                k,
                EventKind::NodeFailed { node_id, retryable: false, .. } if node_id == "second"
            )),
            "{name}: {kinds:?}"
        );
        cleanup(path);
    }
}
