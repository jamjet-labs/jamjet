//! What a `human_approval` decision binds, and who may report the node.
//!
//! Review of PR #169 (reviewer's probes, kept as regressions):
//! - The request's action digest covered no data: the node's payload carried
//!   only the workflow and node ids. A parallel branch could rewrite the run's
//!   state after the reviewer was asked, the reviewer approved what they had
//!   seen, and the guarded step ran on values nobody approved.
//! - After the approval, any developer token (the run's own initiator
//!   included) could claim the re-dispatched node from the `general` queue
//!   and complete it with a state patch of its choosing. The same route let an
//!   external token complete a `condition` node and pick its route.
//! - The in-process worker took the workflow from the item's payload, so a
//!   hand-enqueued item could run a `human_approval` node as another
//!   workflow's `condition` node, with no gate.

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

/// Records each `python_fn` node it runs with its input. `mutate` rewrites
/// the payment, as a parallel branch might.
#[derive(Clone, Default)]
struct Probe(Arc<Mutex<Vec<(String, Value)>>>);

#[async_trait::async_trait]
impl NodeExecutor for Probe {
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, ExecutorError> {
        self.0.lock().unwrap().push((
            item.node_id.clone(),
            item.payload.get("input").cloned().unwrap_or(Value::Null),
        ));
        let patch = if item.node_id == "mutate" {
            json!({ "amount": 1000000, "payee": "attacker" })
        } else {
            json!({})
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
    json!({
        "id": id,
        "kind": { "type": "python_fn", "module": "myapp", "function": id, "output_schema": "" },
        "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
    })
}

/// s -> review (human_approval) -> publish ; s -> mutate (a parallel branch)
fn ir() -> Value {
    json!({
        "workflow_id": "wf-probe", "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": "s", "policy": null,
        "nodes": {
            "s": step("s"),
            "review": {
                "id": "review",
                "kind": { "type": "human_approval", "description": "Pay?", "timeout_secs": null, "fallback_node": null },
                "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
            },
            "publish": step("publish"),
            "mutate": step("mutate"),
        },
        "edges": [
            { "from": "s", "to": "review", "condition": null },
            { "from": "s", "to": "mutate", "condition": null },
            { "from": "review", "to": "publish", "condition": null },
            { "from": "publish", "to": "end", "condition": null },
            { "from": "mutate", "to": "end", "condition": null }
        ],
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

/// route (condition: state.ok -> publish, else mutate)
fn cond_ir() -> Value {
    let mut ir = ir();
    ir["workflow_id"] = json!("wf-cond");
    ir["start_node"] = json!("route");
    ir["nodes"] = json!({
        "route": {
            "id": "route",
            "kind": { "type": "condition", "branches": [
                { "condition": "state.ok", "target": "publish" },
                { "condition": null, "target": "mutate" }
            ] },
            "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
        },
        "publish": step("publish"),
        "mutate": step("mutate"),
    });
    ir["edges"] = json!([
        { "from": "route", "to": "publish", "condition": null },
        { "from": "route", "to": "mutate", "condition": null },
        { "from": "publish", "to": "end", "condition": null },
        { "from": "mutate", "to": "end", "condition": null }
    ]);
    ir
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
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

async fn env() -> Env {
    let backend: Arc<dyn StateBackend> = Arc::new(jamjet_state::InMemoryBackend::new());
    let app = build_router_with_opts(support::make_state(backend.clone()), false);
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

async fn start(env: &Env) -> ExecutionId {
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        json!({ "ir": ir() }),
    )
    .await;
    assert!(st.is_success(), "register: {st} {body}");
    let (st, body) = call(
        &env.app,
        "POST",
        "/executions",
        &env.alice,
        json!({ "workflow_id": "wf-probe", "workflow_version": "0.1.0", "input": { "amount": 100, "payee": "acme" } }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    eid_of(&body)
}

/// Every request for `review`, oldest first.
fn requests(evs: &[Event]) -> Vec<Value> {
    evs.iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolApprovalRequired {
                node_id, context, ..
            } if node_id == "review" => Some(context.clone()),
            _ => None,
        })
        .collect()
}

async fn settle(env: &Env) {
    for _ in 0..8 {
        let _ = env.engine.poll_and_execute().await;
        let _ = env.external.poll_and_execute().await;
        env.scheduler.tick().await.unwrap();
    }
}

fn ran_input(env: &Env, node: &str) -> Option<Value> {
    env.probe
        .0
        .lock()
        .unwrap()
        .iter()
        .find(|(n, _)| n == node)
        .map(|(_, i)| i.clone())
}

/// Run `s`, let `review` raise its request at amount=100, then let the
/// parallel branch rewrite the payment. Returns the run and the first
/// request's context.
async fn request_then_mutate(env: &Env) -> (ExecutionId, Value) {
    let eid = start(env).await;
    assert!(env.external.poll_and_execute().await.unwrap()); // s
    env.scheduler.tick().await.unwrap();
    assert!(env.engine.poll_and_execute().await.unwrap()); // review asks
    env.scheduler.tick().await.unwrap();
    let ctx = requests(&env.backend.get_events(&eid).await.unwrap())[0].clone();
    assert!(env.external.poll_and_execute().await.unwrap()); // mutate
    env.scheduler.tick().await.unwrap();
    (eid, ctx)
}

/// PROBE A. The request binds the state it was raised on, and the reviewer
/// is shown it. An approval of that request does not release the node once
/// the state has moved: the node asks again, showing the new values.
#[tokio::test]
async fn an_approval_does_not_cover_state_that_moved_after_the_request() {
    let env = env().await;
    let (eid, ctx) = request_then_mutate(&env).await;
    let shown = &ctx["action_basis"]["payload"]["input"];
    assert_eq!(
        shown["amount"], 100,
        "the reviewer sees the bound state: {ctx}"
    );
    assert_eq!(shown["payee"], "acme");

    let (st, body) = call(
        &env.app,
        "POST",
        &path(&eid, "/approve"),
        &env.bob,
        json!({ "decision": "approved", "node_id": "review", "action_digest": ctx["action_digest"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    settle(&env).await;

    assert_eq!(
        ran_input(&env, "publish"),
        None,
        "the guarded step ran on state nobody approved"
    );
    let evs = env.backend.get_events(&eid).await.unwrap();
    let reqs = requests(&evs);
    assert_eq!(reqs.len(), 2, "the node asks again: {evs:#?}");
    let again = &reqs[1]["action_basis"]["payload"]["input"];
    assert_eq!(again["amount"], 1000000);
    assert_eq!(again["payee"], "attacker");

    // Approving what is now shown releases it, on exactly those values.
    let (st, body) = call(
        &env.app,
        "POST",
        &path(&eid, "/approve"),
        &env.bob,
        json!({ "decision": "approved", "node_id": "review", "action_digest": reqs[1]["action_digest"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    settle(&env).await;
    let input = ran_input(&env, "publish").expect("publish runs once approved");
    assert_eq!(input["amount"], 1000000);
    // The decision is in the state the next step reads, under the node id.
    assert_eq!(input["review"]["approved"], true, "{input}");
    assert_eq!(input["review"]["approver"], "bob", "{input}");
}

/// PROBE B. The approved node is not handed to an external worker, and a
/// result reported for it is refused, so no token can complete it with a
/// state patch of its own.
#[tokio::test]
async fn an_external_token_cannot_claim_or_report_an_approved_node() {
    let env = env().await;
    let (eid, ctx) = request_then_mutate(&env).await;
    // Approve the current state so the node is released.
    let (st, _) = call(
        &env.app,
        "POST",
        &path(&eid, "/approve"),
        &env.bob,
        json!({ "decision": "approved", "node_id": "review", "action_digest": ctx["action_digest"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    settle(&env).await;
    let reqs = requests(&env.backend.get_events(&eid).await.unwrap());
    let (st, _) = call(
        &env.app,
        "POST",
        &path(&eid, "/approve"),
        &env.bob,
        json!({ "decision": "approved", "node_id": "review", "action_digest": reqs[1]["action_digest"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    env.scheduler.tick().await.unwrap(); // re-dispatch review on `general`

    let (st, claimed) = call(
        &env.app,
        "POST",
        "/work-items/claim",
        &env.alice,
        json!({ "worker_id": "ext", "queue_types": ["general", "model"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{claimed}");
    assert_eq!(
        claimed,
        json!({ "claimed": false }),
        "the engine's queues are not served"
    );

    // Even holding the item (taken here straight from the backend), a
    // reported result is refused and nothing is recorded.
    let wi = env
        .backend
        .claim_work_item("ext", &["general"])
        .await
        .unwrap()
        .expect("review is queued");
    assert_eq!(wi.node_id, "review");
    let (st, body) = call(
        &env.app,
        "POST",
        &format!("/work-items/{}/complete", wi.id),
        &env.alice,
        json!({ "output": {}, "state_patch": { "amount": 777777 }, "lease_fence": wi.lease_fence }),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    let (st, body) = call(
        &env.app,
        "POST",
        &format!("/work-items/{}/fail", wi.id),
        &env.alice,
        json!({ "error": "nope", "lease_fence": wi.lease_fence }),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    let evs = env.backend.get_events(&eid).await.unwrap();
    assert!(
        !evs.iter().any(|e| matches!(&e.kind,
            EventKind::NodeCompleted { node_id, .. } | EventKind::NodeFailed { node_id, .. }
                if node_id == "review")),
        "{evs:#?}"
    );
}

/// The older hole on the same route: an external token completed a
/// `condition` node and chose its route.
#[tokio::test]
async fn an_external_token_cannot_claim_or_report_a_condition_node() {
    let env = env().await;
    let ir = cond_ir();
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        json!({ "ir": ir }),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    let (st, body) = call(
        &env.app,
        "POST",
        "/executions",
        &env.alice,
        json!({ "workflow_id": "wf-cond", "workflow_version": "0.1.0", "input": { "ok": false } }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let eid = eid_of(&body);

    let (_, claimed) = call(
        &env.app,
        "POST",
        "/work-items/claim",
        &env.alice,
        json!({ "worker_id": "ext", "queue_types": ["general"] }),
    )
    .await;
    assert_eq!(claimed, json!({ "claimed": false }));
    let wi = env
        .backend
        .claim_work_item("ext", &["general"])
        .await
        .unwrap()
        .expect("route is queued");
    let (st, body) = call(
        &env.app,
        "POST",
        &format!("/work-items/{}/complete", wi.id),
        &env.alice,
        json!({ "output": { "chosen_target": "publish" }, "state_patch": {}, "lease_fence": wi.lease_fence }),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    let (st, body) = call(
        &env.app,
        "POST",
        &format!("/work-items/{}/fail", wi.id),
        &env.alice,
        json!({ "error": "nope", "lease_fence": wi.lease_fence }),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    let evs = env.backend.get_events(&eid).await.unwrap();
    assert!(!evs.iter().any(|e| matches!(&e.kind,
        EventKind::NodeCompleted { node_id, .. } | EventKind::NodeFailed { node_id, .. }
            if node_id == "route")));
}

/// An item for an engine-run node is withheld from external workers on any
/// queue, not only the engine's: `POST /work-items` can put one on a queue an
/// external worker polls. (A `condition` node: unlike `human_approval`, the
/// policy guard alone would let it through.) It is settled without failing
/// the node, whose real item belongs to the engine.
#[tokio::test]
async fn an_engine_node_on_an_external_queue_is_withheld() {
    let env = env().await;
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        json!({ "ir": cond_ir() }),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    let (st, body) = call(
        &env.app,
        "POST",
        "/executions",
        &env.alice,
        json!({ "workflow_id": "wf-cond", "workflow_version": "0.1.0", "input": { "ok": false } }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let eid = eid_of(&body);
    env.backend
        .enqueue_work_item(WorkItem {
            id: uuid::Uuid::new_v4(),
            execution_id: eid.clone(),
            node_id: "route".into(),
            queue_type: "python_tool".into(),
            payload: json!({ "workflow_id": "wf-cond", "workflow_version": "0.1.0", "node_id": "route" }),
            attempt: 0,
            max_attempts: 1,
            created_at: chrono::Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            lease_fence: 0,
            tenant_id: jamjet_state::DEFAULT_TENANT.into(),
        })
        .await
        .unwrap();
    let (st, claimed) = call(
        &env.app,
        "POST",
        "/work-items/claim",
        &env.alice,
        json!({ "worker_id": "ext", "queue_types": ["python_tool"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{claimed}");
    assert_eq!(claimed, json!({ "claimed": false }), "{claimed}");
    // The engine still routes the run itself.
    settle(&env).await;
    let evs = env.backend.get_events(&eid).await.unwrap();
    assert!(
        evs.iter().any(
            |e| matches!(&e.kind, EventKind::NodeCompleted { node_id, .. } if node_id == "route")
        ),
        "{evs:#?}"
    );
    assert!(
        ran_input(&env, "mutate").is_some(),
        "the default branch ran"
    );
}

/// The in-process worker reads the workflow from the execution, not the
/// item's payload. A hand-enqueued item for `review` whose payload names a
/// workflow where `review` is a `condition` is settled without running, and
/// the approval still stands between the run and `publish`.
#[tokio::test]
async fn the_engine_does_not_run_an_item_under_the_workflow_its_payload_names() {
    let env = env().await;
    // A second workflow where `review` is an ungated condition.
    let mut other = ir();
    other["workflow_id"] = json!("wf-other");
    other["nodes"]["review"]["kind"] = json!({ "type": "condition", "branches": [] });
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        json!({ "ir": other }),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");

    let eid = start(&env).await;
    assert!(env.external.poll_and_execute().await.unwrap()); // s
                                                             // Before the scheduler dispatches `review`, an item for it is queued by
                                                             // hand, its payload pointing at the other workflow.
    env.backend
        .enqueue_work_item(WorkItem {
            id: uuid::Uuid::new_v4(),
            execution_id: eid.clone(),
            node_id: "review".into(),
            queue_type: "general".into(),
            payload: json!({ "workflow_id": "wf-other", "workflow_version": "0.1.0", "node_id": "review" }),
            attempt: 0,
            max_attempts: 1,
            created_at: chrono::Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            lease_fence: 0,
            tenant_id: jamjet_state::DEFAULT_TENANT.into(),
        })
        .await
        .unwrap();
    assert!(
        env.engine.poll_and_execute().await.unwrap(),
        "the stray item is claimed"
    );
    let evs = env.backend.get_events(&eid).await.unwrap();
    assert!(
        !evs.iter().any(|e| matches!(&e.kind,
            EventKind::NodeCompleted { node_id, .. } | EventKind::NodeStarted { node_id, .. }
                if node_id == "review")),
        "the stray item must not run: {evs:#?}"
    );
    settle(&env).await;
    assert_eq!(
        ran_input(&env, "publish"),
        None,
        "publish waits for the approval"
    );
    assert_eq!(
        requests(&env.backend.get_events(&eid).await.unwrap()).len(),
        1,
        "the real item asks for approval"
    );
}

/// PROBE C. The initiator cannot approve their own run.
#[tokio::test]
async fn the_initiator_cannot_approve() {
    let env = env().await;
    let eid = start(&env).await;
    assert!(env.external.poll_and_execute().await.unwrap());
    env.scheduler.tick().await.unwrap();
    assert!(env.engine.poll_and_execute().await.unwrap());
    env.scheduler.tick().await.unwrap();
    let ctx = requests(&env.backend.get_events(&eid).await.unwrap())[0].clone();
    let (st, body) = call(
        &env.app,
        "POST",
        &path(&eid, "/approve"),
        &env.alice,
        json!({ "decision": "approved", "node_id": "review", "action_digest": ctx["action_digest"] }),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
}
