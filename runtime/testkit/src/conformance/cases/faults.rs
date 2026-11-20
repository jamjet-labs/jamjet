//! A transaction stopped between any two of its statements writes nothing,
//! and the same call made again lands once.
//!
//! Each point in `jamjet_state::faults::POINTS` belongs to one operation and
//! one or more stores. On each store, every point its code holds is failed in
//! turn: the store is set up so the operation reaches the point, an error is
//! armed there, and the operation is called. The call must fail with that
//! error and leave every row of the store as it was. Then the same call is
//! made again with nothing armed, and the rows it changed must be what one
//! call of it writes. Each call runs in a [`Scope`] of its own, so the fault
//! never reaches the other cases running in the same binary.
//!
//! The point right before a body's last commit is failed on a setup that
//! runs every statement of the body, or once per path where the paths to
//! the commit each run a statement the others do not. So a write that a
//! commit split off anywhere in the body, in a branch or in a loop's later
//! passes, is still there when the error stops the call.
//!
//! The hang case holds one run's scheduler pass inside its transaction and
//! makes another run's pass meanwhile. SQLite has one writer per file, so
//! the other pass waits for the hang to end and lands within the busy
//! timeout. The Postgres half, where the other pass commits during the
//! hang, comes with the Postgres backend; until then the case fails on
//! Postgres rather than pass there unrun. Both cases skip memory only.

use crate::conformance::cases::support::skipped;
use crate::conformance::{Fixture, StoreKind, StoreRows};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::faults::{self, Fault, Scope, POINTS};
use jamjet_state::{
    ApprovalProjectionRow, BackendResult, ConditionalAppend, Event, EventKind, ExecutionFinish,
    FailOutcome, KeyedStart, PassCommit, RunKeyClaim, SchedulerPass, Snapshot, StateBackend,
    StateBackendError, TurnCommit, WorkItem, DEFAULT_TENANT, MAX_LEASE_SECS,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;
use StoreKind::{Sqlite, SqliteScoped};

/// The worker every claim in this module is made for.
const WORKER: &str = "worker-1";

/// The first part of the labels of points inside a store's own
/// transactions, which are this module's: `pg` is the Postgres backend's.
/// Points elsewhere, at seams between components, are tested by the tests
/// that drive those components.
const STORE_CODE: &[&str] = &["sqlite", "sqlite_scoped", "pg"];

/// The stores whose code holds a label's points. A store's label missing
/// here fails the error case on every store: a point no operation reaches
/// tests nothing.
fn stores_of(label: &str) -> Option<&'static [StoreKind]> {
    const UNSCOPED: &[StoreKind] = &[Sqlite];
    const SCOPED: &[StoreKind] = &[SqliteScoped];
    const BOTH: &[StoreKind] = &[Sqlite, SqliteScoped];
    Some(match label {
        "sqlite.commit_turn"
        | "sqlite.append_event"
        | "sqlite.claim_work_item"
        | "sqlite.fail_work_item_fenced"
        | "sqlite.finalize_rollover_fenced"
        | "sqlite.apply_approval_projection"
        | "sqlite.apply_approval_projection_batch" => UNSCOPED,
        "sqlite_scoped.commit_turn"
        | "sqlite_scoped.append_event"
        | "sqlite_scoped.claim_work_item"
        | "sqlite_scoped.fail_work_item_fenced"
        | "sqlite_scoped.finalize_rollover_fenced"
        | "sqlite_scoped.apply_approval_projection"
        | "sqlite_scoped.apply_approval_projection_batch" => SCOPED,
        "sqlite.create_run"
        | "sqlite.finish_execution"
        | "sqlite.append_event_if"
        | "sqlite.expire_stale_leases"
        | "sqlite.reattach_lease"
        | "sqlite.commit_scheduler_pass"
        | "sqlite.reclaim_expired_leases_recorded"
        | "sqlite.fail_work_item_fenced_recorded"
        | "sqlite.patch_append_array" => BOTH,
        _ => return None,
    })
}

