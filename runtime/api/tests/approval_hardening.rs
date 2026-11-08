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

/// PROBE H: a large state still shows the reviewer every key.
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
        json!({ "notes": "x".repeat(20_000), "zz_payee": "attacker" }),
    )
    .await;
    settle(&env).await;
    let ctx = reqs(&env.backend.get_events(&eid).await.unwrap(), "review")[0].clone();
    let basis = &ctx["action_basis"];
    assert_eq!(basis["payload_truncated"], true);
    assert_eq!(basis["payload"]["input"]["zz_payee"], "attacker", "{basis}");
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
