//! Two schedulers over one store, as two engine instances would run them.
//!
//! A scheduler folds a run's log and then records what it decided: a node
//! dispatched (its `NodeScheduled`, then its item), a dead branch skipped, a
//! rejected approval failed, the run finished. Two schedulers that fold the
//! same log decide the same things, and each must not record them a second
//! time.
//!
//! Every round here holds each scheduler right after its first read of the
//! log (`get_events_since`) until the other has read it too: a two-party
//! barrier, so the race is opened on purpose each round, not left to timing.
//! A scheduler that waits 2 s for the other goes on alone; that is counted,
//! never a failure. Each test checks the barrier met at least once.

mod support;

use jamjet_core::workflow::WorkflowStatus;
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::ApprovalDecision;
use jamjet_state::{Event, EventKind, InMemoryBackend};
use jamjet_testkit::{Hooks, Intercept, Op};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{
    kinds, linear, node, once_each, open_sqlite, per_node, scheduled, sqlite_file_url, start_run,
    status, step, workflow, Store, Worker,
};
use tokio::sync::Barrier;

const ROUNDS: usize = 6;

/// The barrier both schedulers meet at, renewed each round. A fresh one per
/// round, because a wait that timed out leaves its arrival counted in a
/// `tokio::sync::Barrier`, and the next round's single arrival would then go
/// through alone.
#[derive(Default)]
struct Rendezvous {
    barrier: Mutex<Option<Arc<Barrier>>>,
    armed: [AtomicBool; 2],
    met: AtomicUsize,
    timed_out: AtomicUsize,
}

impl Rendezvous {
    fn arm(&self) {
        *self.barrier.lock().unwrap() = Some(Arc::new(Barrier::new(2)));
        for armed in &self.armed {
            armed.store(true, Ordering::SeqCst);
        }
    }

    fn met(&self) -> usize {
        self.met.load(Ordering::SeqCst)
    }
}

/// One scheduler's hooks: after its first `get_events_since` of a round,
/// wait at the round's barrier.
struct MeetAfterRead {
    party: usize,
    rendezvous: Arc<Rendezvous>,
}

