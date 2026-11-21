//! The scheduler and a run's caps.
//!
//! A step that is claimed under the cap may report more than the cap has
//! left. Its completion is recorded, since the step ran, and then the run is
//! past its cap. The scheduler's next pass must fail the run before it
//! dispatches anything after that step, whichever transport ran it: the
//! in-process worker reports a model, tokens and its idempotency key, an
//! HTTP worker reports what it reports (tokens, a cost, or both).

mod support;

use async_trait::async_trait;
use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use jamjet_ir::workflow::TokenBudgetIr;
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::{StateBackend, StateBackendError, TurnCommit, WorkItem};
use jamjet_state::caps::{may_fire, RunCaps};
use jamjet_state::{fold_run_usage, Event, EventKind};
use jamjet_testkit::{Hooks, Intercept, Op};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use support::{
    kinds, linear, start_run, start_segment, status, step, stores, workflow, Worker, QUEUE,
};

/// Which cap the run carries.
#[derive(Clone, Copy, Debug)]
enum Cap {
    /// `token_budget: {total_tokens: 5}`.
    Tokens,
    /// `cost_budget_usd: 1.0`.
    Dollars,
}

/// How the step's completion reads in the log.
#[derive(Clone, Copy, Debug)]
enum Transport {
    /// As the in-process worker writes it.
    InProcess,
    /// As `POST /work-items/:id/complete` writes it.
    Http,
}

fn capped(cap: Cap, mut ir: Value) -> Value {
    match cap {
        Cap::Tokens => ir["token_budget"] = json!({ "total_tokens": 5 }),
        Cap::Dollars => ir["cost_budget_usd"] = json!(1.0),
    }
    ir
}

/// The completion of `node`, spending past the cap: 10 tokens against 5,
/// or $1.50 against $1.00.
fn spent(cap: Cap, transport: Transport, node: &str, key: String) -> EventKind {
    let cost = match cap {
        Cap::Tokens => None,
        Cap::Dollars => Some(1.5),
    };
    let (system, model, input, output) = match transport {
        Transport::InProcess => (
            Some("anthropic".to_string()),
            Some("claude-sonnet-4-6".to_string()),
            Some(7),
            Some(3),
        ),
        // An HTTP worker under a dollar cap may report a cost and no tokens.
        Transport::Http => match cap {
            Cap::Tokens => (None, None, Some(10), None),
            Cap::Dollars => (None, None, None, None),
        },
    };
    EventKind::NodeCompleted {
        node_id: node.into(),
        output: json!({}),
        state_patch: json!({}),
        duration_ms: 1,
        gen_ai_system: system,
        gen_ai_model: model,
        input_tokens: input,
        output_tokens: output,
        finish_reason: None,
        cost_usd: cost,
        provenance: None,
        idempotency_key: Some(key),
    }
}

/// The trip event the pass records, as JSON.
fn trip(cap: Cap) -> Value {
    match cap {
        Cap::Tokens => json!({
            "type": "token_budget_exceeded",
            "node_id": "a",
            "kind": "total_tokens",
            "limit": 5,
            "current": 10
        }),
        Cap::Dollars => json!({
            "type": "cost_budget_exceeded",
            "node_id": "a",
            "limit_usd": 1.0,
            "current_usd": 1.5
        }),
    }
}

/// `a -> b`. `a` is claimed under the cap and completes past it. The next
/// pass records the trip and fails the run in the same write: nothing is
/// dispatched, failed or skipped after `a`'s completion, and `b` never gets
/// an item.
#[tokio::test]
async fn a_run_past_its_cap_fails_before_any_successor_is_dispatched() {
    for cap in [Cap::Tokens, Cap::Dollars] {
        for transport in [Transport::InProcess, Transport::Http] {
            for store in stores().await {
                let case = format!("{} {cap:?} {transport:?}", store.name());
                let backend = store.backend();
                let eid = start_run(backend.as_ref(), capped(cap, linear(&["a", "b"]))).await;

                let item = backend
                    .claim_work_item("test-worker", &[QUEUE])
                    .await
                    .unwrap()
                    .expect("a's item");
                assert_eq!(item.node_id, "a", "{case}");
                let key = jamjet_state::derive_idempotency_key(backend.as_ref(), &eid, "a")
                    .await
                    .unwrap();
                backend
                    .commit_turn(
                        item.id,
                        item.lease_fence,
                        Event::new(eid.clone(), 0, spent(cap, transport, "a", key)),
                        false,
                    )
                    .await
                    .unwrap();

                let scheduler = Scheduler::new(backend.clone());
                let worker = Worker::default();
                for _ in 0..4 {
                    scheduler.tick().await.unwrap();
                    worker.complete_all(backend.as_ref(), |_| json!({})).await;
                }

                let events = backend.get_events(&eid).await.unwrap();
                let done = events
                    .iter()
                    .position(|e| matches!(&e.kind, EventKind::NodeCompleted { node_id, .. } if node_id == "a"))
                    .expect("a's completion");
                let after: Vec<Value> = events[done + 1..]
                    .iter()
                    .map(|e| serde_json::to_value(&e.kind).unwrap())
                    .collect();
                assert_eq!(
                    after.iter().map(|k| k["type"].clone()).collect::<Vec<_>>(),
                    vec![trip(cap)["type"].clone(), json!("workflow_failed")],
                    "{case}: after a run passes its cap the next pass fails it and does nothing \
                     else; the log was {:?}",
                    kinds(&events)
                );
                assert_eq!(after[0], trip(cap), "{case}");
                assert_eq!(
                    status(backend.as_ref(), &eid).await,
                    WorkflowStatus::Failed,
                    "{case}"
                );
                assert!(
                    worker.items_of("b").is_empty(),
                    "{case}: b was dispatched on a run past its cap"
                );
            }
        }
    }
}

