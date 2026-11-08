//! Worker heartbeat — periodically renews the lease on a claimed work item.

use jamjet_state::backend::{StateBackend, WorkItemId};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

/// The heartbeat task of one claimed item. Dropping it stops the task.
///
/// A bare `JoinHandle` detaches its task when dropped, so every early return
/// (`?`) between spawning a heartbeat and the explicit `abort` left a task
/// renewing the lease forever: the item could never be reclaimed and its run
/// never moved again.
#[must_use = "dropping the heartbeat stops it"]
pub struct Heartbeat(tokio::task::JoinHandle<()>);

impl Heartbeat {
    /// Stop renewing the lease.
    pub fn abort(&self) {
        self.0.abort();
    }

    /// Whether the task has stopped (aborted, or the lease was lost).
    pub fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Spawns a background task that renews the lease on a work item, stopped
/// when the returned [`Heartbeat`] is aborted or dropped.
pub fn spawn_heartbeat(
    backend: Arc<dyn StateBackend>,
    item_id: WorkItemId,
    worker_id: String,
    lease_fence: i64,
    interval: Duration,
) -> Heartbeat {
    Heartbeat(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            if let Err(e) = backend.renew_lease(item_id, &worker_id, lease_fence).await {
                // The lease is gone (stolen, failed over, or item already settled).
                // Stop renewing; the fenced commit path fails closed on its own.
                info!(
                    worker_id = %worker_id,
                    item_id = %item_id,
                    "Heartbeat stopping: lease no longer held ({e})"
                );
                break;
            }
        }
    }))
}
