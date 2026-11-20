//! Two engines on one store. Each case races calls from the fixture's two
//! handles (`backend` and `second`, two pools on one store where the store
//! has pools), round after round, and asserts that some calls overlapped:
//! both had started before either returned. Calls made one after the other
//! give the answers these cases accept, so a run in which nothing
//! overlapped shows nothing.
//!
//! The cases run in the default tenant on the scoped stores, where both
//! handles are the default tenant's views.

use crate::conformance::cases::support::{all_at_once, race, skipped, timed, Span};
use crate::conformance::Fixture;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::approvals::still_pending;
use jamjet_state::event::{ApprovalDecision, DEADLINE_PRINCIPAL};
use jamjet_state::{
    ApprovalProjectionRow, ConditionalAppend, Event, EventKind, ExecutionFinish, StateBackend,
    StateBackendError, WorkItem, WorkItemId, DEFAULT_TENANT,
};
use serde_json::json;
use std::sync::Arc;

/// Rounds per race, unless a case says otherwise.
const ROUNDS: usize = 200;

/// A store's name and its two handles, as two engines hold it.
type Handles = (String, Arc<dyn StateBackend>, Arc<dyn StateBackend>);

/// The fixture's two handles, or `None` where it has only one (said on
/// stderr).
fn handles(fx: &Fixture) -> Option<Handles> {
    let Some(second) = fx.second.clone() else {
        skipped(fx, "the store has no second handle");
        return None;
    };
    Some((format!("{:?}", fx.kind), fx.backend.clone(), second))
}

