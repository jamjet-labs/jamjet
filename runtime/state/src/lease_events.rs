//! The events that record a lost lease or a worker's failure report: what
//! the scheduler's lease sweep and `POST /work-items/:id/fail` leave in a
//! run's log. The backends write them in the same step as the settle (see
//! [`StateBackend::reclaim_expired_leases_recorded`] and
//! [`StateBackend::fail_work_item_fenced_recorded`]), so they are built here
//! once, for every backend.
//!
//! The claim's own fast reclaim writes a different trail for the same lapse,
//! `crate::sqlite::lease_expiry_events`, and keeps it.
//!
//! [`StateBackend::reclaim_expired_leases_recorded`]: crate::backend::StateBackend::reclaim_expired_leases_recorded
//! [`StateBackend::fail_work_item_fenced_recorded`]: crate::backend::StateBackend::fail_work_item_fenced_recorded

use crate::backend::{FailOutcome, WorkItem};
use crate::event::EventKind;

/// What the lease sweep did with an expired item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepOutcome {
    /// Attempts remain: the item is pending again.
    Retry,
    /// Attempts are spent: the item is dead-lettered.
    Exhausted,
}

/// The sweep's events for `item`, which carries its attempt number after
/// the sweep. A retry records the failed attempt and the retry that follows
/// it; an exhausted item records its final failure only.
pub fn sweep_events(item: &WorkItem, outcome: SweepOutcome) -> Vec<EventKind> {
    match outcome {
        SweepOutcome::Retry => vec![
            EventKind::NodeFailed {
                node_id: item.node_id.clone(),
                error: "lease expired: worker presumed dead".into(),
                attempt: item.attempt.saturating_sub(1),
                retryable: true,
            },
            EventKind::RetryScheduled {
                node_id: item.node_id.clone(),
                attempt: item.attempt,
                delay_ms: (1u64 << item.attempt.min(6)) * 1000,
            },
        ],
        SweepOutcome::Exhausted => vec![EventKind::NodeFailed {
            node_id: item.node_id.clone(),
            error: format!("exhausted {} attempts: lease expired", item.max_attempts),
            attempt: item.attempt,
            retryable: false,
        }],
    }
}

/// The events of a worker's failure report that `outcome` settled, with the
/// worker's `error`. They mirror [`sweep_events`]: a retry records the
/// attempt that just failed and then the retry, an exhausted item its final
/// attempt count.
pub fn fail_events(outcome: &FailOutcome, error: &str) -> Vec<EventKind> {
    let (item, delay_ms) = match outcome {
        FailOutcome::Retryable { item, delay_ms } => (item, Some(*delay_ms)),
        FailOutcome::Exhausted { item } => (item, None),
    };
    let retryable = delay_ms.is_some();
    let mut events = vec![EventKind::NodeFailed {
        node_id: item.node_id.clone(),
        error: error.to_string(),
        attempt: if retryable {
            item.attempt.saturating_sub(1)
        } else {
            item.attempt
        },
        retryable,
    }];
    if let Some(delay_ms) = delay_ms {
        events.push(EventKind::RetryScheduled {
            node_id: item.node_id.clone(),
            attempt: item.attempt,
            delay_ms,
        });
    }
    events
}