#[async_trait::async_trait]
impl Hooks for MeetAfterRead {
    async fn after(&self, op: &Op<'_>) {
        if !matches!(op, Op::GetEventsSince(_)) {
            return;
        }
        if !self.rendezvous.armed[self.party].swap(false, Ordering::SeqCst) {
            return;
        }
        let barrier = self.rendezvous.barrier.lock().unwrap().clone();
        let Some(barrier) = barrier else {
            return;
        };
        match tokio::time::timeout(Duration::from_secs(2), barrier.wait()).await {
            Ok(arrival) => {
                if arrival.is_leader() {
                    self.rendezvous.met.fetch_add(1, Ordering::SeqCst);
                }
            }
            Err(_) => {
                self.rendezvous.timed_out.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}

/// Two schedulers, each on its own `Intercept`, over one store.
struct Pair {
    schedulers: [Arc<Scheduler>; 2],
    rendezvous: Arc<Rendezvous>,
    /// The first scheduler's store, unwrapped: what the test reads and the
    /// worker writes through.
    store: Store,
}

impl Pair {
    fn over(backends: [Arc<dyn StateBackend>; 2], store: Store) -> Self {
        let rendezvous = Arc::new(Rendezvous::default());
        let [first, second] = backends;
        let scheduler = |party: usize, backend: Arc<dyn StateBackend>| {
            let hooks = Arc::new(MeetAfterRead {
                party,
                rendezvous: rendezvous.clone(),
            });
            Arc::new(Scheduler::new(Arc::new(Intercept::new(backend, hooks))))
        };
        Self {
            schedulers: [scheduler(0, first), scheduler(1, second)],
            rendezvous: rendezvous.clone(),
            store,
        }
    }

    /// Two pools on one SQLite file, as two processes would hold.
    async fn sqlite() -> Self {
        let url = sqlite_file_url();
        let first = open_sqlite(&url).await;
        let second = open_sqlite(&url).await;
        Self::over([first.clone(), second], Store::Sqlite(first))
    }

    /// One memory backend both schedulers share.
    fn memory() -> Self {
        let shared = Arc::new(InMemoryBackend::new());
        Self::over([shared.clone(), shared.clone()], Store::Memory(shared))
    }

    async fn both() -> Vec<Self> {
        vec![Self::sqlite().await, Self::memory()]
    }

    fn backend(&self) -> Arc<dyn StateBackend> {
        self.store.backend()
    }

    /// Both schedulers tick at once, each on its own task.
    async fn round(&self) {
        self.rendezvous.arm();
        let ticks: Vec<_> = self
            .schedulers
            .iter()
            .map(|s| {
                let s = s.clone();
                tokio::spawn(async move { s.tick().await.map_err(|e| e.to_string()) })
            })
            .collect();
        for tick in ticks {
            tick.await.unwrap().unwrap();
        }
    }

    /// Passes the store refused as stale, over both schedulers.
    fn stale_passes(&self) -> u64 {
        self.schedulers.iter().map(|s| s.stale_passes()).sum()
    }

    /// The race this file is about opened at least once: both schedulers
    /// read the log before either wrote, and the store refused at least one
    /// pass because the other's landed first.
    fn assert_the_race_opened(&self) {
        assert!(
            self.rendezvous.met() >= 1,
            "{}: the two schedulers never met after reading the log ({} waits timed out); \
             the test did not race them",
            self.store.name(),
            self.rendezvous.timed_out.load(Ordering::SeqCst)
        );
        assert!(
            self.stale_passes() >= 1,
            "{}: the store refused no pass as stale, so no two passes raced",
            self.store.name()
        );
    }
}

/// Run `ir` for `ROUNDS` rounds, the worker completing every item between
/// them. Returns the run's log.
async fn run_to_the_end(pair: &Pair, ir: serde_json::Value, worker: &Worker) -> Vec<Event> {
    let backend = pair.backend();
    let execution_id = start_run(backend.as_ref(), ir).await;
    for _ in 0..ROUNDS {
        pair.round().await;
        worker.complete_all(backend.as_ref(), |_| json!({})).await;
    }
    backend.get_events(&execution_id).await.unwrap()
}

async fn dispatch_each_node_once(pair: Pair) {
    let worker = Worker::default();
    let events = run_to_the_end(&pair, linear(&["a", "b", "c"]), &worker).await;
    pair.assert_the_race_opened();
    let name = pair.store.name();
    let log = kinds(&events);
    assert_eq!(
        per_node(&events, scheduled),
        once_each(&["a", "b", "c"]),
        "{name}: each node is scheduled once; items per node {:?}, log {log:?}",
        worker.items_per_node()
    );
    assert_eq!(
        worker.items_per_node(),
        once_each(&["a", "b", "c"]),
        "{name}: each node gets one work item"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, EventKind::WorkflowCompleted { .. })),
        "{name}: the run completes; log {log:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_schedulers_on_one_sqlite_file_dispatch_each_node_once() {
    dispatch_each_node_once(Pair::sqlite().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_schedulers_on_one_memory_backend_dispatch_each_node_once() {
    dispatch_each_node_once(Pair::memory()).await;
}

/// `gate` routes to `taken`; `not_taken` and the node after it are a dead
/// branch the scheduler records as skipped, once each.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_schedulers_skip_a_dead_branch_once() {
    let condition = "state.go == \"taken\"";
    let ir = workflow(
        "gate",
        vec![
            node(
                "gate",
                json!({ "type": "condition", "branches": [
                    { "condition": condition, "target": "taken" },
                    { "condition": null, "target": "not_taken" }
                ] }),
                None,
            ),
            step("taken"),
            step("not_taken"),
            step("not_taken_tail"),
        ],
        &[
            ("gate", "taken", Some(condition)),
            ("gate", "not_taken", None),
            ("not_taken", "not_taken_tail", None),
        ],
    );
    for pair in Pair::both().await {
        let backend = pair.backend();
        let execution_id = start_run(backend.as_ref(), ir.clone()).await;
        let worker = Worker::default();
        for _ in 0..ROUNDS {
            pair.round().await;
            worker
                .complete_all(backend.as_ref(), |node| match node {
                    "gate" => json!({ "chosen_target": "taken" }),
                    _ => json!({}),
                })
                .await;
        }
        let events = backend.get_events(&execution_id).await.unwrap();
        pair.assert_the_race_opened();
        let skipped = per_node(&events, |k| match k {
            EventKind::NodeSkipped { node_id, .. } => Some(node_id),
            _ => None,
        });
        assert_eq!(
            skipped,
            once_each(&["not_taken", "not_taken_tail"]),
            "{}: each node of the dead branch is skipped once; log {:?}",
            pair.store.name(),
            kinds(&events)
        );
    }
}

/// A rejected approval fails its node: one `NodeFailed`, however many
/// schedulers fold the rejection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_schedulers_fail_a_rejected_node_once() {
    let ir = workflow(
        "review",
        vec![node(
            "review",
            json!({ "type": "human_approval", "description": "review the change",
                    "timeout_secs": null, "fallback_node": null }),
            None,
        )],
        &[],
    );
    for pair in Pair::both().await {
        let backend = pair.backend();
        let execution_id = start_run(backend.as_ref(), ir.clone()).await;
        // The worker holds the node for a decision, as the approval guard
        // does: it records the request and settles its item.
        let worker = Worker::default();
        for item in worker.claim_all(backend.as_ref()).await {
            backend
                .append_event(Event::new(
                    execution_id.clone(),
                    0,
                    EventKind::ToolApprovalRequired {
                        node_id: item.node_id.clone(),
                        tool_name: "review".into(),
                        approver: "human".into(),
                        context: json!({}),
                    },
                ))
                .await
                .unwrap();
            assert!(backend
                .complete_work_item_fenced(item.id, item.lease_fence)
                .await
                .unwrap());
        }
        backend
            .append_event(Event::new(
                execution_id.clone(),
                0,
                EventKind::ApprovalReceived {
                    node_id: "review".into(),
                    user_id: "reviewer".into(),
                    decision: ApprovalDecision::Rejected,
                    comment: Some("not this one".into()),
                    state_patch: None,
                    principal: None,
                    approved_digest: None,
                    request_sequence: None,
                },
            ))
            .await
            .unwrap();
        for _ in 0..ROUNDS {
            pair.round().await;
        }
        let events = backend.get_events(&execution_id).await.unwrap();
        pair.assert_the_race_opened();
        let failed = per_node(&events, |k| match k {
            EventKind::NodeFailed {
                node_id,
                retryable: false,
                ..
            } => Some(node_id),
            _ => None,
        });
        assert_eq!(
            failed,
            once_each(&["review"]),
            "{}: the rejected node fails once; log {:?}",
            pair.store.name(),
            kinds(&events)
        );
        assert_eq!(
            status(backend.as_ref(), &execution_id).await,
            WorkflowStatus::Failed
        );
    }
}

/// Guard, green before the one-write pass: the finish is already one write
/// that refuses a run that is terminal, so two schedulers that both see the
/// run done record one terminal event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_schedulers_finish_a_run_once() {
    for pair in Pair::both().await {
        let backend = pair.backend();
        let execution_id = start_run(backend.as_ref(), linear(&["a", "b", "c"])).await;
        let worker = Worker::default();
        let mut finished_in_a_raced_round = None;
        for _ in 0..ROUNDS {
            let met_before = pair.rendezvous.met();
            let was_running =
                status(backend.as_ref(), &execution_id).await == WorkflowStatus::Running;
            pair.round().await;
            if was_running && status(backend.as_ref(), &execution_id).await.is_terminal() {
                finished_in_a_raced_round = Some(pair.rendezvous.met() > met_before);
            }
            worker.complete_all(backend.as_ref(), |_| json!({})).await;
        }
        let name = pair.store.name();
        assert_eq!(
            finished_in_a_raced_round,
            Some(true),
            "{name}: the run must finish in a round where both schedulers read it"
        );
        pair.assert_the_race_opened();
        let events = backend.get_events(&execution_id).await.unwrap();
        let terminal = events
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    EventKind::WorkflowCompleted { .. }
                        | EventKind::WorkflowFailed { .. }
                        | EventKind::WorkflowCancelled { .. }
                )
            })
            .count();
        assert_eq!(
            terminal,
            1,
            "{name}: one terminal event; log {:?}",
            kinds(&events)
        );
        assert_eq!(
            status(backend.as_ref(), &execution_id).await,
            WorkflowStatus::Completed
        );
    }
}