fn run(id: &ExecutionId) -> WorkflowExecution {
    let now = chrono::Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf-race".into(),
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

/// A running run with one pending item on `queue`. Each round uses a queue
/// of its own, so no earlier round's item is in reach of its claims.
async fn run_with_an_item(b: &dyn StateBackend, queue: &str) -> (ExecutionId, WorkItemId) {
    let id = ExecutionId::new();
    b.create_execution(run(&id)).await.unwrap();
    let item = WorkItem {
        id: uuid::Uuid::new_v4(),
        execution_id: id.clone(),
        node_id: "step".into(),
        queue_type: queue.into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: chrono::Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: DEFAULT_TENANT.into(),
    };
    let item_id = b.enqueue_work_item(item).await.unwrap();
    (id, item_id)
}

/// The event's `type` tag, as the log stores it: `workflow_completed`.
fn type_of(e: &Event) -> String {
    let kind = serde_json::to_value(&e.kind).unwrap();
    kind["type"].as_str().unwrap_or("?").to_string()
}

fn completed(by: &str) -> EventKind {
    EventKind::NodeCompleted {
        node_id: "step".into(),
        output: json!({ "by": by }),
        state_patch: json!({}),
        duration_ms: 1,
        gen_ai_system: None,
        gen_ai_model: None,
        input_tokens: None,
        output_tokens: None,
        finish_reason: None,
        cost_usd: None,
        provenance: None,
        idempotency_key: None,
    }
}

/// Two claimers on one pending item, released at once: one gets it, the
/// other gets nothing, and the store holds the item under the winner's
/// lease. Over the run each handle wins at least once, so neither handle
/// only ever claimed after the other had finished.
pub async fn two_handles_claim_one_item_once(fx: &Fixture) {
    const ROUNDS: usize = 400;
    let Some((name, first, second)) = handles(fx) else {
        return;
    };
    let mut overlapped = 0;
    let mut wins = [0usize; 2];
    for round in 0..ROUNDS {
        let queue = format!("claim-{round}");
        let (_, id) = run_with_an_item(&*first, &queue).await;
        let (a, b, qa, qb) = (first.clone(), second.clone(), queue.clone(), queue.clone());
        let (x, y, at_once) = race(
            async move { a.claim_work_item("worker-a", &[qa.as_str()]).await },
            async move { b.claim_work_item("worker-b", &[qb.as_str()]).await },
        )
        .await;
        overlapped += usize::from(at_once);
        let (x, y) = (x.unwrap(), y.unwrap());
        let (winner, held, handle) = match (&x, &y) {
            (Some(held), None) => (0, held, &first),
            (None, Some(held)) => (1, held, &second),
            _ => panic!(
                "{name}, round {round}: one claim takes the item and the other gets nothing; \
                 {x:?}, {y:?}"
            ),
        };
        wins[winner] += 1;
        assert_eq!(held.id, id, "{name}, round {round}");
        let row = fx.hooks.item_row(id).await.expect("the item's row");
        assert_eq!(
            (
                row.status.as_str(),
                row.worker_id.as_deref(),
                row.lease_fence
            ),
            ("claimed", held.worker_id.as_deref(), held.lease_fence),
            "{name}, round {round}: the store holds the item under the winner's lease"
        );
        assert_eq!(row.lease_epoch, 1, "{name}, round {round}: one claim");
        assert!(
            handle
                .complete_work_item_fenced(id, held.lease_fence)
                .await
                .unwrap(),
            "{name}, round {round}: the winner's lease settles the item"
        );
    }
    assert!(
        overlapped > 0,
        "{name}: the two claims never overlapped in {ROUNDS} rounds, so no round raced"
    );
    assert!(
        wins[0] > 0 && wins[1] > 0,
        "{name}: each handle wins at least once in {ROUNDS} rounds; wins {wins:?}"
    );
}

/// Worker A's lease lapses and worker B, on the other handle, claims the
/// item again. A's late completion and B's completion race: A's fence is
/// refused (`FenceLost` from `commit_turn`, `false` from
/// `complete_work_item_fenced`), B's lands, and the run records one
/// `NodeCompleted`, B's.
pub async fn late_completion_after_reclaim_is_refused_by_fence(fx: &Fixture) {
    let Some((name, first, second)) = handles(fx) else {
        return;
    };
    let mut overlapped = 0;
    for round in 0..ROUNDS {
        let queue = format!("late-{round}");
        let (run_id, id) = run_with_an_item(&*first, &queue).await;
        let lost = first
            .claim_work_item("worker-a", &[queue.as_str()])
            .await
            .unwrap()
            .expect("the item is claimable");
        fx.hooks.force_lease_expired(id).await;
        let taken = second
            .claim_work_item("worker-b", &[queue.as_str()])
            .await
            .unwrap()
            .expect("a lapsed lease is claimable again");
        assert_eq!(taken.id, id, "{name}, round {round}");
        assert!(
            taken.lease_fence > lost.lease_fence,
            "{name}, round {round}: the new claim mints a newer fence"
        );

        let (a, b) = (first.clone(), second.clone());
        let (late_fence, fence) = (lost.lease_fence, taken.lease_fence);
        let by_a = Event::new(run_id.clone(), 0, completed("worker-a"));
        let by_b = Event::new(run_id.clone(), 0, completed("worker-b"));
        let (late, current, at_once) = race(
            async move { a.commit_turn(id, late_fence, by_a, false).await },
            async move { b.commit_turn(id, fence, by_b, false).await },
        )
        .await;
        overlapped += usize::from(at_once);
        assert!(
            matches!(late, Err(StateBackendError::FenceLost(_))),
            "{name}, round {round}: A's late commit: {late:?}"
        );
        current.unwrap_or_else(|e| panic!("{name}, round {round}: B's commit: {e}"));
        assert!(
            !first
                .complete_work_item_fenced(id, lost.lease_fence)
                .await
                .unwrap(),
            "{name}, round {round}: A's fenced completion is refused"
        );

        let completions: Vec<serde_json::Value> = first
            .get_events(&run_id)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.kind {
                EventKind::NodeCompleted { output, .. } => Some(output),
                _ => None,
            })
            .collect();
        assert_eq!(
            completions,
            vec![json!({ "by": "worker-b" })],
            "{name}, round {round}: one NodeCompleted, B's"
        );
    }
    assert!(
        overlapped > 0,
        "{name}: the two commits never overlapped in {ROUNDS} rounds, so no round raced"
    );
}