pub async fn an_error_at_any_step_leaves_no_partial_write(fx: &Fixture) {
    assert!(
        faults::enabled(),
        "jamjet-state is built without its test-hooks feature, so no point can fire"
    );
    let unowned: Vec<_> = POINTS
        .iter()
        .filter(|(label, _)| {
            let store = label.split_once('.').map_or(*label, |(store, _)| store);
            STORE_CODE.contains(&store) && stores_of(label).is_none()
        })
        .collect();
    assert!(
        unowned.is_empty(),
        "fault points in a store's code with no operation in this case: {unowned:?}"
    );
    let points: Vec<(&'static str, u32)> = POINTS
        .iter()
        .copied()
        .filter(|(label, _)| stores_of(label).is_some_and(|kinds| kinds.contains(&fx.kind)))
        .collect();
    if points.is_empty() {
        // Only memory has no transactions to stop. Any other store with no
        // point here would pass this case without failing one.
        assert_eq!(
            fx.kind,
            StoreKind::Memory,
            "{:?}: no point in faults::POINTS belongs to this store, so this case would \
             stop none of its transactions: give its labels their stores in stores_of",
            fx.kind
        );
        return skipped(fx, "memory has no transactions to stop");
    }

    for (label, step) in points {
        let (_, operation) = label
            .split_once('.')
            .expect("a label is <store>.<operation>");
        let last = step == last_step(label);
        let paths = paths(operation, last);
        for path in 0..paths {
            let point = if paths > 1 {
                format!("{label}#{step} (path {} of {paths})", path + 1)
            } else {
                format!("{label}#{step}")
            };
            let at = At { step, last, path };
            let call = prepare(fx, operation, at).await;
            let before = every_row(fx).await;

            let scope = Scope::new();
            scope.arm(label, step, Fault::Error);
            let failed = scope.run((call.run)()).await;
            let injected = format!("fault injected at {label}#{step}");
            match &failed {
                Err(StateBackendError::Database(message)) if *message == injected => {}
                other => panic!("{point}: the call did not stop there, it returned {other:?}"),
            }
            assert_eq!(scope.fired(label, step), 1, "{point}: fired once");
            let after_failure = every_row(fx).await;
            let partial = changes(&before, &after_failure);
            assert!(
                partial.is_empty(),
                "{point}: the failed call left a partial write: {partial:#?}"
            );

            (call.run)()
                .await
                .unwrap_or_else(|e| panic!("{point}: the same call made again: {e}"));
            let landed = changes(&before, &every_row(fx).await);
            let counts: BTreeMap<String, (usize, usize)> = landed
                .iter()
                .map(|(table, change)| (table.clone(), (change.gone.len(), change.new.len())))
                .collect();
            let expected: BTreeMap<String, (usize, usize)> = call
                .lands
                .iter()
                .map(|(table, gone, new)| (table.to_string(), (*gone, *new)))
                .collect();
            assert_eq!(
                counts, expected,
                "{point}: the call made again did not write what one call writes \
                 (rows gone, rows new, per table): {landed:#?}"
            );
        }
    }
}

/// The highest step of `label`: the point right before its body's last
/// commit.
fn last_step(label: &str) -> u32 {
    POINTS
        .iter()
        .filter(|(l, _)| *l == label)
        .map(|(_, s)| *s)
        .max()
        .expect("a label in POINTS has a step")
}

/// How many setups a point is failed on: one, or for the point before the
/// last commit of a body whose paths to that commit each run a statement the
/// other does not, one per path.
fn paths(operation: &str, last: bool) -> u32 {
    match operation {
        // Retried or out of attempts: the requeue, or the dead letter.
        "reclaim_expired_leases_recorded" | "fail_work_item_fenced_recorded" if last => 2,
        // A segment's seed snapshot, a fresh run key, or a key moved from
        // the run it named.
        "create_run" if last => 3,
        _ => 1,
    }
}

/// Which point a setup is for: its step, whether it is the point before the
/// body's last commit, and which of that point's [`paths`] to take.
#[derive(Debug, Clone, Copy)]
struct At {
    step: u32,
    last: bool,
    path: u32,
}

pub async fn a_hang_inside_a_transaction_blocks_only_its_run(fx: &Fixture) {
    const LABEL: &str = "sqlite.commit_scheduler_pass";
    const STEP: u32 = 2;
    // Shorter than the SQLite busy timeout, so the waiting pass lands.
    const HANG: Duration = Duration::from_secs(2);
    match fx.kind {
        Sqlite | SqliteScoped => {}
        StoreKind::Memory => return skipped(fx, "memory has no transactions to hold"),
        StoreKind::Postgres | StoreKind::PostgresScoped => panic!(
            "{:?}: the Postgres half of this case is not written, so it would pass here \
             without holding a transaction; it comes with the Postgres backend",
            fx.kind
        ),
    }
    assert!(
        faults::enabled(),
        "jamjet-state is built without its test-hooks feature, so no point can fire"
    );
    let Some(second) = fx.second.clone() else {
        return skipped(fx, "the store has no second handle");
    };
    let b = fx.backend.clone();
    let (slow_run, slow_pass) = dispatch_pass(&b).await;
    let (other_run, other_pass) = dispatch_pass(&b).await;

    // The slow pass hangs after its second statement, inside its
    // transaction: on SQLite it holds the file's write lock meanwhile.
    let scope = Scope::new();
    scope.arm(LABEL, STEP, Fault::Hang(HANG));
    let slow = tokio::spawn({
        let (scope, b) = (scope.clone(), b.clone());
        async move {
            let start = Instant::now();
            let out = scope.run(b.commit_scheduler_pass(slow_pass)).await;
            (out, start.elapsed())
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while scope.fired(LABEL, STEP) == 0 {
        assert!(
            Instant::now() < deadline,
            "the slow pass never reached {LABEL}#{STEP}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Another run's pass, through the other pool, as a second engine
    // would make it, outside the scope.
    let other_start = Instant::now();
    let other = second.commit_scheduler_pass(other_pass).await;
    let other_took = other_start.elapsed();
    let (slow, slow_took) = slow.await.expect("the slow pass's task");

    assert!(
        matches!(slow, Ok(PassCommit::Committed { .. })),
        "the slow pass: {slow:?}"
    );
    assert!(slow_took >= HANG, "the slow pass hung: {slow_took:?}");
    assert_eq!(
        scope.fired(LABEL, STEP),
        1,
        "the slow pass reached the point once"
    );
    assert!(
        matches!(other, Ok(PassCommit::Committed { .. })),
        "the other pass lands within the busy timeout: {other:?} after {other_took:?}"
    );
    // It started while the slow pass held the write lock, so it waited
    // for the hang to end: the two passes did meet.
    assert!(
        other_took >= HANG / 2,
        "the other pass waited for the slow one: {other_took:?}"
    );
    for run in [&slow_run, &other_run] {
        assert_eq!(
            b.latest_sequence(run).await.unwrap(),
            3,
            "{run}: the pass landed"
        );
    }
}

// ── The operations that own the points ───────────────────────────────────────

type Outcome = Pin<Box<dyn Future<Output = BackendResult<()>> + Send>>;

/// One call of the operation that owns a label, on a store set up so the
/// call reaches a given step.
struct Call {
    /// Makes the call, afresh each time, with the same arguments. It fails
    /// with the backend's error, and panics if it lands with an answer other
    /// than the one the setup leads to.
    run: Box<dyn Fn() -> Outcome + Send + Sync>,
    /// What one call that lands writes, per table: the rows it changes or
    /// removes (as they were), and the rows it changes or adds (as they are
    /// after). Every other table is left as it was.
    lands: &'static [(&'static str, usize, usize)],
}

fn call<F, Fut>(f: F) -> Box<dyn Fn() -> Outcome + Send + Sync>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = BackendResult<()>> + Send + 'static,
{
    Box::new(move || Box::pin(f()))
}

/// Set up the store so `operation` reaches the point `at`, and return the
/// call. Where a step lies on one branch of the operation, the setup takes
/// that branch. For the point before the last commit, the setup runs every
/// statement of the body, or takes the path `at` names.
async fn prepare(fx: &Fixture, operation: &str, at: At) -> Call {
    let b = fx.backend.clone();
    let step = at.step;
    match operation {
        "commit_turn" if step <= 2 => {
            // The run is over: the commit withdraws the item, records nothing.
            let (run, item) = claimed(&b, 0, 3).await;
            cancel(&b, &run).await;
            let event = completed(&run, None);
            Call {
                run: call(move || {
                    let (b, event) = (b.clone(), event.clone());
                    async move {
                        let commit = b
                            .commit_turn_unless_terminal(item.id, item.lease_fence, event, false)
                            .await?;
                        assert!(
                            matches!(commit, TurnCommit::ExecutionTerminal(_)),
                            "{commit:?}"
                        );
                        Ok(())
                    }
                }),
                lands: &[("work_items", 1, 1)],
            }
        }
        "commit_turn" => {
            // With an idempotency key and a snapshot, and no snapshot before
            // it, so the commit reads the run's input.
            let (run, item) = claimed(&b, 0, 3).await;
            let event = completed(&run, Some(format!("key-{}", Uuid::new_v4())));
            Call {
                run: call(move || {
                    let (b, event) = (b.clone(), event.clone());
                    async move {
                        let commit = b
                            .commit_turn_unless_terminal(item.id, item.lease_fence, event, true)
                            .await?;
                        assert!(matches!(commit, TurnCommit::Committed(_)), "{commit:?}");
                        Ok(())
                    }
                }),
                lands: &[
                    ("events", 0, 1),
                    ("snapshots", 0, 1),
                    ("tool_effects", 0, 1),
                    ("work_items", 1, 1),
                ],
            }
        }
        "append_event" => {
            let (run, _) = started(&b, 0, 3).await;
            let event = Event::new(run, 0, skipped_node("b"));
            Call {
                run: call(move || {
                    let (b, event) = (b.clone(), event.clone());
                    async move { b.append_event(event).await.map(drop) }
                }),
                lands: &[("events", 0, 1)],
            }
        }
        "claim_work_item" => {
            let (_, item) = started(&b, 0, 3).await;
            Call {
                run: call(move || {
                    let (b, item) = (b.clone(), item.clone());
                    async move {
                        let claimed = b
                            .claim_work_item_with_lease(
                                WORKER,
                                &[item.queue_type.as_str()],
                                Some(MAX_LEASE_SECS),
                            )
                            .await?;
                        assert_eq!(claimed.map(|c| c.id), Some(item.id));
                        Ok(())
                    }
                }),
                lands: &[("work_items", 1, 1)],
            }
        }
        "fail_work_item_fenced" | "fail_work_item_fenced_recorded" => {
            let recorded = operation == "fail_work_item_fenced_recorded";
            fail(&b, recorded, at).await
        }
        "finalize_rollover_fenced" => {
            let (run, item) = claimed(&b, 0, 3).await;
            Call {
                run: call(move || {
                    let (b, run) = (b.clone(), run.clone());
                    async move {
                        let settled = b
                            .finalize_rollover_fenced(&run, item.id, item.lease_fence)
                            .await?;
                        assert!(settled);
                        Ok(())
                    }
                }),
                lands: &[("work_items", 1, 1), ("workflow_executions", 1, 1)],
            }
        }
        "apply_approval_projection" => {
            let (run, _) = started(&b, 0, 3).await;
            Call {
                run: call(move || {
                    let (b, run) = (b.clone(), run.clone());
                    async move {
                        b.apply_approval_projection(approval(&run, "gate"), "approvals", 3)
                            .await
                    }
                }),
                lands: &[("proj_approvals", 0, 1), ("projector_checkpoints", 0, 1)],
            }
        }
        "apply_approval_projection_batch" => {
            let (run, _) = started(&b, 0, 3).await;
            Call {
                run: call(move || {
                    let (b, run) = (b.clone(), run.clone());
                    async move {
                        let rows = vec![approval(&run, "gate-1"), approval(&run, "gate-2")];
                        b.apply_approval_projection_batch(rows, "approvals", &run, 4)
                            .await
                    }
                }),
                lands: &[("proj_approvals", 0, 2), ("projector_checkpoints", 0, 1)],
            }
        }
        "create_run" if step == 1 || (at.last && at.path > 0) => {
            // A keyed start: the only run that claims a key. Before the last
            // commit, path 2 claims a key that is new and path 3 moves one
            // from the run it named.
            let key = format!("faults-{}", Uuid::new_v4().simple());
            let replaces = if at.last && at.path == 2 {
                Some(started_with_key(&b, &key).await)
            } else {
                None
            };
            let id = ExecutionId::new();
            let item = pending(&id, 0, 3);
            let lands: &'static [(&'static str, usize, usize)] = if replaces.is_some() {
                &[
                    ("events", 0, 2),
                    ("run_keys", 1, 1),
                    ("work_items", 0, 1),
                    ("workflow_executions", 0, 1),
                ]
            } else {
                &[
                    ("events", 0, 2),
                    ("run_keys", 0, 1),
                    ("work_items", 0, 1),
                    ("workflow_executions", 0, 1),
                ]
            };
            Call {
                run: call(move || {
                    let (b, id, item) = (b.clone(), id.clone(), item.clone());
                    let claim = key_claim(&key, replaces.clone());
                    async move {
                        let scheduled = scheduled(&item);
                        let start = b
                            .start_execution_keyed(
                                run_row(&id),
                                started_kind(),
                                scheduled,
                                item,
                                claim,
                            )
                            .await?;
                        assert_eq!(start, KeyedStart::Started);
                        Ok(())
                    }
                }),
                lands,
            }
        }
        "create_run" => {
            // A continuation segment: the only run with a seed snapshot, so
            // the call runs every statement of the body but the key's.
            let (parent, _) = started(&b, 0, 3).await;
            let id = ExecutionId::new();
            let mut segment = run_row(&id);
            segment.parent_execution_id = Some(parent);
            segment.segment_number = 1;
            let seed = Snapshot::new(id.clone(), 0, json!({ "carried": true }));
            let item = pending(&id, 0, 3);
            Call {
                run: call(move || {
                    let (b, segment, seed, item) =
                        (b.clone(), segment.clone(), seed.clone(), item.clone());
                    async move {
                        let scheduled = scheduled(&item);
                        b.create_segment_atomic(segment, seed, started_kind(), scheduled, item)
                            .await
                    }
                }),
                lands: &[
                    ("events", 0, 2),
                    ("snapshots", 0, 1),
                    ("work_items", 0, 1),
                    ("workflow_executions", 0, 1),
                ],
            }
        }
        "finish_execution" => {
            let (run, _) = started(&b, 0, 3).await;
            let event = Event::new(
                run,
                0,
                EventKind::WorkflowCompleted {
                    final_state: json!({ "done": true }),
                },
            );
            Call {
                run: call(move || {
                    let (b, event) = (b.clone(), event.clone());
                    async move {
                        let finish = b.finish_execution(event, WorkflowStatus::Completed).await?;
                        assert!(matches!(finish, ExecutionFinish::Finished(_)), "{finish:?}");
                        Ok(())
                    }
                }),
                lands: &[("events", 0, 1), ("workflow_executions", 1, 1)],
            }
        }
        "append_event_if" => {
            let (run, _) = started(&b, 0, 3).await;
            let event = Event::new(run, 0, skipped_node("b"));
            Call {
                run: call(move || {
                    let (b, event) = (b.clone(), event.clone());
                    async move {
                        let always = |_: &[Event]| true;
                        let append = b.append_event_if(event, &always).await?;
                        assert!(
                            matches!(append, ConditionalAppend::Appended(_)),
                            "{append:?}"
                        );
                        Ok(())
                    }
                }),
                lands: &[("events", 0, 1)],
            }
        }
        "expire_stale_leases" => {
            // The claim's fast reclaim, run by a claim that finds nothing
            // else to take. Step 2 settles an item of a finished run; the
            // others requeue an item and record its retry. The point before
            // the commit has one of each, so the call runs both branches.
            let finished = step == 2 || at.last;
            let retried = step != 2;
            // Every item is claimed before any lease lapses: a claim runs
            // the fast reclaim first.
            let mut held = Vec::new();
            for _ in 0..usize::from(finished) + usize::from(retried) {
                held.push(claimed(&b, 0, 3).await);
            }
            for (_, item) in &held {
                fx.hooks.force_lease_expired(item.id).await;
            }
            if finished {
                cancel(&b, &held[0].0).await;
            }
            let lands: &'static [(&'static str, usize, usize)] = match (finished, retried) {
                (true, true) => &[("events", 0, 2), ("work_items", 2, 2)],
                (true, false) => &[("work_items", 1, 1)],
                _ => &[("events", 0, 2), ("work_items", 1, 1)],
            };
            Call {
                run: call(move || {
                    let b = b.clone();
                    async move {
                        assert!(claim_nothing(&*b).await?.is_none());
                        Ok(())
                    }
                }),
                lands,
            }
        }
        "reattach_lease" => {
            // The lease lapsed and the fast reclaim requeued the item; its
            // worker's heartbeat takes it back.
            let (_, item) = claimed(&b, 0, 3).await;
            fx.hooks.force_lease_expired(item.id).await;
            assert!(claim_nothing(&*b).await.unwrap().is_none());
            Call {
                run: call(move || {
                    let b = b.clone();
                    async move {
                        let renewal = b.heartbeat_lease(item.id, WORKER, item.lease_fence).await?;
                        assert!(renewal.reattached, "{renewal:?}");
                        Ok(())
                    }
                }),
                lands: &[("work_items", 1, 1)],
            }
        }
        "commit_scheduler_pass" => {
            // A pass that dispatches a node. From step 6 on it also ends
            // the run, so it reaches the steps of the finish, and the point
            // before the commit follows every statement of the body.
            let (_, mut pass) = dispatch_pass(&b).await;
            let lands: &'static [(&'static str, usize, usize)] = if step >= 6 {
                pass.finish = Some((
                    WorkflowStatus::Completed,
                    EventKind::WorkflowCompleted {
                        final_state: json!({}),
                    },
                ));
                &[
                    ("events", 0, 2),
                    ("work_items", 0, 1),
                    ("workflow_executions", 1, 1),
                ]
            } else {
                &[("events", 0, 1), ("work_items", 0, 1)]
            };
            Call {
                run: pass_call(b, pass),
                lands,
            }
        }
        "reclaim_expired_leases_recorded" => {
            // Step 2 is an item of a finished run; 3 and 5 an item out of
            // attempts; the rest an item that is retried. The point before
            // the commit takes both of the last two, one per path.
            let exhausted = matches!(step, 3 | 5) || (at.last && at.path == 1);
            let max_attempts = if exhausted { 1 } else { 3 };
            let (run, item) = claimed(&b, 0, max_attempts).await;
            fx.hooks.force_lease_expired(item.id).await;
            let (lands, retried, exhausted): (&'static [(&'static str, usize, usize)], _, _) =
                match (step, exhausted) {
                    (2, _) => {
                        cancel(&b, &run).await;
                        (&[("work_items", 1, 1)], 0, 0)
                    }
                    (_, true) => (
                        &[
                            ("dead_letter_items", 0, 1),
                            ("events", 0, 1),
                            ("work_items", 1, 1),
                        ],
                        0,
                        1,
                    ),
                    _ => (&[("events", 0, 2), ("work_items", 1, 1)], 1, 0),
                };
            Call {
                run: call(move || {
                    let b = b.clone();
                    async move {
                        let swept = b.reclaim_expired_leases_recorded().await?;
                        assert_eq!(
                            (swept.retryable.len(), swept.exhausted.len()),
                            (retried, exhausted),
                            "{swept:?}"
                        );
                        Ok(())
                    }
                }),
                lands,
            }
        }
        "patch_append_array" => {
            let (run, _) = started(&b, 0, 3).await;
            Call {
                run: call(move || {
                    let (b, run) = (b.clone(), run.clone());
                    async move { b.patch_append_array(&run, "seen", json!("a")).await }
                }),
                lands: &[("workflow_executions", 1, 1)],
            }
        }
        other => panic!("no operation in the faults case for {other}"),
    }
}

