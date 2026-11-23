//! The approvals projector with more than one engine on one store, and with
//! more running runs than one page holds.
//!
//! Two projectors over one store each read a run's log, fold it and write
//! the rows and the checkpoint. One that read the log before a decision
//! landed writes an older view than one that read it after. Its batch must
//! not move a row or a checkpoint back.
//!
//! Each test runs on memory, on a SQLite file through two pools and, when
//! `JAMJET_TEST_POSTGRES_URL` is set, on a Postgres database through two
//! pools.

use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_scheduler::Projector;
use jamjet_state::backend::{ApprovalProjectionRow, StateBackend};
use jamjet_state::event::ApprovalDecision;
use jamjet_state::{Event, EventKind, InMemoryBackend};
use jamjet_testkit::conformance::{
    Fixture, MemoryFactory, PostgresFactory, SqliteFactory, StoreFactory,
};
use jamjet_testkit::{Hooks, Intercept, Op};
use serde_json::json;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// The stores each test runs on, by name. Postgres is left out when it is
/// not configured.
async fn fixtures() -> Vec<(&'static str, Fixture)> {
    let mut out = Vec::new();
    let factories: [(&'static str, &dyn StoreFactory); 3] = [
        ("memory", &MemoryFactory),
        ("sqlite", &SqliteFactory),
        ("postgres", &PostgresFactory),
    ];
    for (name, factory) in factories {
        match factory.fresh().await {
            Some(fx) => out.push((name, fx)),
            None => jamjet_testkit::skip(&format!("projector on {name}")),
        }
    }
    out
}

fn running(id: &ExecutionId) -> WorkflowExecution {
    let now = chrono::Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf-projector".into(),
        workflow_version: "1.0.0".into(),
        status: WorkflowStatus::Running,
        initial_input: json!({}),
        current_state: json!({}),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    }
}

fn request(id: &ExecutionId) -> Event {
    Event::new(
        id.clone(),
        0,
        EventKind::ToolApprovalRequired {
            node_id: "review".into(),
            tool_name: "send_report".into(),
            approver: "human".into(),
            context: json!({ "doc": "q3" }),
        },
    )
}

fn decision(id: &ExecutionId, approve: bool) -> Event {
    Event::new(
        id.clone(),
        0,
        EventKind::ApprovalReceived {
            node_id: "review".into(),
            user_id: "alice".into(),
            decision: if approve {
                ApprovalDecision::Approved
            } else {
                ApprovalDecision::Rejected
            },
            comment: Some("checked".into()),
            state_patch: None,
            principal: None,
            approved_digest: None,
            request_sequence: None,
            choice: None,
        },
    )
}

/// A running run with one approval request, and the request's sequence.
async fn run_with_a_request(b: &dyn StateBackend) -> (ExecutionId, i64) {
    let id = ExecutionId::new();
    b.create_execution(running(&id)).await.unwrap();
    let at = b.append_event(request(&id)).await.unwrap();
    (id, at)
}

/// The rows and the checkpoint the store holds for `id`, each sequence
/// given as the place of its event in the run's log, from 1. The memory
/// store numbers events across all runs, so its sequences differ from
/// another store's for the same log; the places do not.
async fn projected(b: &dyn StateBackend, id: &ExecutionId) -> (Vec<ApprovalProjectionRow>, i64) {
    let log: Vec<i64> = b
        .get_events(id)
        .await
        .unwrap()
        .iter()
        .map(|e| e.sequence)
        .collect();
    let place = |seq: i64| match log.iter().position(|s| *s == seq) {
        Some(i) => i as i64 + 1,
        None => -seq,
    };
    let rows = b
        .get_approval_projection(id)
        .await
        .unwrap()
        .into_iter()
        .map(|mut r| {
            r.last_sequence = place(r.last_sequence);
            r
        })
        .collect();
    let checkpoint = b.get_projector_checkpoint("approvals", id).await.unwrap();
    (
        rows,
        if checkpoint == 0 {
            0
        } else {
            place(checkpoint)
        },
    )
}

/// The behind projector's hooks. Right after it reads a run's log, a
/// decision lands on that run and the other projector projects it, so the
/// behind one then writes a view older than the store's.
struct DecideAfterRead {
    store: Arc<dyn StateBackend>,
    ahead: Projector,
    /// The run the ahead projector may read now; it fails every other, so
    /// it projects only the run the behind one is on.
    current: Arc<Mutex<Option<ExecutionId>>>,
    approve: Mutex<HashSet<ExecutionId>>,
    /// Rounds where the ahead projector wrote the decision first.
    overtaken: AtomicUsize,
    /// Batches the behind projector sent after it was overtaken.
    stale_batches: AtomicUsize,
}