/// Worker A's lease lapses. A's heartbeat and worker B's claim, on the other
/// handle, race for the item, and exactly one gets it. Where B's claim lands
/// first, A's heartbeat is refused and cannot re-attach, and B keeps the
/// lease: the item is B's under B's fence, B renews it, and A's next
/// heartbeat is refused too. Some rounds must go B's way, or the case never
/// saw a heartbeat after another claim.
pub async fn late_heartbeat_after_another_claim_cannot_reattach(fx: &Fixture) {
    let Some((name, first, second)) = handles(fx) else {
        return;
    };
    let mut overlapped = 0;
    let (mut kept_by_a, mut taken_by_b) = (0, 0);
    for round in 0..ROUNDS {
        let queue = format!("heartbeat-{round}");
        let (_, id) = run_with_an_item(&*first, &queue).await;
        let lost = first
            .claim_work_item("worker-a", &[queue.as_str()])
            .await
            .unwrap()
            .expect("the item is claimable");
        fx.hooks.force_lease_expired(id).await;

        let (a, b, q) = (first.clone(), second.clone(), queue.clone());
        let late_fence = lost.lease_fence;
        let (beat, claim, at_once) = race(
            async move { a.heartbeat_lease(id, "worker-a", late_fence).await },
            async move { b.claim_work_item("worker-b", &[q.as_str()]).await },
        )
        .await;
        overlapped += usize::from(at_once);
        let claim = claim.unwrap();
        let row = fx.hooks.item_row(id).await.expect("the item's row");
        match (beat, claim) {
            (Ok(renewal), None) => {
                kept_by_a += 1;
                assert_eq!(
                    (row.worker_id.as_deref(), row.lease_fence),
                    (Some("worker-a"), renewal.lease_fence),
                    "{name}, round {round}: A kept the item"
                );
                assert!(first
                    .complete_work_item_fenced(id, renewal.lease_fence)
                    .await
                    .unwrap());
            }
            (Err(StateBackendError::FenceLost(_)), Some(taken)) => {
                taken_by_b += 1;
                assert_eq!(taken.id, id, "{name}, round {round}");
                assert_eq!(
                    (
                        row.status.as_str(),
                        row.worker_id.as_deref(),
                        row.lease_fence
                    ),
                    ("claimed", Some("worker-b"), taken.lease_fence),
                    "{name}, round {round}: B keeps the lease"
                );
                second
                    .renew_lease(id, "worker-b", taken.lease_fence)
                    .await
                    .unwrap_or_else(|e| panic!("{name}, round {round}: B renews: {e}"));
                let again = first.heartbeat_lease(id, "worker-a", late_fence).await;
                assert!(
                    matches!(again, Err(StateBackendError::FenceLost(_))),
                    "{name}, round {round}: A's next heartbeat: {again:?}"
                );
                assert!(second
                    .complete_work_item_fenced(id, taken.lease_fence)
                    .await
                    .unwrap());
            }
            (beat, claim) => panic!(
                "{name}, round {round}: exactly one of A's heartbeat and B's claim gets the \
                 item; heartbeat {beat:?}, claim {claim:?}"
            ),
        }
    }
    assert!(
        overlapped > 0,
        "{name}: the heartbeat and the claim never overlapped in {ROUNDS} rounds"
    );
    assert!(
        taken_by_b > 0,
        "{name}: B's claim never landed first in {ROUNDS} rounds (A kept it {kept_by_a} times)"
    );
}