/// A worker's failure report, fenced, plain or with its events recorded.
/// Step 2 of the recorded one is an item of a finished run; steps 2 and 3
/// of the plain one, and 3 and 5 of the recorded one, an item out of
/// attempts; the rest an item that is retried. The recorded one's point
/// before its last commit takes both of the last two, one per path.
async fn fail(b: &Arc<dyn StateBackend>, recorded: bool, at: At) -> Call {
    let step = at.step;
    let finished = recorded && step == 2;
    let exhausted = if recorded {
        matches!(step, 3 | 5) || (at.last && at.path == 1)
    } else {
        matches!(step, 2 | 3)
    };
    let (run, item) = claimed(b, 0, if exhausted { 1 } else { 3 }).await;
    if finished {
        cancel(b, &run).await;
    }
    let lands: &'static [(&'static str, usize, usize)] = match (recorded, finished, exhausted) {
        (_, true, _) => &[("work_items", 1, 1)],
        (false, _, true) => &[("dead_letter_items", 0, 1), ("work_items", 1, 1)],
        (false, _, false) => &[("work_items", 1, 1)],
        (true, _, true) => &[
            ("dead_letter_items", 0, 1),
            ("events", 0, 1),
            ("work_items", 1, 1),
        ],
        (true, _, false) => &[("events", 0, 2), ("work_items", 1, 1)],
    };
    let b = b.clone();
    Call {
        run: call(move || {
            let b = b.clone();
            async move {
                let outcome = if recorded {
                    b.fail_work_item_fenced_recorded(item.id, item.lease_fence, "boom")
                        .await?
                } else {
                    b.fail_work_item_fenced(item.id, item.lease_fence, "boom")
                        .await?
                };
                match (&outcome, finished, exhausted) {
                    (None, true, _)
                    | (Some(FailOutcome::Exhausted { .. }), false, true)
                    | (Some(FailOutcome::Retryable { .. }), false, false) => {}
                    _ => panic!("the failure report answered {outcome:?}"),
                }
                Ok(())
            }
        }),
        lands,
    }
}

