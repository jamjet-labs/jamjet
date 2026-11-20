//! Spike finding G1 (2026-09-06): queue type was derived from node kind, so a
//! worker in any language but Python or Java had nothing to claim. An
//! `external_fn` node names its queue and its handler; any worker that speaks
//! the claim/heartbeat/complete protocol can drain it.
mod support;

use axum::http::StatusCode;
use jamjet_state::backend::StateBackend;
use jamjet_state::InMemoryBackend;
use jamjet_testkit::{Hooks, Intercept, Op};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use support::{make_state, post, register_and_start, TestBackend};

fn external_fn_ir() -> Value {
    json!({
        "workflow_id": "wf-external",
        "version": "0.1.0",
        "name": null, "description": null, "state_schema": "",
        "start_node": "authorize",
        "nodes": {
            "authorize": {
                "id": "authorize",
                "kind": { "type": "external_fn", "queue": "pas-control", "handler": "authorize", "output_schema": "" },
                "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
            }
        },
        "edges": [ { "from": "authorize", "to": "end", "condition": null } ],
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

#[tokio::test]
async fn an_external_fn_node_is_claimable_from_its_named_queue_with_handler_and_input() {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let state = make_state(backend);
    register_and_start(&state, external_fn_ir(), json!({ "key": "value" })).await;

    let (st, body) = post(
        &state,
        "/work-items/claim",
        json!({ "worker_id": "ts-worker", "queue_types": ["python_tool"] }),
    )
    .await;
    assert!(st.is_success());
    assert_eq!(
        body["claimed"], false,
        "the python queue must not see it, got {body}"
    );

    let (st, body) = post(
        &state,
        "/work-items/claim",
        json!({ "worker_id": "ts-worker", "queue_types": ["pas-control"] }),
    )
    .await;
    assert!(st.is_success());
    assert_eq!(body["claimed"], true, "got {body}");
    let wi = &body["work_item"];
    assert_eq!(wi["queue_type"], "pas-control");
    assert_eq!(wi["node_id"], "authorize");
    assert_eq!(wi["payload"]["handler"], "authorize");
    assert_eq!(wi["payload"]["input"]["key"], "value");
}

/// M6 (PR #140 review): an `external_fn` queue name is validated when the
/// workflow is registered. An empty name, a built-in queue's name (whose
/// worker cannot run the node: a `python_tool` worker gets a payload with no
/// module or function and fails it three times), or a name outside a safe
/// charset is refused at the moment the author is listening.
#[tokio::test]
async fn an_external_fn_queue_name_must_be_a_safe_non_builtin_name() {
    let too_long = "q".repeat(jamjet_ir::MAX_QUEUE_NAME_LEN + 1);
    for bad in [
        "",
        too_long.as_str(),
        "python_tool",
        "java_tool",
        "model",
        "general",
        "tool",
        "retrieval",
        "privileged",
        "has space",
        "slash/name",
        "über",
    ] {
        let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
        let state = make_state(backend);
        let mut ir = external_fn_ir();
        ir["nodes"]["authorize"]["kind"]["queue"] = json!(bad);
        let (st, body) = post(&state, "/workflows", json!({ "ir": ir })).await;
        assert!(
            st.is_client_error(),
            "queue name {bad:?} must be refused at registration, got {st} {body}"
        );
    }
    for good in ["pas-control", "gpu_queue.v2", "A1"] {
        let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
        let state = make_state(backend);
        let mut ir = external_fn_ir();
        ir["nodes"]["authorize"]["kind"]["queue"] = json!(good);
        let (st, body) = post(&state, "/workflows", json!({ "ir": ir })).await;
        assert!(
            st.is_success(),
            "queue name {good:?} must register, got {st} {body}"
        );
    }
}

/// The node's items that are pending or claimed.
async fn live_items(backend: &TestBackend, execution_id: &str, node_id: &str) -> usize {
    let uuid = uuid::Uuid::parse_str(execution_id.trim_start_matches("exec_")).unwrap();
    match backend {
        TestBackend::Memory(b) => {
            let mut live = 0;
            for id in b.work_item_ids_for_test() {
                if let Some(item) = b.get_work_item(id).await.unwrap() {
                    if item.execution_id.0 == uuid && item.node_id == node_id {
                        live += 1;
                    }
                }
            }
            live
        }
        TestBackend::Sqlite { url, .. } => {
            let pool = sqlx::SqlitePool::connect(url).await.unwrap();
            let (live,): (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM work_items WHERE execution_id = ? AND node_id = ? \
                 AND status IN ('pending', 'claimed')",
            )
            .bind(uuid.to_string())
            .bind(node_id)
            .fetch_one(&pool)
            .await
            .unwrap();
            pool.close().await;
            live as usize
        }
    }
}

/// `POST /work-items` for a node the engine already dispatched would put a
/// second item for the same step on the queue, and two workers would each
/// run it. The node still has its live item, so the hand enqueue is refused.
#[tokio::test]
async fn hand_enqueue_of_a_node_with_a_live_item_is_refused() {
    let ir = external_fn_ir();
    let queue = ir["nodes"]["authorize"]["kind"]["queue"].clone();
    for backend in support::backends().await {
        let name = backend.name();
        let state = make_state(backend.dyn_backend());
        let execution_id = register_and_start(&state, ir.clone(), json!({})).await;
        assert_eq!(
            live_items(&backend, &execution_id, "authorize").await,
            1,
            "{name}: the start dispatched the node with one item"
        );

        let (st, body) = post(
            &state,
            "/work-items",
            json!({
                "execution_id": execution_id,
                "node_id": "authorize",
                "queue_type": queue,
                "payload": {}
            }),
        )
        .await;
        let live = live_items(&backend, &execution_id, "authorize").await;
        assert!(
            st == StatusCode::CONFLICT && live == 1,
            "{name}: a hand enqueue of a node with a live item is refused and adds \
             nothing; got {st} {body}, {live} live items"
        );
        assert_eq!(
            body,
            json!({ "error": "node already has a pending or claimed item" }),
            "{name}"
        );
    }
}

/// Take the node's item and settle it, as a worker that ran it would, so
/// the node has no live item.
async fn settle_the_start_item(backend: &dyn StateBackend, queue: &str) {
    let item = backend
        .claim_work_item("test-worker", &[queue])
        .await
        .unwrap()
        .expect("the start node's item");
    assert!(backend
        .complete_work_item_fenced(item.id, item.lease_fence)
        .await
        .unwrap());
}

/// A finished run gets no new item: the hand enqueue's pass is refused, as
/// the scheduler's is.
#[tokio::test]
async fn hand_enqueue_on_a_finished_run_is_refused() {
    let ir = external_fn_ir();
    let queue = ir["nodes"]["authorize"]["kind"]["queue"].clone();
    for backend in support::backends().await {
        let name = backend.name();
        let state = make_state(backend.dyn_backend());
        let execution_id = register_and_start(&state, ir.clone(), json!({})).await;
        let store = backend.dyn_backend();
        settle_the_start_item(store.as_ref(), queue.as_str().unwrap()).await;
        let id = jamjet_core::workflow::ExecutionId(
            uuid::Uuid::parse_str(execution_id.trim_start_matches("exec_")).unwrap(),
        );
        store
            .finish_execution(
                jamjet_state::Event::new(
                    id.clone(),
                    0,
                    jamjet_state::EventKind::WorkflowCompleted {
                        final_state: json!({}),
                    },
                ),
                jamjet_core::workflow::WorkflowStatus::Completed,
            )
            .await
            .unwrap();
        let before = store.get_events(&id).await.unwrap().len();

        let (st, body) = post(
            &state,
            "/work-items",
            json!({
                "execution_id": execution_id,
                "node_id": "authorize",
                "queue_type": queue,
                "payload": {}
            }),
        )
        .await;
        assert_eq!(
            (st, body),
            (
                StatusCode::CONFLICT,
                json!({ "error": "execution is Completed" })
            ),
            "{name}"
        );
        assert_eq!(
            live_items(&backend, &execution_id, "authorize").await,
            0,
            "{name}: no item"
        );
        assert_eq!(
            store.get_events(&id).await.unwrap().len(),
            before,
            "{name}: nothing appended to the finished run"
        );
    }
}

/// Moves the run's log once, right after the route read its last sequence,
/// as another writer landing between the read and the write would, and
/// counts the passes the route sends.
struct MoveTheLogOnce {
    inner: Arc<dyn StateBackend>,
    armed: AtomicBool,
    passes: AtomicUsize,
}

#[async_trait::async_trait]
impl Hooks for MoveTheLogOnce {
    async fn before(&self, op: &Op<'_>) -> Option<jamjet_state::StateBackendError> {
        if matches!(op, Op::CommitSchedulerPass(_)) {
            self.passes.fetch_add(1, Ordering::SeqCst);
        }
        None
    }

    async fn after(&self, op: &Op<'_>) {
        let Op::LatestSequence(execution_id) = op else {
            return;
        };
        if self.armed.swap(false, Ordering::SeqCst) {
            self.inner
                .append_event(jamjet_state::Event::new(
                    (*execution_id).clone(),
                    0,
                    jamjet_state::EventKind::NodeStarted {
                        node_id: "authorize".into(),
                        worker_id: "another-worker".into(),
                        attempt: 0,
                    },
                ))
                .await
                .unwrap();
        }
    }
}

/// A pass refused only because the log moved is sent again from the
/// sequence the store gave back, and the hand enqueue goes through.
#[tokio::test]
async fn a_hand_enqueue_is_tried_again_when_the_log_moved() {
    let ir = external_fn_ir();
    let queue = ir["nodes"]["authorize"]["kind"]["queue"].clone();
    for backend in support::backends().await {
        let name = backend.name();
        let hooks = Arc::new(MoveTheLogOnce {
            inner: backend.dyn_backend(),
            armed: AtomicBool::new(false),
            passes: AtomicUsize::new(0),
        });
        let state = make_state(Arc::new(Intercept::new(
            backend.dyn_backend(),
            hooks.clone(),
        )));
        let execution_id = register_and_start(&state, ir.clone(), json!({})).await;
        settle_the_start_item(backend.dyn_backend().as_ref(), queue.as_str().unwrap()).await;

        hooks.armed.store(true, Ordering::SeqCst);
        let (st, body) = post(
            &state,
            "/work-items",
            json!({
                "execution_id": execution_id,
                "node_id": "authorize",
                "queue_type": queue,
                "payload": {}
            }),
        )
        .await;
        assert!(
            !hooks.armed.load(Ordering::SeqCst),
            "{name}: the log was moved"
        );
        assert_eq!(
            (st, hooks.passes.load(Ordering::SeqCst)),
            (StatusCode::CREATED, 2),
            "{name}: the first pass is stale and the second lands; body {body}"
        );
        assert_eq!(
            live_items(&backend, &execution_id, "authorize").await,
            1,
            "{name}: one new item"
        );
    }
}