/// `finish_execution` from both handles at once, one completing the run and
/// one cancelling it: one is `Finished`, the other `AlreadyTerminal` with
/// the winner's status, and the log holds one terminal event, the winner's.
pub async fn two_finishes_record_one_terminal_event(fx: &Fixture) {
    let Some((name, first, second)) = handles(fx) else {
        return;
    };
    let mut overlapped = 0;
    for round in 0..ROUNDS {
        let id = ExecutionId::new();
        first.create_execution(run(&id)).await.unwrap();
        let (a, b) = (first.clone(), second.clone());
        let complete = Event::new(
            id.clone(),
            0,
            EventKind::WorkflowCompleted {
                final_state: json!({}),
            },
        );
        let cancel = Event::new(
            id.clone(),
            0,
            EventKind::WorkflowCancelled {
                reason: Some("by the other engine".into()),
            },
        );
        let (x, y, at_once) = race(
            async move {
                a.finish_execution(complete, WorkflowStatus::Completed)
                    .await
            },
            async move { b.finish_execution(cancel, WorkflowStatus::Cancelled).await },
        )
        .await;
        overlapped += usize::from(at_once);
        let (x, y) = (x.unwrap(), y.unwrap());
        let (sequence, status, tag) = match (&x, &y) {
            (ExecutionFinish::Finished(s), ExecutionFinish::AlreadyTerminal(prior))
                if *prior == WorkflowStatus::Completed =>
            {
                (*s, WorkflowStatus::Completed, "workflow_completed")
            }
            (ExecutionFinish::AlreadyTerminal(prior), ExecutionFinish::Finished(s))
                if *prior == WorkflowStatus::Cancelled =>
            {
                (*s, WorkflowStatus::Cancelled, "workflow_cancelled")
            }
            _ => panic!(
                "{name}, round {round}: one finish lands and the other sees its status; \
                 {x:?}, {y:?}"
            ),
        };
        let log: Vec<(i64, String)> = first
            .get_events(&id)
            .await
            .unwrap()
            .iter()
            .map(|e| (e.sequence, type_of(e)))
            .collect();
        assert_eq!(
            log,
            vec![(sequence, tag.to_string())],
            "{name}, round {round}: one terminal event"
        );
        let stored = first.get_execution(&id).await.unwrap().unwrap().status;
        assert_eq!(stored, status, "{name}, round {round}");
    }
    assert!(
        overlapped > 0,
        "{name}: the two finishes never overlapped in {ROUNDS} rounds, so no round raced"
    );
}

/// A human decision and the deadline's expiry of one approval request, each
/// appended with `append_event_if` under `still_pending`, from both handles
/// at once: exactly one lands, the other is refused, and the request has
/// one outcome in the log.
pub async fn decision_and_expiry_of_one_request_land_once(fx: &Fixture) {
    let Some((name, first, second)) = handles(fx) else {
        return;
    };
    let mut overlapped = 0;
    let mut decided = 0;
    for round in 0..ROUNDS {
        let id = ExecutionId::new();
        first.create_execution(run(&id)).await.unwrap();
        let request = first
            .append_event(Event::new(
                id.clone(),
                0,
                EventKind::ToolApprovalRequired {
                    node_id: "gate".into(),
                    tool_name: "tool".into(),
                    approver: "human".into(),
                    context: json!({}),
                },
            ))
            .await
            .unwrap();
        let decision = Event::new(
            id.clone(),
            0,
            EventKind::ApprovalReceived {
                node_id: "gate".into(),
                user_id: "alice".into(),
                decision: ApprovalDecision::Approved,
                comment: None,
                state_patch: None,
                principal: None,
                approved_digest: None,
                request_sequence: Some(request),
                choice: None,
            },
        );
        let expiry = Event::new(
            id.clone(),
            0,
            EventKind::ApprovalExpired {
                node_id: "gate".into(),
                request_sequence: request,
                expires_at: chrono::Utc::now(),
                principal: DEADLINE_PRINCIPAL.into(),
            },
        );
        let (a, b) = (first.clone(), second.clone());
        let (x, y, at_once) = race(
            async move {
                let pending = move |log: &[Event]| still_pending(log, "gate", request);
                a.append_event_if(decision, &pending).await
            },
            async move {
                let pending = move |log: &[Event]| still_pending(log, "gate", request);
                b.append_event_if(expiry, &pending).await
            },
        )
        .await;
        overlapped += usize::from(at_once);
        let (x, y) = (x.unwrap(), y.unwrap());
        let outcome = match (&x, &y) {
            (ConditionalAppend::Appended(s), ConditionalAppend::Refused) if *s == request + 1 => {
                decided += 1;
                "approval_received"
            }
            (ConditionalAppend::Refused, ConditionalAppend::Appended(s)) if *s == request + 1 => {
                "approval_expired"
            }
            _ => panic!(
                "{name}, round {round}: one of the decision and the expiry lands; {x:?}, {y:?}"
            ),
        };
        let tail: Vec<String> = first
            .get_events_since(&id, request)
            .await
            .unwrap()
            .iter()
            .map(type_of)
            .collect();
        assert_eq!(
            tail,
            vec![outcome],
            "{name}, round {round}: the request has one outcome"
        );
    }
    assert!(
        overlapped > 0,
        "{name}: the decision and the expiry never overlapped in {ROUNDS} rounds \
         ({decided} decisions landed)"
    );
}