fn pass_call(
    b: Arc<dyn StateBackend>,
    pass: SchedulerPass,
) -> Box<dyn Fn() -> Outcome + Send + Sync> {
    call(move || {
        let (b, pass) = (b.clone(), pass.clone());
        async move {
            let commit = b.commit_scheduler_pass(pass).await?;
            assert!(matches!(commit, PassCommit::Committed { .. }), "{commit:?}");
            Ok(())
        }
    })
}

// ── Runs, items and events ───────────────────────────────────────────────────

fn run_row(id: &ExecutionId) -> WorkflowExecution {
    let now = chrono::Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf".into(),
        workflow_version: "1.0.0".into(),
        status: WorkflowStatus::Running,
        initial_input: json!({ "input": 1 }),
        current_state: json!({}),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    }
}

/// The start node `a` of `run`, in a queue of the run's own, so a claim of
/// that queue takes this item and no other case's.
fn pending(run: &ExecutionId, attempt: u32, max_attempts: u32) -> WorkItem {
    WorkItem {
        id: Uuid::new_v4(),
        execution_id: run.clone(),
        node_id: "a".into(),
        queue_type: format!("faults-{}", run.0.simple()),
        payload: json!({ "node_id": "a" }),
        attempt,
        max_attempts,
        created_at: chrono::Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: DEFAULT_TENANT.into(),
    }
}

