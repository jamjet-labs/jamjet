//! What a live run is waiting for.
//!
//! Derived on read from the run's event log and its open work items. Nothing
//! is stored. A caller that starts workers on demand reads it to decide
//! whether one is needed.

use chrono::{DateTime, Utc};
use jamjet_core::workflow::WorkflowExecution;
use jamjet_state::approvals::pending_approvals;
use jamjet_state::backend::{BackendResult, StateBackend};
use jamjet_worker::executable::ENGINE_QUEUES;
use serde::Serialize;

/// Who a live run waits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitingFor {
    /// A reviewer's decision.
    Approval,
    /// A step a worker holds under a live lease.
    Step,
    /// A worker to claim the next step.
    Worker,
    /// The engine: its own steps, or the next scheduler pass.
    Engine,
}

impl WaitingFor {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "approval" => Some(Self::Approval),
            "step" => Some(Self::Step),
            "worker" => Some(Self::Worker),
            "engine" => Some(Self::Engine),
            _ => None,
        }
    }
}

/// `None` for a finished run. Otherwise, first match wins:
/// an open request whose deadline has not passed is `Approval`;
/// a claimed item whose lease has not expired is `Step`;
/// a pending item on a queue outside `ENGINE_QUEUES` is `Worker`;
/// anything else is `Engine`: an item on an engine queue, no item between
/// scheduler passes, and an expired request no sweep has recorded yet.
///
/// A claimed item whose lease has expired counts as pending: it can be
/// claimed again, so it is `Worker` on a worker queue and `Engine` on an
/// engine queue.
pub async fn waiting_for(
    b: &dyn StateBackend,
    exec: &WorkflowExecution,
    now: DateTime<Utc>,
) -> BackendResult<Option<WaitingFor>> {
    if exec.status.is_terminal() {
        return Ok(None);
    }
    let events = b.get_events(&exec.execution_id).await?;
    if pending_approvals(&events)
        .iter()
        .any(|p| !p.deadline().expired(now))
    {
        return Ok(Some(WaitingFor::Approval));
    }
    let items = b.open_work_items(&exec.execution_id).await?;
    let held =
        |i: &jamjet_state::OpenWorkItem| i.claimed && i.lease_expires_at.is_some_and(|t| t > now);
    if items.iter().any(held) {
        return Ok(Some(WaitingFor::Step));
    }
    if items
        .iter()
        .any(|i| !ENGINE_QUEUES.contains(&i.queue_type.as_str()))
    {
        return Ok(Some(WaitingFor::Worker));
    }
    Ok(Some(WaitingFor::Engine))
}
