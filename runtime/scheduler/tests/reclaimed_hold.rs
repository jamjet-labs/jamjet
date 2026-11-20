//! A node held for an approval whose worker stops before it settles the
//! item.
//!
//! The sweep takes the item back and puts it on the queue again, and the
//! hold stays. When the approval then lands, the node may run again, and the
//! requeued item is what runs it. A second item for the node would run the
//! step twice.

mod support;

use jamjet_core::workflow::WorkflowStatus;
use jamjet_scheduler::Scheduler;
use jamjet_state::event::ApprovalDecision;
use jamjet_state::{Event, EventKind};
use serde_json::json;
use std::time::{Duration, Instant};
use support::{kinds, linear, per_node, status, stores, Worker};

/// The approval lands while the node's requeued item is still pending.
/// The scheduler must not dispatch the node a second time beside it: its
/// pass is refused while the old item is pending or claimed, so the run
/// waits for its pass until that item settles, up to one lease once a
/// worker holds it.
#[tokio::test]
async fn a_reclaimed_hold_approved_while_its_item_is_pending_runs_once() {
    for store in stores().await {
        let name = store.name();
        let backend = store.backend();
        let scheduler = Scheduler::new(backend.clone());
        let execution_id = support::start_run(backend.as_ref(), linear(&["a", "b"])).await;

        // A worker takes a's item and holds the node for a decision, then
        // stops before it settles the item.
        let held = Worker::default().claim_all(backend.as_ref()).await;
        assert_eq!(held.len(), 1, "{name}: a's item");
        let item = held[0].clone();
        backend
            .append_event(Event::new(
                execution_id.clone(),
                0,
                EventKind::ToolApprovalRequired {
                    node_id: "a".into(),
                    tool_name: "deploy".into(),
                    approver: "human".into(),
                    context: json!({}),
                },
            ))
            .await
            .unwrap();
        store.expire_lease(item.id).await;

        // The sweep runs before any claim, so it is the sweep that takes the
        // item back, not a claim's own reclaim.
        scheduler.tick().await.unwrap();
        let log = backend.get_events(&execution_id).await.unwrap();
        assert!(
            log.iter()
                .any(|e| matches!(e.kind, EventKind::RetryScheduled { .. })),
            "{name}: the sweep requeued the item; log {:?}",
            kinds(&log)
        );

        backend
            .append_event(Event::new(
                execution_id.clone(),
                0,
                EventKind::ApprovalReceived {
                    node_id: "a".into(),
                    user_id: "reviewer".into(),
                    decision: ApprovalDecision::Approved,
                    comment: None,
                    state_patch: None,
                    principal: None,
                    approved_digest: None,
                    request_sequence: None,
                },
            ))
            .await
            .unwrap();

        // Ticks and a worker take turns until the run ended and the requeued
        // item ran. SQLite puts a requeued item back after a retry delay (two
        // seconds at the first retry), so this takes that long there.
        let worker = Worker::default();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            scheduler.tick().await.unwrap();
            worker.complete_all(backend.as_ref(), |_| json!({})).await;
            let ended = status(backend.as_ref(), &execution_id).await.is_terminal();
            if ended && worker.items_of("a").contains(&item.id) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{name}: the run did not end with a's requeued item run; items of a {:?}, log {:?}",
                worker.items_of("a"),
                kinds(&backend.get_events(&execution_id).await.unwrap())
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let events = backend.get_events(&execution_id).await.unwrap();
        let completed = per_node(&events, |k| match k {
            EventKind::NodeCompleted { node_id, .. } => Some(node_id),
            _ => None,
        });
        assert_eq!(
            (completed.get("a"), worker.items_of("a").len()),
            (Some(&1), 1),
            "{name}: a completes once and runs once, from its requeued item; log {:?}",
            kinds(&events)
        );
        assert_eq!(
            status(backend.as_ref(), &execution_id).await,
            WorkflowStatus::Completed,
            "{name}"
        );
        assert!(
            scheduler.stale_passes() >= 1,
            "{name}: no pass was refused, so the scheduler never tried to dispatch a beside \
             its requeued item and the test showed nothing"
        );
    }
}