fn started_kind() -> EventKind {
    EventKind::WorkflowStarted {
        workflow_id: "wf".into(),
        workflow_version: "1.0.0".into(),
        initial_input: json!({ "input": 1 }),
        initiated_by: None,
        run_key: None,
    }
}

fn scheduled(item: &WorkItem) -> EventKind {
    EventKind::NodeScheduled {
        node_id: item.node_id.clone(),
        queue_type: item.queue_type.clone(),
    }
}

fn skipped_node(node: &str) -> EventKind {
    EventKind::NodeSkipped {
        node_id: node.into(),
        reason: "not taken".into(),
    }
}

/// `a`'s completion, with `key` as its idempotency key.
fn completed(run: &ExecutionId, key: Option<String>) -> Event {
    Event::new(
        run.clone(),
        0,
        EventKind::NodeCompleted {
            node_id: "a".into(),
            output: json!({ "ok": true }),
            state_patch: json!({ "a": 1 }),
            duration_ms: 1,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
            cost_usd: None,
            provenance: None,
            idempotency_key: key,
        },
    )
}

fn approval(run: &ExecutionId, node: &str) -> ApprovalProjectionRow {
    ApprovalProjectionRow {
        execution_id: run.clone(),
        node_id: node.into(),
        status: "pending".into(),
        user_id: None,
        comment: None,
        last_sequence: 2,
        tool_name: Some("send".into()),
        approver: None,
        context: Some(json!({ "why": "check" })),
    }
}

