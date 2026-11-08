//! Follow-ups from the re-review of #169 (reviewer's probes, round 2, kept as
//! regressions).
//!
//! - Y: a node with no incoming edge ran as a root when the run started,
//!   before the approval it was meant to follow (the YAML compiler drew no
//!   edge to a `branch` node's targets). Registration now refuses it.
//! - E: any later step could write the approval record a condition reads, and
//!   so route as if approved after an expiry. The record is engine-owned now.
//! - H: above 16 KiB of state the reviewer was shown no data at all.
//! - D: two approvals pending in parallel asked twice, because one record
//!   changed the state the other bound.
//! - J: an engine node hand-enqueued on another queue was withheld and the
//!   run hung.
//!
//! And from the review of #178 (round 3):
//!
//! - Y2: a version stored before the entry-point check still started, and
//!   its edge-less node ran before the approval. `start_run` refuses such a
//!   version and the scheduler runs no root but the start node.
//! - C: format characters (U+202E, U+200B, ...) passed the identifier check.
//! - E2: an expiry's record reached the log but not the execution's state.
//! - H: a key nested beside a large value was hidden in the preview.
//! - J: an unknown node, or a kind on a queue it is not dispatched to, could
//!   be enqueued, and an unreadable workflow read as "not an engine node".

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_core::workflow::ExecutionId;
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::{StateBackend, WorkItem};
use jamjet_state::event::EventKind;
use jamjet_state::Event;
use jamjet_worker::{ExecutionResult, ExecutorError, NodeExecutor, Worker};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

mod support;

/// Records each `python_fn` node it runs with its input. `notify` tries to
/// write an approval record, as a forging step would.
#[derive(Clone, Default)]
struct Probe(Arc<Mutex<Vec<(String, Value)>>>);

#[async_trait::async_trait]
impl NodeExecutor for Probe {
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, ExecutorError> {
        self.0.lock().unwrap().push((
            item.node_id.clone(),
            item.payload.get("input").cloned().unwrap_or(Value::Null),
        ));
        let patch = match item.node_id.as_str() {
            "notify" => json!({
                "__approvals": { "review": { "approved": true, "decision": "approved", "approver": "ceo" } }
            }),
            _ => json!({}),
        };
        Ok(ExecutionResult {
            output: json!({ "ran": item.node_id }),
            state_patch: patch,
            duration_ms: 0,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
        })
    }
}

