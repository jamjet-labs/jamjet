//! Spike finding G2 (2026-09-06): `POST /executions` built the START node's
//! work item with only `workflow_id` and `workflow_version`, while every later
//! node dispatched by the scheduler carries `node_id` plus the kind-specific
//! enrichment (`module`, `function`, `input` for `python_fn`). A workflow whose
//! first step is a Python function therefore failed three times and died.
mod support;

use jamjet_state::backend::StateBackend;
use jamjet_state::InMemoryBackend;
use serde_json::json;
use std::sync::Arc;
use support::{make_state, python_fn_start_ir, register_and_start};

#[tokio::test]
async fn python_fn_start_node_work_item_is_enriched_like_every_other_node() {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let state = make_state(backend.clone());

    register_and_start(
        &state,
        python_fn_start_ir("wf-start"),
        json!({ "key": "value" }),
    )
    .await;

    let item = backend
        .claim_work_item("py-worker", &["python_tool"])
        .await
        .expect("claim")
        .expect("the start node must be claimable from python_tool");

    assert_eq!(item.node_id, "py_start");
    assert_eq!(
        item.payload["node_id"], "py_start",
        "base payload carries node_id"
    );
    assert_eq!(
        item.payload["module"], "myapp.tools",
        "python_fn enrichment: module"
    );
    assert_eq!(
        item.payload["function"], "run_check",
        "python_fn enrichment: function"
    );
    assert_eq!(
        item.payload["input"]["key"], "value",
        "python_fn enrichment: input is the initial state"
    );
    assert_eq!(item.payload["workflow_id"], "wf-start");
    assert_eq!(item.payload["workflow_version"], "0.1.0");
}

/// `__budget` is the engine's bookkeeping, left out of the approval digest and
/// read back by the budget enforcer. An initiator must not seed it through
/// the run's input: the start node would receive a key no approval covers,
/// and the run would start with a budget the engine never recorded.
#[tokio::test]
async fn the_initial_input_cannot_seed_engine_bookkeeping() {
    let backend = Arc::new(jamjet_state::InMemoryBackend::new());
    let state = support::make_state(backend.clone() as Arc<dyn StateBackend>);
    let id = support::register_and_start(
        &state,
        support::python_fn_start_ir("wf-seed"),
        json!({ "amount": 1, "__budget": { "total_cost_usd": -1000.0 } }),
    )
    .await;
    let eid = jamjet_core::workflow::ExecutionId(
        uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap(),
    );
    let item = backend
        .claim_work_item_with_lease("probe", &["python_tool"], None)
        .await
        .unwrap()
        .expect("the start item");
    assert_eq!(
        item.payload["input"],
        json!({ "amount": 1 }),
        "{}",
        item.payload
    );
    let exec = backend.get_execution(&eid).await.unwrap().unwrap();
    assert_eq!(exec.initial_input, json!({ "amount": 1 }));
    assert_eq!(exec.current_state, json!({ "amount": 1 }));
    let started = backend
        .get_events(&eid)
        .await
        .unwrap()
        .into_iter()
        .find_map(|e| match e.kind {
            jamjet_state::event::EventKind::WorkflowStarted { initial_input, .. } => {
                Some(initial_input)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(started, json!({ "amount": 1 }));
}