/// A started run, its start item pending at `attempt` of `max_attempts`.
async fn started(
    b: &Arc<dyn StateBackend>,
    attempt: u32,
    max_attempts: u32,
) -> (ExecutionId, WorkItem) {
    let id = ExecutionId::new();
    let item = pending(&id, attempt, max_attempts);
    b.start_execution_atomic(run_row(&id), started_kind(), scheduled(&item), item.clone())
        .await
        .unwrap();
    (id, item)
}

/// A run started under a fresh claim of `key`, its start item pending.
async fn started_with_key(b: &Arc<dyn StateBackend>, key: &str) -> ExecutionId {
    let id = ExecutionId::new();
    let item = pending(&id, 0, 3);
    let start = b
        .start_execution_keyed(
            run_row(&id),
            started_kind(),
            scheduled(&item),
            item,
            key_claim(key, None),
        )
        .await
        .unwrap();
    assert_eq!(start, KeyedStart::Started);
    id
}

/// The claim of `key` for a run of [`run_row`]'s workflow and input.
fn key_claim(key: &str, replaces: Option<ExecutionId>) -> RunKeyClaim {
    RunKeyClaim {
        run_key: key.into(),
        workflow_id: "wf".into(),
        workflow_version: "1.0.0".into(),
        input_hash: "faults".into(),
        replaces,
    }
}