#[async_trait::async_trait]
impl Hooks for DecideAfterRead {
    async fn after(&self, op: &Op<'_>) {
        match op {
            Op::GetEventsSince(id) => {
                let id = (*id).clone();
                let approve = self.approve.lock().unwrap().contains(&id);
                self.store
                    .append_event(decision(&id, approve))
                    .await
                    .unwrap();
                *self.current.lock().unwrap() = Some(id);
                let written = self.ahead.tick().await.unwrap();
                *self.current.lock().unwrap() = None;
                if written == 1 {
                    self.overtaken.fetch_add(1, Ordering::SeqCst);
                }
            }
            Op::ApplyApprovalProjectionBatch { .. } => {
                self.stale_batches.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
    }
}

/// Fails the ahead projector's reads of every run but the current one.
struct OnlyCurrent(Arc<Mutex<Option<ExecutionId>>>);

#[async_trait::async_trait]
impl Hooks for OnlyCurrent {
    async fn before(&self, op: &Op<'_>) -> Option<jamjet_state::backend::StateBackendError> {
        let Op::GetProjectorCheckpoint { execution_id, .. } = op else {
            return None;
        };
        let current = self.0.lock().unwrap().clone();
        (current.as_ref() != Some(*execution_id)).then(|| {
            jamjet_state::backend::StateBackendError::Database("not this run's turn".into())
        })
    }
}

/// Two projectors on one store, 50 runs with a decision each. One projector
/// reads each run's log before its decision lands and writes after the
/// other has projected the decision. The rows and checkpoints end equal to
/// what one projector makes of the same logs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_projectors_converge() {
    const RUNS: usize = 50;
    let mut failed = Vec::new();
    for (name, fx) in fixtures().await {
        let store = fx.backend.clone();
        let second = fx.second.clone().expect("a second handle");
        let mut ids = Vec::new();
        for _ in 0..RUNS {
            ids.push(run_with_a_request(&*store).await.0);
        }
        let current = Arc::new(Mutex::new(None));
        let ahead = Projector::new(Arc::new(Intercept::new(
            second,
            Arc::new(OnlyCurrent(current.clone())),
        )));
        let hooks = Arc::new(DecideAfterRead {
            store: store.clone(),
            ahead,
            current,
            approve: Mutex::new(ids.iter().step_by(2).cloned().collect()),
            overtaken: AtomicUsize::new(0),
            stale_batches: AtomicUsize::new(0),
        });
        let behind = Projector::new(Arc::new(Intercept::new(store.clone(), hooks.clone())));
        behind.tick().await.unwrap();

        // The race opened on every run: the ahead projector wrote the
        // decision first, then the behind one sent its older batch.
        assert_eq!(hooks.overtaken.load(Ordering::SeqCst), RUNS, "{name}");
        assert_eq!(hooks.stale_batches.load(Ordering::SeqCst), RUNS, "{name}");

        // One projector over the same logs.
        let reference = Arc::new(InMemoryBackend::new());
        for id in &ids {
            reference.create_execution(running(id)).await.unwrap();
            for event in store.get_events(id).await.unwrap() {
                reference
                    .append_event(Event::new(id.clone(), 0, event.kind))
                    .await
                    .unwrap();
            }
        }
        Projector::new(reference.clone()).tick().await.unwrap();

        let mut differ = Vec::new();
        for id in &ids {
            let want = projected(&*reference, id).await;
            assert_eq!(want.1, 2, "{name}: the reference read both events");
            assert_ne!(want.0[0].status, "pending", "{name}: the reference decided");
            let got = projected(&*store, id).await;
            if got != want {
                differ.push(format!("{id}: {got:?} where one projector makes {want:?}"));
            }
        }
        if let Some(first) = differ.first() {
            failed.push(format!(
                "{name}: {} of {RUNS} runs differ from one projector's result; the first:\n{first}",
                differ.len()
            ));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// The projector's hooks: once it has written 100 runs, 50 of them finish,
/// as the scheduler finishes runs while the projector walks.
struct FinishSomeMidWalk {
    store: Arc<dyn StateBackend>,
    written: Mutex<Vec<ExecutionId>>,
    finished: AtomicUsize,
}

#[async_trait::async_trait]
impl Hooks for FinishSomeMidWalk {
    async fn after(&self, op: &Op<'_>) {
        let Op::ApplyApprovalProjectionBatch { execution_id, .. } = op else {
            return;
        };
        let finish = {
            let mut written = self.written.lock().unwrap();
            written.push((*execution_id).clone());
            if written.len() == 100 {
                written[..50].to_vec()
            } else {
                Vec::new()
            }
        };
        for id in finish {
            self.store
                .update_execution_status(&id, WorkflowStatus::Completed)
                .await
                .unwrap();
            self.finished.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// 600 running runs, each with a request: one tick projects every one,
/// though 50 runs it has already written finish during the walk. Paging by
/// offset over the running runs skipped as many runs as had left the set
/// behind the walk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn walks_every_running_run_past_one_page() {
    const RUNS: usize = 600;
    let mut failed = Vec::new();
    for (name, fx) in fixtures().await {
        let store = fx.backend.clone();
        let mut runs = Vec::new();
        for _ in 0..RUNS {
            runs.push(run_with_a_request(&*store).await);
        }
        let hooks = Arc::new(FinishSomeMidWalk {
            store: store.clone(),
            written: Mutex::new(Vec::new()),
            finished: AtomicUsize::new(0),
        });
        let projector = Projector::new(Arc::new(Intercept::new(store.clone(), hooks.clone())));
        projector.tick().await.unwrap();

        assert_eq!(
            hooks.finished.load(Ordering::SeqCst),
            50,
            "{name}: runs finished during the walk"
        );
        let mut missed = 0;
        for (id, at) in &runs {
            let rows = store.get_approval_projection(id).await.unwrap();
            let checkpoint = store
                .get_projector_checkpoint("approvals", id)
                .await
                .unwrap();
            if checkpoint != *at || rows.len() != 1 {
                missed += 1;
            }
        }
        if missed > 0 {
            failed.push(format!(
                "{name}: {missed} of {RUNS} runs were not projected"
            ));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}
