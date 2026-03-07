//! Worker heartbeat — periodically renews the lease on a claimed work item.

use jamjet_state::backend::{StateBackend, WorkItemId};
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

/// Spawns a background task that renews the lease on a work item.
/// Returns a JoinHandle that can be aborted when the item is completed.
pub fn spawn_heartbeat(
    backend: Arc<dyn StateBackend>,
    item_id: WorkItemId,
    worker_id: String,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            if let Err(e) = backend.renew_lease(item_id, &worker_id).await {
                warn!(
                    worker_id = %worker_id,
                    item_id = %item_id,
                    "Failed to renew lease: {e}"
                );
            }
        }
    })
}
