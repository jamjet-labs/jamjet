//! A policy-blocked node fails its run instead of stalling it.
//!
//! The claim route settled a blocked item as `failed` and emitted nothing, so
//! the node stayed in the scheduler fold's `scheduled` set and the run stayed
//! `Running` forever; parents and segment chains waiting on it hung too. The
//! in-process worker returned an error without settling, so only lease expiry,
//! retries and exhaustion ended it, each retry recording another violation
//! (review I2). Both transports now commit `NodeFailed { retryable: false }`
//! fenced, and the scheduler fails the run on its next tick.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::InMemoryBackend;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

mod support;

fn blocked_ir() -> Value {
    json!({
        "workflow_id": "wf-blocked",
        "version": "0.1.0",
        "name": null,
        "description": null,
        "state_schema": "",
        "start_node": "gate",
        "nodes": {
            "gate": {
                "id": "gate",
                "kind": { "type": "tool", "tool_ref": "payments.transfer", "input_mapping": {}, "output_schema": "" },
                "retry_policy": null,
                "node_timeout_secs": null,
                "description": null,
                "labels": {},
                "policy": { "blocked_tools": ["payments.*"], "require_approval_for": [], "model_allowlist": [] }
            }
        },
        "edges": [ { "from": "gate", "to": "end", "condition": null } ],
        "retry_policies": {},
        "timeouts": {},
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {}
    })
}

async fn send(app: &axum::Router, uri: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(uri)
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

struct Env {
    backend: Arc<InMemoryBackend>,
    app: axum::Router,
    alice: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Env {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

async fn env(in_process_workers: bool) -> Env {
    let backend = Arc::new(InMemoryBackend::new());
    let state = support::make_state(backend.clone() as Arc<dyn StateBackend>);
    let app = build_router_with_opts(state, false);
    let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
    let dyn_backend = backend.clone() as Arc<dyn StateBackend>;
    let sched_backend = dyn_backend.clone();
    let mut tasks = vec![tokio::spawn(async move {
        Scheduler::new(sched_backend)
            .with_poll_interval(Duration::from_millis(10))
            .run()
            .await
    })];
    if in_process_workers {
        tasks.extend(jamjet_worker::default_pool(dyn_backend).spawn());
    }
    Env {
        backend,
        app,
        alice,
        tasks,
    }
}

async fn start(env: &Env) -> ExecutionId {
    let (st, body) = send(
        &env.app,
        "/workflows",
        &env.alice,
        json!({ "ir": blocked_ir() }),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    let (st, body) = send(
        &env.app,
        "/executions",
        &env.alice,
        json!({ "workflow_id": "wf-blocked", "workflow_version": "0.1.0", "input": {} }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let id = body["execution_id"].as_str().unwrap();
    ExecutionId(uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap())
}

/// Poll the run until it is terminal or `secs` pass.
async fn status_after(env: &Env, eid: &ExecutionId, secs: u64) -> WorkflowStatus {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let status = env
            .backend
            .get_execution(eid)
            .await
            .unwrap()
            .unwrap()
            .status;
        if status.is_terminal() || tokio::time::Instant::now() > deadline {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn count(events: &[jamjet_state::Event], f: impl Fn(&EventKind) -> bool) -> usize {
    events.iter().filter(|e| f(&e.kind)).count()
}

async fn assert_failed_once(env: &Env, eid: &ExecutionId) {
    let status = status_after(env, eid, 5).await;
    let events = env.backend.get_events(eid).await.unwrap();
    assert_eq!(
        status,
        WorkflowStatus::Failed,
        "a blocked node must fail its run, not stall it: {:#?}",
        events.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );
    assert_eq!(
        count(&events, |k| matches!(
            k,
            EventKind::NodeFailed { node_id, retryable: false, .. } if node_id == "gate"
        )),
        1,
        "one terminal NodeFailed"
    );
    assert_eq!(
        count(&events, |k| matches!(k, EventKind::PolicyViolation { .. })),
        1,
        "the block is recorded once, not once per retry"
    );
    assert_eq!(
        count(&events, |k| matches!(k, EventKind::NodeCompleted { .. })),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_blocked_at_the_claim_route_fails_the_run() {
    let env = env(false).await;
    let eid = start(&env).await;
    let (st, body) = send(
        &env.app,
        "/work-items/claim",
        &env.alice,
        json!({ "worker_id": "w", "queue_types": ["tool"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "claimed": false }));
    assert_failed_once(&env, &eid).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_blocked_in_process_fails_the_run() {
    let env = env(true).await;
    let eid = start(&env).await;
    assert_failed_once(&env, &eid).await;
}

/// An item whose node the engine cannot evaluate (here, a kind it does not
/// know) is a permanent refusal too: withheld, and its node failed for good
/// rather than left scheduled.
#[tokio::test]
async fn an_unevaluable_node_is_failed_not_stranded() {
    let env = env(false).await;
    for t in &env.tasks {
        t.abort();
    }
    let mut ir = blocked_ir();
    ir["nodes"]["gate"]["kind"] = json!({ "type": "no_such_kind" });
    env.backend
        .store_workflow(jamjet_state::WorkflowDefinition {
            workflow_id: "wf-blocked".into(),
            version: "0.1.0".into(),
            ir,
            created_at: chrono::Utc::now(),
            tenant_id: "default".into(),
        })
        .await
        .unwrap();
    let eid = ExecutionId::new();
    let now = chrono::Utc::now();
    env.backend
        .create_execution(jamjet_core::workflow::WorkflowExecution {
            execution_id: eid.clone(),
            workflow_id: "wf-blocked".into(),
            workflow_version: "0.1.0".into(),
            status: WorkflowStatus::Running,
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
    env.backend
        .enqueue_work_item(jamjet_state::WorkItem {
            id: uuid::Uuid::new_v4(),
            execution_id: eid.clone(),
            node_id: "gate".into(),
            queue_type: "tool".into(),
            payload: json!({ "workflow_id": "wf-blocked", "workflow_version": "0.1.0" }),
            attempt: 0,
            max_attempts: 3,
            created_at: now,
            lease_expires_at: None,
            worker_id: None,
            lease_fence: 0,
            tenant_id: "default".into(),
        })
        .await
        .unwrap();

    let (st, body) = send(
        &env.app,
        "/work-items/claim",
        &env.alice,
        json!({ "worker_id": "w", "queue_types": ["tool"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "claimed": false }));
    let events = env.backend.get_events(&eid).await.unwrap();
    assert_eq!(
        count(&events, |k| matches!(
            k,
            EventKind::NodeFailed { node_id, retryable: false, .. } if node_id == "gate"
        )),
        1,
        "{events:#?}"
    );
}
