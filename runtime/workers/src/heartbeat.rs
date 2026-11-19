//! Worker heartbeat — periodically renews the lease on a claimed work item.

use jamjet_state::backend::{StateBackend, StateBackendError, WorkItemId};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// How many times one renewal is retried on an error other than a lost
/// fence, with backoff (1 s, 2 s, 4 s), before the heartbeat gives up.
const TRANSIENT_RETRIES: u32 = 3;

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
            // A lost fence ends the heartbeat at once. Any other error (a
            // busy database) is retried with backoff first: one used to end
            // it, and a no-limit node then lost its lease and its reservation
            // over a transient error.
            let mut failures = 0u32;
            loop {
                match backend.renew_lease(item_id, &worker_id, lease_fence).await {
                    Ok(()) => break,
                    Err(StateBackendError::FenceLost(_)) => {
                        info!(
                            worker_id = %worker_id,
                            item_id = %item_id,
                            "Heartbeat stopping: lease no longer held"
                        );
                        return;
                    }
                    Err(e) if failures < TRANSIENT_RETRIES => {
                        failures += 1;
                        warn!(
                            worker_id = %worker_id,
                            item_id = %item_id,
                            attempt = failures,
                            "Heartbeat: renewing the lease failed ({e}); retrying"
                        );
                        tokio::time::sleep(Duration::from_millis(500) * 2u32.pow(failures)).await;
                    }
                    Err(e) => {
                        warn!(
                            worker_id = %worker_id,
                            item_id = %item_id,
                            "Heartbeat stopping: renewing the lease kept failing ({e})"
                        );
                        return;
                    }
                }
            }
        }
    }))
}