/// A started run whose start item [`WORKER`] holds, under a lease that
/// outlasts the case.
async fn claimed(
    b: &Arc<dyn StateBackend>,
    attempt: u32,
    max_attempts: u32,
) -> (ExecutionId, WorkItem) {
    let (id, item) = started(b, attempt, max_attempts).await;
    let held = b
        .claim_work_item_with_lease(WORKER, &[item.queue_type.as_str()], Some(MAX_LEASE_SECS))
        .await
        .unwrap()
        .expect("the run's item is claimable");
    assert_eq!(held.id, item.id);
    (id, held)
}

async fn cancel(b: &Arc<dyn StateBackend>, run: &ExecutionId) {
    let event = Event::new(
        run.clone(),
        0,
        EventKind::WorkflowCancelled { reason: None },
    );
    let finish = b
        .finish_execution(event, WorkflowStatus::Cancelled)
        .await
        .unwrap();
    assert!(matches!(finish, ExecutionFinish::Finished(_)), "{finish:?}");
}

/// A claim of a queue nobody uses: it runs the fast reclaim and takes
/// nothing.
async fn claim_nothing(b: &dyn StateBackend) -> BackendResult<Option<WorkItem>> {
    let queue = format!("empty-{}", Uuid::new_v4().simple());
    b.claim_work_item_with_lease(WORKER, &[queue.as_str()], Some(MAX_LEASE_SECS))
        .await
}

