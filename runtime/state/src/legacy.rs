//! The old step-by-step bodies of the storage methods that now write in one
//! step. They are the trait's defaults, so a `StateBackend` implemented
//! outside this repository keeps compiling and keeps the behaviour it had.
//! Every backend in this crate overrides them.

use crate::backend::{BackendResult, ExecutionFinish, PassCommit, SchedulerPass, StateBackend};
use crate::event::Event;

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