/// `s0`, then `a` and `b` side by side, then `c` once both are done.
fn fan_out() -> Value {
    workflow(
        "s0",
        vec![step("s0"), step("a"), step("b"), step("c")],
        &[
            ("s0", "a", None),
            ("s0", "b", None),
            ("a", "c", None),
            ("b", "c", None),
        ],
    )
}

/// `s0`, then `a` and `b` side by side, and nothing after them.
fn two_branches() -> Value {
    workflow(
        "s0",
        vec![step("s0"), step("a"), step("b")],
        &[("s0", "a", None), ("s0", "b", None)],
    )
}

fn token_capped(total: u32, mut ir: Value) -> Value {
    ir["token_budget"] = json!({ "total_tokens": total });
    ir
}

fn total_tokens_cap(total: u32) -> RunCaps {
    RunCaps {
        token_budget: Some(TokenBudgetIr {
            total_tokens: Some(total),
            input_tokens: None,
            output_tokens: None,
        }),
        cost_budget_usd: None,
    }
}

/// The completion the in-process worker writes for a model step of `node`
/// that spent `input` tokens.
fn model_step(node: &str, input: u64, key: String) -> EventKind {
    EventKind::NodeCompleted {
        node_id: node.into(),
        output: json!({}),
        state_patch: json!({}),
        duration_ms: 1,
        gen_ai_system: Some("anthropic".to_string()),
        gen_ai_model: Some("claude-sonnet-4-6".to_string()),
        input_tokens: Some(input),
        output_tokens: Some(0),
        finish_reason: None,
        cost_usd: None,
        provenance: None,
        idempotency_key: Some(key),
    }
}

/// Complete the claimed `item` as a model step that spent `tokens`.
async fn spend(backend: &dyn StateBackend, item: &WorkItem, tokens: u64) {
    let key = jamjet_state::derive_idempotency_key(backend, &item.execution_id, &item.node_id)
        .await
        .unwrap();
    backend
        .commit_turn(
            item.id,
            item.lease_fence,
            Event::new(
                item.execution_id.clone(),
                0,
                model_step(&item.node_id, tokens, key),
            ),
            false,
        )
        .await
        .unwrap();
}

/// Claim the one pending item, which must be `node`'s.
async fn claim_one(backend: &dyn StateBackend, node: &str) -> WorkItem {
    let item = backend
        .claim_work_item("test-worker", &[QUEUE])
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("{node}'s item"));
    assert_eq!(item.node_id, node);
    item
}

/// Each cap trip in `events`, as JSON.
fn trips(events: &[Event]) -> Vec<Value> {
    events
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                EventKind::TokenBudgetExceeded { .. } | EventKind::CostBudgetExceeded { .. }
            )
        })
        .map(|e| serde_json::to_value(&e.kind).unwrap())
        .collect()
}

/// Each `WorkflowFailed` error in `events`.
fn run_failures(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::WorkflowFailed { error } => Some(error.clone()),
            _ => None,
        })
        .collect()
}

/// The nodes with a `NodeCompleted` in `events`, in log order.
fn completed(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::NodeCompleted { node_id, .. } => Some(node_id.clone()),
            _ => None,
        })
        .collect()
}

