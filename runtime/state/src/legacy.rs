//! The old step-by-step bodies of the storage methods that now write in one
//! step. They are the trait's defaults, so a `StateBackend` implemented
//! outside this repository keeps compiling and keeps the behaviour it had.
//! Every backend in this crate overrides them.

use crate::backend::{
    BackendResult, ExecutionFinish, FailOutcome, PassCommit, ReclaimResult, SchedulerPass,
    StateBackend, WorkItemId,
};
use crate::event::{Event, EventKind};
use crate::lease_events::{fail_events, sweep_events, SweepOutcome};
use jamjet_core::workflow::ExecutionId;

/// [`StateBackend::commit_scheduler_pass`] the way the scheduler wrote a pass
/// before the method existed: each event with `append_event` (its sequence
/// proposed as the latest plus one, as the scheduler proposed it), each item
/// with `enqueue_work_item`, then the finish with `finish_execution`.
///
/// A pass that does not hold together is refused before anything is
/// written, as every backend refuses it. Nothing else is checked: not the
/// log, not live items, not the run's status. A failure part way leaves the
/// earlier writes in place. When `finish_execution` finds the run already
/// terminal the answer is [`PassCommit::ExecutionTerminal`], although the
/// pass's events and items before it have landed.
pub(crate) async fn scheduler_pass_in_steps<B>(
    backend: &B,
    pass: SchedulerPass,
) -> BackendResult<PassCommit>
where
    B: StateBackend + ?Sized,
{
    pass.check_shape()?;
    let mut last_sequence = pass.seen_sequence;
    for kind in pass.events {
        let proposed = backend.latest_sequence(&pass.execution_id).await? + 1;
        last_sequence = backend
            .append_event(Event::new(pass.execution_id.clone(), proposed, kind))
            .await?;
    }
    for item in pass.work_items {
        backend.enqueue_work_item(item).await?;
    }
    if let Some((status, kind)) = pass.finish {
        match backend
            .finish_execution(Event::new(pass.execution_id.clone(), 0, kind), status)
            .await?
        {
            ExecutionFinish::Finished(sequence) => last_sequence = sequence,
            ExecutionFinish::AlreadyTerminal(prior) => {
                return Ok(PassCommit::ExecutionTerminal(prior))
            }
        }
    }
    Ok(PassCommit::Committed { last_sequence })
}

/// [`StateBackend::reclaim_expired_leases_recorded`] the way the scheduler's
/// sweep wrote it before the method existed: [`StateBackend::reclaim_expired_leases`],
/// then [`sweep_events`] for each requeued item and then for each
/// dead-lettered one, appended one by one.
///
/// Nothing is checked: an item of a finished run is reclaimed and recorded
/// like any other. A failed append leaves the settles in place and the
/// later items' events unwritten.
pub(crate) async fn reclaim_in_steps<B>(backend: &B) -> BackendResult<ReclaimResult>
where
    B: StateBackend + ?Sized,
{
    let reclaimed = backend.reclaim_expired_leases().await?;
    for item in &reclaimed.retryable {
        append_in_steps(
            backend,
            &item.execution_id,
            sweep_events(item, SweepOutcome::Retry),
        )
        .await?;
    }
    for item in &reclaimed.exhausted {
        append_in_steps(
            backend,
            &item.execution_id,
            sweep_events(item, SweepOutcome::Exhausted),
        )
        .await?;
    }
    Ok(reclaimed)
}

/// [`StateBackend::fail_work_item_fenced_recorded`] the way the fail route
/// wrote it before the method existed: [`StateBackend::fail_work_item_fenced`],
/// then [`fail_events`] appended one by one.
///
/// The run's status is not checked. A failed append leaves the settle in
/// place with nothing, or only part of its trail, recorded.
pub(crate) async fn fail_in_steps<B>(
    backend: &B,
    item_id: WorkItemId,
    lease_fence: i64,
    error: &str,
) -> BackendResult<Option<FailOutcome>>
where
    B: StateBackend + ?Sized,
{
    let Some(outcome) = backend
        .fail_work_item_fenced(item_id, lease_fence, error)
        .await?
    else {
        return Ok(None);
    };
    let execution_id = match &outcome {
        FailOutcome::Retryable { item, .. } | FailOutcome::Exhausted { item } => {
            item.execution_id.clone()
        }
    };
    append_in_steps(backend, &execution_id, fail_events(&outcome, error)).await?;
    Ok(Some(outcome))
}

/// Append each of `kinds` with `append_event`, its sequence proposed as the
/// latest plus one, as the scheduler and the fail route proposed it.
async fn append_in_steps<B>(
    backend: &B,
    execution_id: &ExecutionId,
    kinds: Vec<EventKind>,
) -> BackendResult<()>
where
    B: StateBackend + ?Sized,
{
    for kind in kinds {
        let proposed = backend.latest_sequence(execution_id).await? + 1;
        backend
            .append_event(Event::new(execution_id.clone(), proposed, kind))
            .await?;
    }
    Ok(())
}
