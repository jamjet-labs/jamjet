//! A store that refuses one write in the middle of a scheduler step.
//!
//! A step that takes two writes leaves half of itself when the second write
//! fails (a busy database, a crash), and nothing later repairs it: a node
//! scheduled with no item, a dead-lettered item with no failure recorded, a
//! failure with no retry. The scheduler's dispatch is one write now (its
//! pass). The sweep still settles an expired item and then appends the
//! events that record it.
//!
//! The store refuses the chosen call once (`Intercept`'s `before` hook), and
//! each test checks the refusal happened.

mod support;

use jamjet_core::workflow::WorkflowStatus;
use jamjet_scheduler::Scheduler;
use jamjet_state::{EventKind, StateBackendError};
use jamjet_testkit::{Hooks, Intercept, Op};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use support::{
    kinds, linear, node, once_each, per_node, scheduled, status, stores, workflow, Worker,
};

/// Refuses the first call `pick` matches, once, and counts the calls
/// `watch` matches.
struct FailOnce {
    pick: fn(&Op<'_>) -> bool,
    fired: AtomicUsize,
    watch: fn(&Op<'_>) -> bool,
    watched: AtomicUsize,
}

impl FailOnce {
    fn new(pick: fn(&Op<'_>) -> bool) -> Arc<Self> {
        Self::watching(pick, |_| false)
    }

    fn watching(pick: fn(&Op<'_>) -> bool, watch: fn(&Op<'_>) -> bool) -> Arc<Self> {
        Arc::new(Self {
            pick,
            fired: AtomicUsize::new(0),
            watch,
            watched: AtomicUsize::new(0),
        })
    }

    fn fired(&self) -> usize {
        self.fired.load(Ordering::SeqCst)
    }

    fn watched(&self) -> usize {
        self.watched.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Hooks for FailOnce {
    async fn before(&self, op: &Op<'_>) -> Option<StateBackendError> {
        if (self.watch)(op) {
            self.watched.fetch_add(1, Ordering::SeqCst);
        }
        if (self.pick)(op)
            && self
                .fired
                .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            return Some(StateBackendError::Database("refused by the test".into()));
        }
        None
    }
}

const TICKS: usize = 20;

/// The dispatch was a `NodeScheduled` append and then an enqueue, and a
/// failed enqueue stranded the node. Now both are in the scheduler's one
/// pass, and the scheduler neither enqueues nor appends on its own, so there
/// is no second write left to fail. The store refuses the pass instead:
/// nothing of it lands, and a later tick dispatches the node again.
#[tokio::test]
async fn a_failed_enqueue_after_node_scheduled_does_not_strand_the_node() {
    for store in stores().await {
        let name = store.name();
        let backend = store.backend();
        let hook = FailOnce::watching(
            |op| matches!(op, Op::CommitSchedulerPass(_)),
            |op| {
                matches!(
                    op,
                    Op::EnqueueWorkItem(_) | Op::AppendEvent(_) | Op::FinishExecution { .. }
                )
            },
        );
        let scheduler = Scheduler::new(Arc::new(Intercept::new(backend.clone(), hook.clone())));
        let execution_id = support::start_run(backend.as_ref(), linear(&["a", "b", "c"])).await;
        let worker = Worker::default();
        for _ in 0..TICKS {
            scheduler.tick().await.unwrap();
            worker.complete_all(backend.as_ref(), |_| json!({})).await;
            if status(backend.as_ref(), &execution_id).await.is_terminal() {
                break;
            }
        }
        assert_eq!(hook.fired(), 1, "{name}: the store refused one pass");
        assert_eq!(
            hook.watched(),
            0,
            "{name}: the scheduler wrote an event, an item or a finish outside its passes"
        );
        let events = backend.get_events(&execution_id).await.unwrap();
        let state = status(backend.as_ref(), &execution_id).await;
        assert_eq!(
            state,
            WorkflowStatus::Completed,
            "{name}: the run ends within {TICKS} ticks; scheduled per node {:?}, \
             items per node {:?}, log {:?}",
            per_node(&events, scheduled),
            worker.items_per_node(),
            kinds(&events)
        );
        assert_eq!(
            worker.items_per_node(),
            once_each(&["a", "b", "c"]),
            "{name}: every node has one item"
        );
        assert_eq!(
            per_node(&events, scheduled),
            once_each(&["a", "b", "c"]),
            "{name}: every node is scheduled once; log {:?}",
            kinds(&events)
        );
    }
}

/// One attempt only: an expired lease dead-letters the item, and the sweep
/// records `NodeFailed { retryable: false }`, which fails the run.
#[tokio::test]
async fn a_failed_lease_event_after_dead_letter_does_not_strand_the_node() {
    let ir = workflow(
        "a",
        vec![node(
            "a",
            json!({ "type": "condition", "branches": [] }),
            Some("no_retry"),
        )],
        &[],
    );
    for store in stores().await {
        let name = store.name();
        let backend = store.backend();
        let hook = FailOnce::new(
            |op| matches!(op, Op::AppendEvent(e) if matches!(e.kind, EventKind::NodeFailed { .. })),
        );
        let scheduler = Scheduler::new(Arc::new(Intercept::new(backend.clone(), hook.clone())));
        let execution_id = support::start_run(backend.as_ref(), ir.clone()).await;
        let held = Worker::default().claim_all(backend.as_ref()).await;
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].max_attempts, 1, "{name}: a one-attempt node");
        store.expire_lease(held[0].id).await;
        // The sweep runs before any claim: a claim's own reclaim never
        // takes an item that has no attempt left, but the order is fixed
        // anyway so the sweep is what this test exercises.
        for _ in 0..TICKS {
            scheduler.tick().await.unwrap();
            if status(backend.as_ref(), &execution_id).await.is_terminal() {
                break;
            }
        }
        assert_eq!(
            hook.fired(),
            1,
            "{name}: the store refused the sweep's NodeFailed"
        );
        let events = backend.get_events(&execution_id).await.unwrap();
        let failed = per_node(&events, |k| match k {
            EventKind::NodeFailed {
                node_id,
                retryable: false,
                ..
            } => Some(node_id),
            _ => None,
        });
        let state = status(backend.as_ref(), &execution_id).await;
        assert!(
            state == WorkflowStatus::Failed && failed == once_each(&["a"]),
            "{name}: the run fails with one NodeFailed {{ retryable: false }}; \
             status {state:?}, log {:?}",
            kinds(&events)
        );
    }
}

/// Three attempts: an expired lease puts the item back, and the sweep
/// records `NodeFailed { retryable: true }` then `RetryScheduled`.
#[tokio::test]
async fn a_failed_retry_event_after_requeue_is_never_half_recorded() {
    let ir = workflow(
        "a",
        vec![node(
            "a",
            json!({ "type": "condition", "branches": [] }),
            None,
        )],
        &[],
    );
    for store in stores().await {
        let name = store.name();
        let backend = store.backend();
        let hook = FailOnce::new(
            |op| matches!(op, Op::AppendEvent(e) if matches!(e.kind, EventKind::RetryScheduled { .. })),
        );
        let scheduler = Scheduler::new(Arc::new(Intercept::new(backend.clone(), hook.clone())));
        let execution_id = support::start_run(backend.as_ref(), ir.clone()).await;
        let held = Worker::default().claim_all(backend.as_ref()).await;
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].max_attempts, 3, "{name}: a three-attempt node");
        store.expire_lease(held[0].id).await;
        // The sweep runs before any claim, so the claim's own reclaim
        // (which records its own trail in one write) does not take the item
        // first.
        for _ in 0..3 {
            scheduler.tick().await.unwrap();
        }
        assert_eq!(
            hook.fired(),
            1,
            "{name}: the store refused the sweep's RetryScheduled"
        );
        let events = backend.get_events(&execution_id).await.unwrap();
        let failures = events
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    EventKind::NodeFailed {
                        retryable: true,
                        ..
                    }
                )
            })
            .count();
        assert!(failures >= 1, "{name}: the sweep recorded the lapse");
        for (i, event) in events.iter().enumerate() {
            let EventKind::NodeFailed {
                node_id,
                retryable: true,
                ..
            } = &event.kind
            else {
                continue;
            };
            let next = events.get(i + 1).map(|e| &e.kind);
            assert!(
                matches!(next, Some(EventKind::RetryScheduled { node_id: n, .. }) if n == node_id),
                "{name}: NodeFailed {{ retryable: true }} at {} is not followed by its \
                 RetryScheduled; log {:?}",
                event.sequence,
                kinds(&events)
            );
        }
    }
}