/// `a` and `b` are dispatched in one pass at 990 of 1000 tokens, and both
/// are claimed: each claim's cap check passes, since neither has reported
/// yet. That overshoot is the steps in flight. Both completions count, the
/// run fails once with one trip, and `c` never runs.
#[tokio::test]
async fn two_parallel_steps_under_the_cap_both_count_and_the_run_fails_once() {
    let caps = total_tokens_cap(1000);
    for store in stores().await {
        let case = store.name();
        let backend = store.backend();
        let eid = start_run(backend.as_ref(), token_capped(1000, fan_out())).await;
        let scheduler = Scheduler::new(backend.clone());
        let worker = Worker::default();

        let s0 = claim_one(backend.as_ref(), "s0").await;
        spend(backend.as_ref(), &s0, 990).await;
        scheduler.tick().await.unwrap();

        let mut claimed = worker.claim_all(backend.as_ref()).await;
        claimed.sort_by(|x, y| x.node_id.cmp(&y.node_id));
        assert_eq!(
            claimed
                .iter()
                .map(|i| i.node_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"],
            "{case}: both branches are dispatched in one pass"
        );
        // The race opened: the cap check each claim makes lets both through.
        for item in &claimed {
            let usage = fold_run_usage(backend.as_ref(), &eid)
                .await
                .unwrap()
                .unwrap();
            assert!(
                may_fire(&caps, &usage).is_ok(),
                "{case}: {} is claimed at {} of 1000",
                item.node_id,
                usage.total_tokens()
            );
        }

        // a lands the run exactly on its cap: the run goes on, and c waits for b.
        spend(backend.as_ref(), &claimed[0], 10).await;
        scheduler.tick().await.unwrap();
        let events = backend.get_events(&eid).await.unwrap();
        assert_eq!(
            trips(&events),
            Vec::<Value>::new(),
            "{case}: at the cap exactly"
        );
        assert_eq!(
            status(backend.as_ref(), &eid).await,
            WorkflowStatus::Running,
            "{case}"
        );

        // b passes it.
        spend(backend.as_ref(), &claimed[1], 10).await;
        for _ in 0..3 {
            scheduler.tick().await.unwrap();
            worker.complete_all(backend.as_ref(), |_| json!({})).await;
        }

        let events = backend.get_events(&eid).await.unwrap();
        assert_eq!(
            completed(&events),
            ["s0", "a", "b"],
            "{case}: {:?}",
            kinds(&events)
        );
        assert_eq!(
            trips(&events),
            vec![json!({
                "type": "token_budget_exceeded",
                "node_id": "b",
                "kind": "total_tokens",
                "limit": 1000,
                "current": 1010
            })],
            "{case}: one trip, counting both steps"
        );
        assert_eq!(
            run_failures(&events),
            ["token budget reached: total_tokens 1010 of 1000"],
            "{case}"
        );
        assert!(worker.items_of("c").is_empty(), "{case}: c was dispatched");
        assert_eq!(
            status(backend.as_ref(), &eid).await,
            WorkflowStatus::Failed,
            "{case}"
        );
    }
}

/// Fails the first scheduler pass written once armed, and counts the passes.
#[derive(Default)]
struct FailOnePass {
    armed: AtomicBool,
    passes: AtomicUsize,
}

#[async_trait]
impl Hooks for FailOnePass {
    async fn before(&self, op: &Op<'_>) -> Option<StateBackendError> {
        if !matches!(op, Op::CommitSchedulerPass(_)) {
            return None;
        }
        self.passes.fetch_add(1, Ordering::SeqCst);
        self.armed
            .swap(false, Ordering::SeqCst)
            .then(|| StateBackendError::Database("the pass could not be written (test)".into()))
    }
}

/// The trip and the run's failure are one write. A pass the store refuses
/// leaves neither, and the run is still running; the next pass writes both.
#[tokio::test]
async fn the_failure_lands_in_one_pass_write() {
    for cap in [Cap::Tokens, Cap::Dollars] {
        for store in stores().await {
            let case = format!("{} {cap:?}", store.name());
            let backend = store.backend();
            let eid = start_run(backend.as_ref(), capped(cap, linear(&["a", "b"]))).await;
            let item = claim_one(backend.as_ref(), "a").await;
            let key = jamjet_state::derive_idempotency_key(backend.as_ref(), &eid, "a")
                .await
                .unwrap();
            backend
                .commit_turn(
                    item.id,
                    item.lease_fence,
                    Event::new(eid.clone(), 0, spent(cap, Transport::InProcess, "a", key)),
                    false,
                )
                .await
                .unwrap();

            let hooks = Arc::new(FailOnePass {
                armed: AtomicBool::new(true),
                ..Default::default()
            });
            let scheduler =
                Scheduler::new(Arc::new(Intercept::new(backend.clone(), hooks.clone())));
            let after_a = |events: &[Event]| -> Vec<Value> {
                let done = events
                    .iter()
                    .position(|e| matches!(&e.kind, EventKind::NodeCompleted { node_id, .. } if node_id == "a"))
                    .expect("a's completion");
                events[done + 1..]
                    .iter()
                    .map(|e| serde_json::to_value(&e.kind).unwrap())
                    .collect()
            };

            scheduler.tick().await.unwrap();
            assert_eq!(hooks.passes.load(Ordering::SeqCst), 1, "{case}");
            let events = backend.get_events(&eid).await.unwrap();
            assert_eq!(
                after_a(&events),
                Vec::<Value>::new(),
                "{case}: a refused pass writes nothing: {:?}",
                kinds(&events)
            );
            assert_eq!(
                status(backend.as_ref(), &eid).await,
                WorkflowStatus::Running,
                "{case}"
            );

            scheduler.tick().await.unwrap();
            assert_eq!(hooks.passes.load(Ordering::SeqCst), 2, "{case}");
            let events = backend.get_events(&eid).await.unwrap();
            let after = after_a(&events);
            assert_eq!(after.len(), 2, "{case}: {:?}", kinds(&events));
            assert_eq!(after[0], trip(cap), "{case}");
            assert_eq!(after[1]["type"], json!("workflow_failed"), "{case}");
            assert_eq!(
                status(backend.as_ref(), &eid).await,
                WorkflowStatus::Failed,
                "{case}"
            );
        }
    }
}

/// `a` and `b` run side by side; `a` passes the cap and the pass fails the
/// run while `b` is still out. `b`'s completion then finds the run ended:
/// its item is withdrawn and nothing is recorded after the failure.
#[tokio::test]
async fn a_step_completing_after_the_failure_is_withdrawn() {
    for store in stores().await {
        let case = store.name();
        let backend = store.backend();
        let eid = start_run(backend.as_ref(), token_capped(5, two_branches())).await;
        let scheduler = Scheduler::new(backend.clone());
        let worker = Worker::default();

        worker.complete_all(backend.as_ref(), |_| json!({})).await;
        scheduler.tick().await.unwrap();
        let mut claimed = worker.claim_all(backend.as_ref()).await;
        claimed.sort_by(|x, y| x.node_id.cmp(&y.node_id));
        assert_eq!(
            claimed
                .iter()
                .map(|i| i.node_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"],
            "{case}"
        );
        spend(backend.as_ref(), &claimed[0], 10).await;
        scheduler.tick().await.unwrap();
        assert_eq!(
            status(backend.as_ref(), &eid).await,
            WorkflowStatus::Failed,
            "{case}: the pass after a fails the run"
        );

        let b = &claimed[1];
        let key = jamjet_state::derive_idempotency_key(backend.as_ref(), &eid, "b")
            .await
            .unwrap();
        let answer = backend
            .commit_turn_unless_terminal(
                b.id,
                b.lease_fence,
                Event::new(eid.clone(), 0, model_step("b", 3, key)),
                false,
            )
            .await
            .unwrap();
        assert!(
            matches!(
                answer,
                TurnCommit::ExecutionTerminal(WorkflowStatus::Failed)
            ),
            "{case}: {answer:?}"
        );
        let events = backend.get_events(&eid).await.unwrap();
        assert!(
            matches!(
                events.last().map(|e| &e.kind),
                Some(EventKind::WorkflowFailed { .. })
            ),
            "{case}: nothing is recorded after the failure: {:?}",
            kinds(&events)
        );
        assert_eq!(completed(&events), ["s0", "a"], "{case}");
        assert!(
            backend.open_work_items(&eid).await.unwrap().is_empty(),
            "{case}: b's item is withdrawn"
        );
    }
}

/// Counts the reads that fold a run's usage.
#[derive(Default)]
struct Reads {
    events: Mutex<Vec<ExecutionId>>,
    executions: AtomicUsize,
}

#[async_trait]
impl Hooks for Reads {
    async fn before(&self, op: &Op<'_>) -> Option<StateBackendError> {
        match op {
            Op::GetEvents(id) => self.events.lock().unwrap().push((*id).clone()),
            Op::GetExecution(_) => {
                self.executions.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
        None
    }
}

/// Segment 1 of a run whose segment 0 spent `earlier` tokens and ended.
async fn second_segment(
    backend: &dyn StateBackend,
    ir: Value,
    earlier: u64,
) -> (ExecutionId, ExecutionId) {
    let root = start_run(backend, ir.clone()).await;
    let item = claim_one(backend, "a").await;
    spend(backend, &item, earlier).await;
    backend
        .update_execution_status(&root, WorkflowStatus::Completed)
        .await
        .unwrap();
    let child = start_segment(backend, ir, Some((&root, 1))).await;
    (root, child)
}

/// A run with no cap has nothing to fold: the scheduler never reads an
/// earlier segment's log or the run's row for it. With a cap, the earlier
/// segment's log is read once, however many passes the run takes.
#[tokio::test]
async fn a_run_without_caps_is_never_folded() {
    for cap in [false, true] {
        for store in stores().await {
            let case = format!("{} capped: {cap}", store.name());
            let backend = store.backend();
            let mut ir = linear(&["a", "b", "c"]);
            if cap {
                ir = token_capped(100, ir);
            }
            let (root, child) = second_segment(backend.as_ref(), ir, 4).await;

            let reads = Arc::new(Reads::default());
            let scheduler =
                Scheduler::new(Arc::new(Intercept::new(backend.clone(), reads.clone())));
            let worker = Worker::default();
            for _ in 0..5 {
                scheduler.tick().await.unwrap();
                worker.complete_all(backend.as_ref(), |_| json!({})).await;
            }
            assert_eq!(
                status(backend.as_ref(), &child).await,
                WorkflowStatus::Completed,
                "{case}: the run went through several passes to its end"
            );
            let events_read = reads.events.lock().unwrap().clone();
            if cap {
                assert_eq!(
                    events_read,
                    vec![root],
                    "{case}: the earlier segment is folded once"
                );
            } else {
                assert_eq!(events_read, Vec::<ExecutionId>::new(), "{case}");
                assert_eq!(reads.executions.load(Ordering::SeqCst), 0, "{case}");
            }
        }
    }
}

/// Segment 0 spent 4 of 5 tokens. Segment 1's first step spends 2: the run
/// is past its cap across the two, and fails before `b`.
#[tokio::test]
async fn a_later_segment_counts_the_spend_of_the_segments_before_it() {
    for store in stores().await {
        let case = store.name();
        let backend = store.backend();
        let (_root, child) =
            second_segment(backend.as_ref(), token_capped(5, linear(&["a", "b"])), 4).await;
        let item = claim_one(backend.as_ref(), "a").await;
        spend(backend.as_ref(), &item, 2).await;

        let scheduler = Scheduler::new(backend.clone());
        let worker = Worker::default();
        for _ in 0..3 {
            scheduler.tick().await.unwrap();
            worker.complete_all(backend.as_ref(), |_| json!({})).await;
        }
        let events = backend.get_events(&child).await.unwrap();
        assert_eq!(
            trips(&events),
            vec![json!({
                "type": "token_budget_exceeded",
                "node_id": "a",
                "kind": "total_tokens",
                "limit": 5,
                "current": 6
            })],
            "{case}: {:?}",
            kinds(&events)
        );
        assert!(worker.items_of("b").is_empty(), "{case}: b was dispatched");
        assert_eq!(
            status(backend.as_ref(), &child).await,
            WorkflowStatus::Failed,
            "{case}"
        );
    }
}

/// An earlier segment's log holds an event this build cannot read, so a
/// capped run's spend cannot be known. The run is not advanced, and the
/// stall is recorded once.
#[tokio::test]
async fn an_unreadable_earlier_segment_holds_a_capped_run() {
    for store in stores().await {
        let case = store.name();
        let backend = store.backend();
        let (root, child) =
            second_segment(backend.as_ref(), token_capped(5, linear(&["a", "b"])), 1).await;
        backend
            .append_event(Event::new(root.clone(), 0, EventKind::Unknown))
            .await
            .unwrap();
        let item = claim_one(backend.as_ref(), "a").await;
        spend(backend.as_ref(), &item, 1).await;

        let scheduler = Scheduler::new(backend.clone());
        let worker = Worker::default();
        for _ in 0..3 {
            scheduler.tick().await.unwrap();
            worker.complete_all(backend.as_ref(), |_| json!({})).await;
        }
        let events = backend.get_events(&child).await.unwrap();
        assert!(worker.items_of("b").is_empty(), "{case}: b was dispatched");
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e.kind, EventKind::RunStalled { .. }))
                .count(),
            1,
            "{case}: {:?}",
            kinds(&events)
        );
        assert_eq!(
            status(backend.as_ref(), &child).await,
            WorkflowStatus::Running,
            "{case}"
        );
    }
}
