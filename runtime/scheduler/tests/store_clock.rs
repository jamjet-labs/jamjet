//! The scheduler's deadline sweep reads the store's clock, not this
//! process's.
//!
//! On Postgres the store's clock is the database's, so two engines whose
//! clocks disagree still expire a request at one moment. Here the store's
//! clock is set apart from the process's through the kit's wrapper, and the
//! sweep is driven by the scheduler's own tick.

mod support;

use chrono::{DateTime, Duration, Utc};
use jamjet_core::workflow::ExecutionId;
use jamjet_scheduler::Scheduler;
use jamjet_state::approvals::{format_deadline, EXPIRES_AT_KEY};
use jamjet_state::backend::StateBackend;
use jamjet_state::{Event, EventKind};
use jamjet_testkit::{Hooks, Intercept};
use serde_json::json;
use std::sync::Arc;
use support::{kinds, linear, start_run, stores, Worker};

/// A store whose clock answers `now`.
struct StoreClock(DateTime<Utc>);

impl Hooks for StoreClock {
    fn store_now(&self) -> Option<DateTime<Utc>> {
        Some(self.0)
    }
}

/// Start a one-node run, take its item and hold the node on a request that
/// expires at `expires_at`.
async fn held_until(backend: &dyn StateBackend, expires_at: DateTime<Utc>) -> ExecutionId {
    let execution_id = start_run(backend, linear(&["a"])).await;
    let held = Worker::default().claim_all(backend).await;
    assert_eq!(held.len(), 1, "a's item");
    backend
        .append_event(Event::new(
            execution_id.clone(),
            0,
            EventKind::ToolApprovalRequired {
                node_id: "a".into(),
                tool_name: "deploy".into(),
                approver: "human".into(),
                context: json!({ EXPIRES_AT_KEY: format_deadline(&expires_at) }),
            },
        ))
        .await
        .unwrap();
    execution_id
}

async fn expiries(backend: &dyn StateBackend, execution_id: &ExecutionId) -> (usize, Vec<String>) {
    let log = backend.get_events(execution_id).await.unwrap();
    let n = log
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ApprovalExpired { .. }))
        .count();
    (n, kinds(&log))
}

/// A store clock past the deadline expires the request while the process
/// clock is still before it; a store clock before the deadline leaves it
/// open while the process clock is past it.
#[tokio::test]
async fn sweep_uses_the_store_clock() {
    for store in stores().await {
        let name = store.name();
        for (expires_in, store_after_deadline) in [
            (Duration::seconds(600), Duration::seconds(1)),
            (Duration::seconds(-600), Duration::seconds(-1)),
        ] {
            let expires_at = Utc::now() + expires_in;
            let clocked: Arc<dyn StateBackend> = Arc::new(Intercept::new(
                store.backend(),
                Arc::new(StoreClock(expires_at + store_after_deadline)),
            ));
            let execution_id = held_until(clocked.as_ref(), expires_at).await;

            Scheduler::new(clocked.clone()).tick().await.unwrap();

            let want = usize::from(store_after_deadline > Duration::zero());
            let process_past = Utc::now() > expires_at;
            let (got, log) = expiries(clocked.as_ref(), &execution_id).await;
            assert_eq!(
                got,
                want,
                "{name}: store clock {} s from the deadline, process clock past it: \
                 {process_past}; log {log:?}",
                store_after_deadline.num_seconds()
            );
        }
    }
}