/// Eight writers, four on each handle, started at once, append 50 events
/// each to one run: the log holds the 400 events at 400 consecutive
/// sequences with no repeat, and each writer's events in the order it
/// appended them. Some appends of the two handles were in flight at once.
/// The run is made with `create_execution`, so the log holds only these; on
/// memory, whose one counter numbers every run's events, it is the only run
/// in the store.
pub async fn concurrent_appends_keep_sequences_dense(fx: &Fixture) {
    const WRITERS: usize = 8;
    const APPENDS: u32 = 50;
    let Some((name, first, second)) = handles(fx) else {
        return;
    };
    let id = ExecutionId::new();
    first.create_execution(run(&id)).await.unwrap();
    let writers = (0..WRITERS).map(|w| {
        let handle = if w % 2 == 0 {
            first.clone()
        } else {
            second.clone()
        };
        let id = id.clone();
        async move {
            let mut spans = Vec::new();
            for n in 0..APPENDS {
                let kind = EventKind::NodeStarted {
                    node_id: format!("writer-{w}"),
                    worker_id: format!("handle-{}", w % 2),
                    attempt: n,
                };
                let (out, span) = timed(handle.append_event(Event::new(id.clone(), 0, kind))).await;
                out?;
                spans.push(span);
            }
            Ok::<_, StateBackendError>(spans)
        }
    });
    // The spans of each handle's appends.
    let mut spans: [Vec<Span>; 2] = [Vec::new(), Vec::new()];
    for (w, done) in all_at_once(writers.collect()).await.into_iter().enumerate() {
        let done = done.unwrap_or_else(|e| panic!("{name}: an append failed: {e}"));
        spans[w % 2].extend(done);
    }
    let overlapped = spans[0]
        .iter()
        .filter(|a| spans[1].iter().any(|b| a.overlaps(*b)))
        .count();

    let log = first.get_events(&id).await.unwrap();
    let total = WRITERS * APPENDS as usize;
    assert_eq!(log.len(), total, "{name}: every append is in the log");
    let sequences: Vec<i64> = log.iter().map(|e| e.sequence).collect();
    let start = sequences[0];
    let dense: Vec<i64> = (start..start + total as i64).collect();
    assert_eq!(
        sequences, dense,
        "{name}: the sequences are consecutive from {start}, with no repeat"
    );
    let mut next = [0u32; WRITERS];
    for e in &log {
        let EventKind::NodeStarted {
            node_id, attempt, ..
        } = &e.kind
        else {
            panic!("{name}: an event no writer appended: {:?}", e.kind);
        };
        let w: usize = node_id["writer-".len()..].parse().unwrap();
        assert_eq!(
            *attempt, next[w],
            "{name}: writer {w}'s appends are in its order"
        );
        next[w] += 1;
    }
    assert!(
        overlapped > 0,
        "{name}: no append of one handle was in flight with one of the other's"
    );
}