/// A started run and the pass that dispatches its second node, `b`, read at
/// the run's latest sequence. The start node's item stays pending, so the
/// pass checks one node with no live item.
async fn dispatch_pass(b: &Arc<dyn StateBackend>) -> (ExecutionId, SchedulerPass) {
    let (run, start) = started(b, 0, 3).await;
    let mut next = pending(&run, 0, 3);
    next.node_id = "b".into();
    next.payload = json!({ "node_id": "b" });
    let pass = SchedulerPass {
        execution_id: run.clone(),
        seen_sequence: b.latest_sequence(&run).await.unwrap(),
        events: vec![EventKind::NodeScheduled {
            node_id: "b".into(),
            queue_type: start.queue_type,
        }],
        work_items: vec![next],
        finish: None,
    };
    (run, pass)
}

// ── Comparing the store's rows ───────────────────────────────────────────────

async fn every_row(fx: &Fixture) -> StoreRows {
    fx.hooks
        .every_row()
        .await
        .expect("a store whose code has fault points keeps rows")
}

/// The rows of one table that differ between two reads.
#[derive(Debug, PartialEq, Eq)]
struct Change {
    /// In the first read and not the second: removed, or as it was before
    /// a change.
    gone: Vec<String>,
    /// In the second read and not the first: added, or as it is after a
    /// change.
    new: Vec<String>,
}

/// The tables whose rows differ between `before` and `after`.
fn changes(before: &StoreRows, after: &StoreRows) -> BTreeMap<String, Change> {
    let none = Vec::new();
    let mut tables: Vec<&String> = before.keys().chain(after.keys()).collect();
    tables.sort();
    tables.dedup();
    let mut out = BTreeMap::new();
    for table in tables {
        let was = before.get(table).unwrap_or(&none);
        let is = after.get(table).unwrap_or(&none);
        let change = Change {
            gone: missing_from(was, is),
            new: missing_from(is, was),
        };
        if !change.gone.is_empty() || !change.new.is_empty() {
            out.insert(table.clone(), change);
        }
    }
    out
}

/// The rows of `rows` that `other` lacks, counting repeats.
fn missing_from(rows: &[String], other: &[String]) -> Vec<String> {
    let mut left: BTreeMap<&str, usize> = BTreeMap::new();
    for row in other {
        *left.entry(row).or_default() += 1;
    }
    let mut missing = Vec::new();
    for row in rows {
        match left.get_mut(row.as_str()) {
            Some(n) if *n > 0 => *n -= 1,
            _ => missing.push(row.clone()),
        }
    }
    missing
}