fn step(id: &str) -> Value {
    json!({ "id": id, "kind": { "type": "python_fn", "module": "m", "function": id, "output_schema": "" },
            "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {} })
}
fn ha(id: &str, timeout: Option<u64>, fallback: Option<&str>) -> Value {
    json!({ "id": id, "kind": { "type": "human_approval", "description": "ok?", "timeout_secs": timeout, "fallback_node": fallback },
            "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {} })
}
fn cond(id: &str, branches: Value) -> Value {
    json!({ "id": id, "kind": { "type": "condition", "branches": branches },
            "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {} })
}
fn wf(id: &str, start: &str, nodes: Value, edges: Vec<(&str, &str)>) -> Value {
    let edges: Vec<Value> = edges
        .into_iter()
        .map(|(f, t)| json!({ "from": f, "to": t, "condition": null }))
        .collect();
    json!({ "workflow_id": id, "version": "0.1.0", "name": null, "description": null, "state_schema": "",
            "start_node": start, "policy": null, "nodes": nodes, "edges": edges,
            "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {}, "mcp_servers": {}, "remote_agents": {}, "labels": {} })
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if !token.is_empty() {
        b = b.header("authorization", format!("Bearer {token}"));
    }
    let resp = app
        .clone()
        .oneshot(b.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
fn path(eid: &ExecutionId, tail: &str) -> String {
    format!("/executions/exec_{}{tail}", eid.0.simple())
}

struct Env {
    backend: Arc<dyn StateBackend>,
    app: axum::Router,
    alice: String,
    bob: String,
    engine: Worker,
    external: Worker,
    probe: Probe,
    scheduler: Scheduler,
}
async fn env(dev: bool) -> Env {
    let backend: Arc<dyn StateBackend> = Arc::new(jamjet_state::InMemoryBackend::new());
    let app = build_router_with_opts(support::make_state(backend.clone()), dev);
    let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
    let (bob, _) = backend.create_token("bob", "reviewer").await.unwrap();
    let probe = Probe::default();
    Env {
        engine: jamjet_worker::server_pool(
            backend.clone(),
            Arc::new(jamjet_models::ModelRegistry::new()),
        )
        .build_worker("engine".into(), vec!["general".into()]),
        external: Worker::new("py".into(), backend.clone(), vec!["python_tool".into()])
            .register_executor("python_fn", Arc::new(probe.clone())),
        scheduler: Scheduler::new(backend.clone()),
        backend,
        app,
        alice,
        bob,
        probe,
    }
}
fn eid_of(body: &Value) -> ExecutionId {
    let id = body["execution_id"].as_str().unwrap();
    ExecutionId(uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap())
}
async fn start(env: &Env, ir: Value, input: Value) -> ExecutionId {
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        json!({ "ir": ir.clone() }),
    )
    .await;
    assert!(st.is_success(), "register: {st} {body}");
    let (st, body) = call(
        &env.app,
        "POST",
        "/executions",
        &env.alice,
        json!({ "workflow_id": ir["workflow_id"], "workflow_version": "0.1.0", "input": input }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    eid_of(&body)
}
async fn settle(env: &Env) {
    for _ in 0..10 {
        let _ = env.engine.poll_and_execute().await;
        let _ = env.external.poll_and_execute().await;
        env.scheduler.tick().await.unwrap();
    }
}
fn reqs(evs: &[Event], node: &str) -> Vec<Value> {
    evs.iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolApprovalRequired {
                node_id, context, ..
            } if node_id == node => Some(context.clone()),
            _ => None,
        })
        .collect()
}
fn final_state(evs: &[Event]) -> Option<Value> {
    evs.iter().find_map(|e| match &e.kind {
        EventKind::WorkflowCompleted { final_state } => Some(final_state.clone()),
        _ => None,
    })
}
fn ran(env: &Env) -> Vec<String> {
    env.probe
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|(n, _)| n.clone())
        .collect()
}
fn input_of(env: &Env, node: &str) -> Option<Value> {
    env.probe
        .0
        .lock()
        .unwrap()
        .iter()
        .find(|(n, _)| n == node)
        .map(|(_, i)| i.clone())
}
async fn approve(env: &Env, eid: &ExecutionId, node: &str, digest: &Value) -> StatusCode {
    let (st, _) = call(
        &env.app,
        "POST",
        &path(eid, "/approve"),
        &env.bob,
        json!({ "decision": "approved", "node_id": node, "action_digest": digest }),
    )
    .await;
    st
}

/// PROBE Y: the shape the YAML compiler produced for `review -> route
/// (branch: approved -> pay, default end)`: no edge into `pay`, so it ran
/// when the run started. Refused, naming the node.
#[tokio::test]
async fn a_node_with_no_incoming_edge_is_refused() {
    let env = env(false).await;
    let ir = wf(
        "wf-y",
        "review",
        json!({ "review": ha("review", None, None),
            "route": cond("route", json!([ { "condition": "state.__approvals.review.approved == true", "target": "pay" },
                                           { "condition": null, "target": "end" } ])),
            "pay": step("pay") }),
        vec![("review", "route"), ("route", "end"), ("pay", "end")],
    );
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        json!({ "ir": ir }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    let msg = body.to_string();
    assert!(
        msg.contains("'pay'") && msg.contains("no incoming edge"),
        "{msg}"
    );
}

/// A run of `ir` started the way `start_run` started one before it checked
/// entry points: the execution, `WorkflowStarted`, and the start node's
/// `NodeScheduled` and work item.
async fn started_before_the_check(env: &Env, ir: &Value) -> ExecutionId {
    let typed: jamjet_ir::WorkflowIr = serde_json::from_value(ir.clone()).unwrap();
    let start = typed.start_node.clone();
    let start_def = &typed.nodes[&start];
    let now = chrono::Utc::now();
    let eid = ExecutionId::new();
    let execution = jamjet_core::workflow::WorkflowExecution {
        execution_id: eid.clone(),
        workflow_id: typed.workflow_id.clone(),
        workflow_version: typed.version.clone(),
        status: jamjet_core::workflow::WorkflowStatus::Running,
        initial_input: json!({}),
        current_state: json!({}),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    };
    let queue = start_def.kind.queue_type().as_str().to_string();
    let item = WorkItem {
        id: uuid::Uuid::new_v4(),
        execution_id: eid.clone(),
        node_id: start.clone(),
        queue_type: queue.clone(),
        payload: jamjet_scheduler::build_work_item_payload(
            &typed,
            &start,
            start_def,
            &serde_json::Map::new(),
            &typed.workflow_id,
            &typed.version,
        ),
        attempt: 0,
        max_attempts: 3,
        created_at: now,
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: jamjet_state::DEFAULT_TENANT.to_string(),
    };
    env.backend
        .start_execution_atomic(
            execution,
            EventKind::WorkflowStarted {
                workflow_id: typed.workflow_id.clone(),
                workflow_version: typed.version.clone(),
                initial_input: json!({}),
                initiated_by: Some("token:alice".into()),
            },
            EventKind::NodeScheduled {
                node_id: start,
                queue_type: queue,
            },
            item,
        )
        .await
        .unwrap();
    eid
}

/// PROBE Y2: a version stored before registration checked entry points (or
/// by any other path) with the Y shape is not started: `POST /executions`
/// refuses it, naming the node. A run of it already in flight never runs
/// that node either: `pay` used to run before the approval was decided.
#[tokio::test]
async fn a_stored_version_with_a_node_nothing_leads_into_does_not_run_it() {
    let env = env(false).await;
    let ir = wf(
        "wf-y2",
        "review",
        json!({ "review": ha("review", None, None),
            "route": cond("route", json!([ { "condition": "state.__approvals.review.approved == true", "target": "pay" },
                                           { "condition": null, "target": "end" } ])),
            "pay": step("pay") }),
        vec![("review", "route"), ("route", "end"), ("pay", "end")],
    );
    support::store_ir(env.backend.as_ref(), &ir).await;
    let (st, body) = call(
        &env.app,
        "POST",
        "/executions",
        &env.alice,
        json!({ "workflow_id": "wf-y2", "workflow_version": "0.1.0", "input": {} }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    let msg = body.to_string();
    assert!(
        msg.contains("'pay'") && msg.contains("no incoming edge"),
        "{msg}"
    );
    assert!(
        env.backend
            .list_executions(None, 10, 0)
            .await
            .unwrap()
            .is_empty(),
        "nothing was started"
    );

    let eid = started_before_the_check(&env, &ir).await;
    settle(&env).await;
    assert!(
        ran(&env).is_empty(),
        "ran before any decision: {:?}",
        ran(&env)
    );
    let evs = env.backend.get_events(&eid).await.unwrap();
    let requests = reqs(&evs, "review");
    assert_eq!(requests.len(), 1, "the approval is asked");
    assert!(
        evs.iter().any(|e| matches!(&e.kind,
            EventKind::NodeSkipped { node_id, .. } if node_id == "pay")),
        "{evs:#?}"
    );
    assert_eq!(
        approve(&env, &eid, "review", &requests[0]["action_digest"]).await,
        StatusCode::OK
    );
    settle(&env).await;
    let evs = env.backend.get_events(&eid).await.unwrap();
    assert!(ran(&env).is_empty(), "ran: {:?}", ran(&env));
    assert!(final_state(&evs).is_some(), "the run completes: {evs:#?}");
}

/// Nodes that only reach each other never run; refused, naming both. A node
/// entered only as a `human_approval` fallback is fine.
#[tokio::test]
async fn unreachable_nodes_are_refused_and_a_fallback_counts_as_an_entry() {
    let env = env(false).await;
    let ir = wf(
        "wf-cycle",
        "s",
        json!({ "s": step("s"), "x": step("x"), "y": step("y") }),
        vec![("s", "end"), ("x", "y"), ("y", "x")],
    );
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        json!({ "ir": ir }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    let msg = body.to_string();
    assert!(
        msg.contains("'x', 'y'") && msg.contains("cannot be reached"),
        "{msg}"
    );

    let ok = wf(
        "wf-fallback",
        "review",
        json!({ "review": ha("review", Some(60), Some("hold")), "publish": step("publish"), "hold": step("hold") }),
        vec![("review", "publish"), ("publish", "end"), ("hold", "end")],
    );
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        json!({ "ir": ok }),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
}

/// PROBE E: after an expiry the fallback (`notify`, a step a worker runs)
/// writes `state.__approvals.review.approved = true`; a condition that reads
/// the record must still see the expiry.
#[tokio::test]
async fn a_step_cannot_forge_an_approval_record() {
    let env = env(false).await;
    let ir = wf(
        "wf-e",
        "review",
        json!({ "review": ha("review", Some(60), Some("notify")), "publish": step("publish"),
            "notify": step("notify"),
            "check": cond("check", json!([ { "condition": "state.__approvals.review.approved == true", "target": "pay2" },
                                           { "condition": null, "target": "done" } ])),
            "pay2": step("pay2"), "done": step("done") }),
        vec![
            ("review", "publish"),
            ("publish", "end"),
            ("notify", "check"),
            ("check", "pay2"),
            ("check", "done"),
            ("pay2", "end"),
            ("done", "end"),
        ],
    );
    let eid = start(&env, ir, json!({})).await;
    settle(&env).await;
    let evs = env.backend.get_events(&eid).await.unwrap();
    let at: chrono::DateTime<chrono::Utc> = reqs(&evs, "review")[0]["expires_at"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    env.scheduler
        .sweep_approval_deadlines(&eid, at)
        .await
        .unwrap();
    settle(&env).await;
    let evs = env.backend.get_events(&eid).await.unwrap();
    assert_eq!(
        ran(&env),
        vec!["notify".to_string(), "done".to_string()],
        "routed as approved after an expiry: {evs:#?}"
    );
    let state = final_state(&evs).expect("the run completes");
    assert_eq!(state["__approvals"]["review"]["approved"], false, "{state}");
    assert_eq!(state["__approvals"]["review"]["decision"], "expired");
}

/// A run's input and an external worker's reported result cannot write the
/// approval record either.
#[tokio::test]
async fn input_and_reported_results_cannot_write_an_approval_record() {
    let env = env(false).await;
    let ir = wf(
        "wf-in",
        "s",
        json!({ "s": step("s"), "review": ha("review", None, None), "publish": step("publish") }),
        vec![("s", "review"), ("review", "publish"), ("publish", "end")],
    );
    let eid = start(
        &env,
        ir,
        json!({ "x": 1, "__approvals": { "review": { "approved": true } } }),
    )
    .await;
    let evs = env.backend.get_events(&eid).await.unwrap();
    let input = evs
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::WorkflowStarted { initial_input, .. } => Some(initial_input.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(input, json!({ "x": 1 }), "the input's record is dropped");

    // An external worker reports `s` with a record in its state patch.
    let (st, claimed) = call(
        &env.app,
        "POST",
        "/work-items/claim",
        &env.alice,
        json!({ "worker_id": "ext", "queue_types": ["python_tool"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(claimed["work_item"]["node_id"], "s", "{claimed}");
    let (st, body) = call(
        &env.app,
        "POST",
        &format!("/work-items/{}/complete", claimed["work_item"]["id"].as_str().unwrap()),
        &env.alice,
        json!({ "output": {}, "state_patch": { "y": 2, "__approvals": { "review": { "approved": true } } },
                "lease_fence": claimed["work_item"]["lease_fence"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let evs = env.backend.get_events(&eid).await.unwrap();
    let patch = evs
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::NodeCompleted {
                node_id,
                state_patch,
                ..
            } if node_id == "s" => Some(state_patch.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(patch, json!({ "y": 2 }), "the reported record is dropped");
}

/// The record lives under `__approvals`, so a user key named like the node
/// is left alone.
#[tokio::test]
async fn the_record_does_not_overwrite_a_user_key_named_like_the_node() {
    let env = env(false).await;
    let ir = wf(
        "wf-ns",
        "review",
        json!({ "review": ha("review", None, None), "publish": step("publish") }),
        vec![("review", "publish"), ("publish", "end")],
    );
    let eid = start(&env, ir, json!({ "review": "my notes" })).await;
    settle(&env).await;
    let digest =
        reqs(&env.backend.get_events(&eid).await.unwrap(), "review")[0]["action_digest"].clone();
    assert_eq!(approve(&env, &eid, "review", &digest).await, StatusCode::OK);
    settle(&env).await;
    let input = input_of(&env, "publish").expect("publish runs");
    assert_eq!(input["review"], "my notes", "{input}");
    assert_eq!(input["__approvals"]["review"]["approved"], true, "{input}");
}

/// PROBE H: a large state still shows the reviewer every key, nested ones
/// beside a large value included.
#[tokio::test]
async fn a_large_state_still_shows_every_key_to_the_reviewer() {
    let env = env(false).await;
    let ir = wf(
        "wf-h",
        "review",
        json!({ "review": ha("review", None, None), "publish": step("publish") }),
        vec![("review", "publish"), ("publish", "end")],
    );
    let eid = start(
        &env,
        ir,
        json!({ "notes": "x".repeat(20_000), "zz_payee": "attacker",
                "order": { "memo": "y".repeat(5000), "payee": "attacker2" } }),
    )
    .await;
    settle(&env).await;
    let ctx = reqs(&env.backend.get_events(&eid).await.unwrap(), "review")[0].clone();
    let basis = &ctx["action_basis"];
    assert_eq!(basis["payload_truncated"], true);
    assert_eq!(basis["payload"]["input"]["zz_payee"], "attacker", "{basis}");
    assert_eq!(
        basis["payload"]["input"]["order"]["payee"], "attacker2",
        "{basis}"
    );
    assert_eq!(
        basis["payload"]["input"]["notes"]["preview_truncated"],
        true
    );
    let (_, listing) = call(
        &env.app,
        "GET",
        &path(&eid, "/approvals"),
        &env.bob,
        Value::Null,
    )
    .await;
    assert!(listing.to_string().contains("attacker"), "{listing}");
}

/// PROBE D: two approvals pending side by side are each asked once: one
/// completing does not move the state the other is bound to.
#[tokio::test]
async fn parallel_approvals_are_each_asked_once() {
    let env = env(false).await;
    let ir = wf(
        "wf-d",
        "s",
        json!({ "s": step("s"), "a": ha("a", None, None), "b": ha("b", None, None),
            "pa": step("pa"), "pb": step("pb") }),
        vec![
            ("s", "a"),
            ("s", "b"),
            ("a", "pa"),
            ("b", "pb"),
            ("pa", "end"),
            ("pb", "end"),
        ],
    );
    let eid = start(&env, ir, json!({ "x": 1 })).await;
    settle(&env).await;
    let evs = env.backend.get_events(&eid).await.unwrap();
    let (ra, rb) = (reqs(&evs, "a"), reqs(&evs, "b"));
    assert_eq!((ra.len(), rb.len()), (1, 1));
    assert_eq!(
        approve(&env, &eid, "a", &ra[0]["action_digest"]).await,
        StatusCode::OK
    );
    settle(&env).await;
    assert_eq!(
        approve(&env, &eid, "b", &rb[0]["action_digest"]).await,
        StatusCode::OK
    );
    settle(&env).await;
    let evs = env.backend.get_events(&eid).await.unwrap();
    assert_eq!(reqs(&evs, "b").len(), 1, "b was asked again: {evs:#?}");
    let state = final_state(&evs).expect("the run completes");
    assert_eq!(state["__approvals"]["a"]["approved"], true, "{state}");
    assert_eq!(state["__approvals"]["b"]["approved"], true, "{state}");
}

/// PROBE J: an engine node cannot be hand-enqueued; the run is not left
/// waiting on an item no one will run.
#[tokio::test]
async fn an_engine_node_cannot_be_hand_enqueued() {
    let env = env(true).await;
    let ir = wf(
        "wf-j",
        "s",
        json!({ "s": step("s"), "review": ha("review", None, None), "publish": step("publish") }),
        vec![("s", "review"), ("review", "publish"), ("publish", "end")],
    );
    let (st, body) = call(&env.app, "POST", "/workflows", "", json!({ "ir": ir })).await;
    assert!(st.is_success(), "{body}");
    let (st, body) = call(
        &env.app,
        "POST",
        "/executions",
        "",
        json!({ "workflow_id": "wf-j", "workflow_version": "0.1.0", "input": {} }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let id = body["execution_id"].as_str().unwrap().to_string();
    let eid = eid_of(&body);
    let (st, body) = call(
        &env.app,
        "POST",
        "/work-items",
        "",
        json!({ "execution_id": id, "node_id": "review", "queue_type": "tool",
                "payload": { "workflow_id": "wf-j", "workflow_version": "0.1.0", "node_id": "review" } }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    settle(&env).await;
    let evs = env.backend.get_events(&eid).await.unwrap();
    assert_eq!(
        reqs(&evs, "review").len(),
        1,
        "the real node asks: {evs:#?}"
    );
}

/// Identifiers that reach the audit log's hashed fields refuse control
/// characters: the v1 entry hash joins fields with U+001F.
#[tokio::test]
async fn a_worker_id_with_a_control_character_is_refused() {
    let env = env(true).await;
    let (st, body) = call(
        &env.app,
        "POST",
        "/work-items/claim",
        "",
        json!({ "worker_id": "mallory\u{1f}system", "queue_types": ["python_tool"] }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn an_approver_name_with_a_control_character_is_refused() {
    let env = env(true).await;
    let ir = wf(
        "wf-c",
        "review",
        json!({ "review": ha("review", None, None), "publish": step("publish") }),
        vec![("review", "publish"), ("publish", "end")],
    );
    let (st, body) = call(&env.app, "POST", "/workflows", "", json!({ "ir": ir })).await;
    assert!(st.is_success(), "{body}");
    let (_, body) = call(
        &env.app,
        "POST",
        "/executions",
        "",
        json!({ "workflow_id": "wf-c", "workflow_version": "0.1.0", "input": {} }),
    )
    .await;
    let eid = eid_of(&body);
    settle(&env).await;
    let (st, body) = call(
        &env.app,
        "POST",
        &path(&eid, "/approve"),
        "",
        json!({ "decision": "approved", "node_id": "review", "user_id": "mallory\u{1f}system" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(env
        .backend
        .get_events(&eid)
        .await
        .unwrap()
        .iter()
        .all(|e| !matches!(e.kind, EventKind::ApprovalReceived { .. })));
}

#[tokio::test]
async fn a_handler_name_with_a_control_character_is_refused_at_registration() {
    let env = env(true).await;
    let mut bad = wf(
        "wf-c2",
        "review",
        json!({ "review": ha("review", None, None), "publish": step("publish") }),
        vec![("review", "publish"), ("publish", "end")],
    );
    bad["nodes"]["publish"]["kind"]["function"] = json!("pub\u{1f}lish");
    let (st, body) = call(&env.app, "POST", "/workflows", "", json!({ "ir": bad })).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("control characters"), "{body}");
}

/// PROBE C: invisible format characters (zero-width, bidirectional
/// overrides and isolates, the byte-order mark) are refused in the same
/// identifiers as control characters: handler names, workflow ids, worker
/// ids, tenant ids and approver names. Letters of any script, accents, emoji
/// and NBSP are not.
#[tokio::test]
async fn identifiers_refuse_format_characters_but_not_unicode_names() {
    let env = env(true).await;
    let named = |id: &str, f: &str| {
        let mut ir = wf(id, "a", json!({ "a": step("a") }), vec![("a", "end")]);
        ir["nodes"]["a"]["kind"]["function"] = json!(f);
        ir
    };
    for (id, f) in [
        ("f1", "pay\u{202e}lmth"),
        ("f2", "zero\u{200b}width"),
        ("f3", "bom\u{feff}x"),
        ("f4", "iso\u{2066}x"),
    ] {
        let (st, body) = call(
            &env.app,
            "POST",
            "/workflows",
            "",
            json!({ "ir": named(id, f) }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{f:?}: {body}");
        assert!(
            body.to_string().contains("invisible format characters"),
            "{body}"
        );
    }
    for (id, f) in [("u1", "数据_é_🙂"), ("u2", "nbsp\u{a0}x")] {
        let (st, body) = call(
            &env.app,
            "POST",
            "/workflows",
            "",
            json!({ "ir": named(id, f) }),
        )
        .await;
        assert!(st.is_success(), "{f:?}: {st} {body}");
    }
    let mut ir = named("tmp", "f");
    ir["workflow_id"] = json!("wf\u{202e}x");
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        "",
        json!({ "ir": ir.clone() }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    ir["workflow_id"] = json!("wf é");
    let (st, body) = call(&env.app, "POST", "/workflows", "", json!({ "ir": ir })).await;
    assert!(st.is_success(), "{st} {body}");

    for (worker, ok) in [
        ("w\u{202e}", false),
        ("w\u{200b}", false),
        ("wörker-数据", true),
    ] {
        let (st, body) = call(
            &env.app,
            "POST",
            "/work-items/claim",
            "",
            json!({ "worker_id": worker, "queue_types": ["python_tool"] }),
        )
        .await;
        assert_eq!(
            st == StatusCode::BAD_REQUEST,
            !ok,
            "{worker:?}: {st} {body}"
        );
    }
    for (tenant, ok) in [
        ("t\u{200b}x", false),
        ("t\u{2069}", false),
        ("tenant-é", true),
    ] {
        let (st, body) = call(
            &env.app,
            "POST",
            "/tenants",
            "",
            json!({ "id": tenant, "name": "t" }),
        )
        .await;
        assert_eq!(
            st == StatusCode::BAD_REQUEST,
            !ok,
            "{tenant:?}: {st} {body}"
        );
    }

    let ir = wf(
        "wf-cf",
        "review",
        json!({ "review": ha("review", None, None), "publish": step("publish") }),
        vec![("review", "publish"), ("publish", "end")],
    );
    let (st, body) = call(&env.app, "POST", "/workflows", "", json!({ "ir": ir })).await;
    assert!(st.is_success(), "{body}");
    let (_, body) = call(
        &env.app,
        "POST",
        "/executions",
        "",
        json!({ "workflow_id": "wf-cf", "workflow_version": "0.1.0", "input": {} }),
    )
    .await;
    let eid = eid_of(&body);
    settle(&env).await;
    let (st, body) = call(
        &env.app,
        "POST",
        &path(&eid, "/approve"),
        "",
        json!({ "decision": "approved", "node_id": "review", "user_id": "bob\u{200f}" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    let (st, body) = call(
        &env.app,
        "POST",
        &path(&eid, "/approve"),
        "",
        json!({ "decision": "approved", "node_id": "review", "user_id": "bøb 数据" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
}

/// The heartbeat refuses a worker id with a control character, as the claim
/// does. The same request with a clean id is not a 400, so the refusal is the
/// character check's.
#[tokio::test]
async fn a_heartbeat_worker_id_with_a_control_character_is_refused() {
    let env = env(true).await;
    let uri = format!("/work-items/{}/heartbeat", uuid::Uuid::new_v4());
    let (st, body) = call(
        &env.app,
        "POST",
        &uri,
        "",
        json!({ "worker_id": "mallory\u{1f}system", "lease_fence": 1 }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("worker_id"), "{body}");
    let (st, body) = call(
        &env.app,
        "POST",
        &uri,
        "",
        json!({ "worker_id": "mallory", "lease_fence": 1 }),
    )
    .await;
    assert_ne!(st, StatusCode::BAD_REQUEST, "{body}");
}

/// PROBE E2: the record the scheduler writes when an approval expires is in
/// the execution `GET /executions/:id` returns, while the run waits on the
/// fallback and after it completes. It used to reach only the event log, so
/// the execution showed `__approvals` null while the run routed on it.
#[tokio::test]
async fn an_expiry_record_is_in_the_execution_state() {
    let env = env(false).await;
    let ir = wf(
        "wf-e2",
        "review",
        json!({ "review": ha("review", Some(60), Some("notify")), "publish": step("publish"),
            "notify": step("notify") }),
        vec![("review", "publish"), ("publish", "end"), ("notify", "end")],
    );
    let eid = start(&env, ir, json!({ "x": 1 })).await;
    for _ in 0..3 {
        let _ = env.engine.poll_and_execute().await;
        env.scheduler.tick().await.unwrap();
    }
    let evs = env.backend.get_events(&eid).await.unwrap();
    let at: chrono::DateTime<chrono::Utc> = reqs(&evs, "review")[0]["expires_at"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    env.scheduler
        .sweep_approval_deadlines(&eid, at)
        .await
        .unwrap();
    for _ in 0..3 {
        env.scheduler.tick().await.unwrap();
    }
    let (st, exec) = call(&env.app, "GET", &path(&eid, ""), &env.alice, Value::Null).await;
    assert_eq!(st, StatusCode::OK, "{exec}");
    let state = &exec["current_state"];
    assert_eq!(
        state["__approvals"]["review"]["decision"], "expired",
        "{exec}"
    );
    assert_eq!(state["__approvals"]["review"]["approved"], false, "{exec}");
    assert_eq!(state["x"], 1, "{exec}");

    // An external worker runs the fallback and reports over HTTP.
    let (st, claimed) = call(
        &env.app,
        "POST",
        "/work-items/claim",
        &env.alice,
        json!({ "worker_id": "ext", "queue_types": ["python_tool"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(claimed["work_item"]["node_id"], "notify", "{claimed}");
    let (st, body) = call(
        &env.app,
        "POST",
        &format!(
            "/work-items/{}/complete",
            claimed["work_item"]["id"].as_str().unwrap()
        ),
        &env.alice,
        json!({ "output": {}, "state_patch": { "notified": true },
                "lease_fence": claimed["work_item"]["lease_fence"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    for _ in 0..3 {
        env.scheduler.tick().await.unwrap();
    }
    let (_, exec) = call(&env.app, "GET", &path(&eid, ""), &env.alice, Value::Null).await;
    assert_eq!(exec["status"], "completed", "{exec}");
    let state = &exec["current_state"];
    assert_eq!(
        state["__approvals"]["review"]["decision"], "expired",
        "{exec}"
    );
    assert_eq!(state["notified"], true, "{exec}");
}

/// PROBE J: `POST /work-items` takes only a node the execution's workflow
/// has, on the queue its kind is dispatched to. An unknown node and a
/// `python_fn` on an engine queue used to be accepted (201); the engine then
/// failed the node for want of an executor.
#[tokio::test]
async fn an_enqueue_names_a_real_node_on_its_own_queue() {
    let env = env(true).await;
    let ir = wf(
        "wf-j2",
        "s",
        json!({ "s": step("s"), "review": ha("review", None, None), "publish": step("publish") }),
        vec![("s", "review"), ("review", "publish"), ("publish", "end")],
    );
    let (st, body) = call(&env.app, "POST", "/workflows", "", json!({ "ir": ir })).await;
    assert!(st.is_success(), "{body}");
    let (_, body) = call(
        &env.app,
        "POST",
        "/executions",
        "",
        json!({ "workflow_id": "wf-j2", "workflow_version": "0.1.0", "input": {} }),
    )
    .await;
    let id = body["execution_id"].as_str().unwrap().to_string();
    let eid = eid_of(&body);
    let enqueue = |node: &str, queue: &str| {
        json!({ "execution_id": id, "node_id": node, "queue_type": queue,
                "payload": { "workflow_id": "wf-j2", "workflow_version": "0.1.0", "node_id": node } })
    };
    let before = env.backend.get_events(&eid).await.unwrap().len();
    for (node, queue, needle) in [
        ("ghost", "tool", "does not exist"),
        ("ghost", "python_tool", "does not exist"),
    ] {
        let (st, body) = call(&env.app, "POST", "/work-items", "", enqueue(node, queue)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{node}@{queue}: {body}");
        assert!(body.to_string().contains(needle), "{node}@{queue}: {body}");
    }
    for queue in ["general", "tool", "model"] {
        let (st, body) = call(
            &env.app,
            "POST",
            "/work-items",
            "",
            enqueue("publish", queue),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "publish@{queue}: {body}");
        assert!(
            body.to_string().contains("queue 'python_tool'"),
            "publish@{queue}: {body}"
        );
    }
    assert_eq!(
        env.backend.get_events(&eid).await.unwrap().len(),
        before,
        "a refused enqueue schedules nothing"
    );
    let (st, body) = call(
        &env.app,
        "POST",
        "/work-items",
        "",
        enqueue("publish", "python_tool"),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
}

/// An execution of a workflow version that is not registered (the IR
/// cannot be read).
async fn orphan_execution(env: &Env) -> ExecutionId {
    let orphan = ExecutionId::new();
    let now = chrono::Utc::now();
    env.backend
        .create_execution(jamjet_core::workflow::WorkflowExecution {
            execution_id: orphan.clone(),
            workflow_id: "wf-gone".into(),
            workflow_version: "0.1.0".into(),
            status: jamjet_core::workflow::WorkflowStatus::Running,
            initial_input: json!({}),
            current_state: json!({}),
            started_at: now,
            updated_at: now,
            completed_at: None,
            session_type: None,
            parent_execution_id: None,
            segment_number: 0,
        })
        .await
        .unwrap();
    orphan
}

/// PROBE J: the enqueue FAILS CLOSED. When the execution's workflow cannot
/// be read, nothing is known about the node, and the enqueue is refused
/// (it used to read as "not an engine node" and be accepted).
#[tokio::test]
async fn an_enqueue_for_an_unreadable_workflow_is_refused() {
    let env = env(true).await;
    let orphan = orphan_execution(&env).await;
    let (st, body) = call(
        &env.app,
        "POST",
        "/work-items",
        "",
        json!({ "execution_id": format!("exec_{}", orphan.0.simple()), "node_id": "publish",
                "queue_type": "python_tool", "payload": {} }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert!(body.to_string().contains("not registered"), "{body}");
    assert!(env.backend.get_events(&orphan).await.unwrap().is_empty());
    let (st, body) = call(
        &env.app,
        "POST",
        "/work-items",
        "",
        json!({ "execution_id": format!("exec_{}", ExecutionId::new().0.simple()),
                "node_id": "publish", "queue_type": "python_tool", "payload": {} }),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
}

/// Nor does an external worker report a failure for a node whose workflow
/// cannot be read: the node kind check fails closed there too.
#[tokio::test]
async fn a_failure_report_for_an_unreadable_workflow_is_refused() {
    let env = env(true).await;
    let orphan = orphan_execution(&env).await;
    let item = WorkItem {
        id: uuid::Uuid::new_v4(),
        execution_id: orphan.clone(),
        node_id: "publish".into(),
        queue_type: "python_tool".into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: chrono::Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: jamjet_state::DEFAULT_TENANT.to_string(),
    };
    env.backend.enqueue_work_item(item.clone()).await.unwrap();
    let claimed = env
        .backend
        .claim_work_item_with_lease("ext", &["python_tool"], None)
        .await
        .unwrap()
        .expect("the item");
    assert_eq!(claimed.id, item.id);
    let (st, body) = call(
        &env.app,
        "POST",
        &format!("/work-items/{}/fail", item.id),
        "",
        json!({ "error": "boom", "lease_fence": claimed.lease_fence }),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .unwrap()
            .contains("cannot be checked"),
        "{body}"
    );
    assert!(
        env.backend.get_events(&orphan).await.unwrap().is_empty(),
        "nothing is recorded"
    );
}

/// Registration refuses an edge to a node that does not exist (it used to
/// register, 201).
#[tokio::test]
async fn an_edge_to_a_missing_node_is_refused() {
    let env = env(false).await;
    let ghost = wf(
        "wf-ghost",
        "a",
        json!({ "a": step("a") }),
        vec![("a", "ghost"), ("a", "end")],
    );
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        json!({ "ir": ghost }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    let msg = body.to_string();
    assert!(
        msg.contains("'ghost'") && msg.contains("does not exist"),
        "{msg}"
    );
    let ok = wf(
        "wf-edges-ok",
        "a",
        json!({ "a": step("a"), "b": step("b") }),
        vec![("a", "b"), ("b", "end")],
    );
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        json!({ "ir": ok }),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
}

/// A start node that does not exist is named as such, rather than reported
/// as every other node lacking an incoming edge.
#[tokio::test]
async fn a_missing_start_node_is_named() {
    let env = env(false).await;
    let no_start = wf(
        "wf-nostart",
        "nope",
        json!({ "a": step("a") }),
        vec![("a", "end")],
    );
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        json!({ "ir": no_start }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    let msg = body.to_string();
    assert!(msg.contains("start node 'nope' does not exist"), "{msg}");
    assert!(!msg.contains("no incoming edge"), "{msg}");
}