/// Two handles append 100 values each to one array in a run's state, a pair
/// of appends at a time, released at once: the array holds all 200 values,
/// once each, and each handle's in the order it appended them. A
/// read-change-write with no lock loses one of two racing appends.
pub async fn concurrent_array_appends_keep_every_value(fx: &Fixture) {
    const PAIRS: u32 = 100;
    let Some((name, first, second)) = handles(fx) else {
        return;
    };
    let id = ExecutionId::new();
    first.create_execution(run(&id)).await.unwrap();
    let mut overlapped = 0;
    for n in 0..PAIRS {
        let (a, b, ia, ib) = (first.clone(), second.clone(), id.clone(), id.clone());
        let (x, y, at_once) = race(
            async move {
                a.patch_append_array(&ia, "values", json!({ "by": "a", "n": n }))
                    .await
            },
            async move {
                b.patch_append_array(&ib, "values", json!({ "by": "b", "n": n }))
                    .await
            },
        )
        .await;
        overlapped += usize::from(at_once);
        x.unwrap_or_else(|e| panic!("{name}, pair {n}: a's append: {e}"));
        y.unwrap_or_else(|e| panic!("{name}, pair {n}: b's append: {e}"));
    }

    let state = first
        .get_execution(&id)
        .await
        .unwrap()
        .unwrap()
        .current_state;
    let values = state["values"].as_array().cloned().unwrap_or_default();
    let by = |who: &str| -> Vec<u64> {
        values
            .iter()
            .filter(|v| v["by"] == who)
            .map(|v| v["n"].as_u64().unwrap())
            .collect()
    };
    let every: Vec<u64> = (0..u64::from(PAIRS)).collect();
    assert_eq!(
        (values.len(), by("a"), by("b")),
        (2 * PAIRS as usize, every.clone(), every),
        "{name}: every value once, each handle's in its order"
    );
    assert!(
        overlapped > 0,
        "{name}: the two appends never overlapped in {PAIRS} pairs, so no pair raced"
    );
}

/// An approval read-model row as the projector writes it.
fn projected(
    execution_id: &ExecutionId,
    status: &str,
    last_sequence: i64,
) -> ApprovalProjectionRow {
    ApprovalProjectionRow {
        execution_id: execution_id.clone(),
        node_id: "gate".into(),
        status: status.into(),
        user_id: None,
        comment: None,
        last_sequence,
        tool_name: None,
        approver: None,
        context: None,
    }
}

/// The row and the checkpoint the store holds for the run.
async fn projection(b: &dyn StateBackend, id: &ExecutionId) -> (Vec<(String, i64)>, i64) {
    let rows = b
        .get_approval_projection(id)
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.status, r.last_sequence))
        .collect();
    let checkpoint = b.get_projector_checkpoint("approvals", id).await.unwrap();
    (rows, checkpoint)
}

/// Two projectors, one behind the other. A batch at checkpoint 5 that lands
/// after one at 9 leaves the row and the checkpoint at 9; so does every
/// round of the two batches racing from both handles.
pub async fn a_stale_projection_batch_never_regresses(fx: &Fixture) {
    let Some((name, first, second)) = handles(fx) else {
        return;
    };
    let newer = |id: &ExecutionId| (vec![projected(id, "approved", 9)], 9);
    let stale = |id: &ExecutionId| (vec![projected(id, "pending", 5)], 5);
    let expected = (vec![("approved".to_string(), 9)], 9);

    let id = ExecutionId::new();
    first.create_execution(run(&id)).await.unwrap();
    let (rows, checkpoint) = newer(&id);
    first
        .apply_approval_projection_batch(rows, "approvals", &id, checkpoint)
        .await
        .unwrap();
    let (rows, checkpoint) = stale(&id);
    second
        .apply_approval_projection_batch(rows, "approvals", &id, checkpoint)
        .await
        .unwrap();
    assert_eq!(
        projection(&*first, &id).await,
        expected,
        "{name}: a stale batch after a newer one"
    );

    let mut overlapped = 0;
    for round in 0..ROUNDS {
        let id = ExecutionId::new();
        first.create_execution(run(&id)).await.unwrap();
        let (a, b, ia, ib) = (first.clone(), second.clone(), id.clone(), id.clone());
        let ((rows_a, at_a), (rows_b, at_b)) = (newer(&id), stale(&id));
        let (x, y, at_once) = race(
            async move {
                a.apply_approval_projection_batch(rows_a, "approvals", &ia, at_a)
                    .await
            },
            async move {
                b.apply_approval_projection_batch(rows_b, "approvals", &ib, at_b)
                    .await
            },
        )
        .await;
        overlapped += usize::from(at_once);
        x.unwrap();
        y.unwrap();
        assert_eq!(
            projection(&*first, &id).await,
            expected,
            "{name}, round {round}"
        );
    }
    assert!(
        overlapped > 0,
        "{name}: the two batches never overlapped in {ROUNDS} rounds, so no round raced"
    );
}
