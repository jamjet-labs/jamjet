//! The driver: what moves a run when no loop does.
//!
//! With `JAMJET_BACKGROUND=off` nothing in the process schedules, claims or
//! sweeps on its own. The driver does that work when something calls it, and
//! only then: inside a request that just wrote to a run, and on a tick.
//!
//! [`Driver::advance`] takes one run as far as it can without waiting for a
//! person or a worker: it builds the scheduler's pass for the run, and runs
//! the engine's own quick steps (a condition, an approval park, a limit) that
//! the pass dispatched, as the engine and not as whoever's request got it
//! there, then builds the next pass. It stops at a step another process runs
//! (an external worker's, or a model call, which is left for the tick), at
//! the end of the run, at its budget, or when the server is stopping.
//!
//! Nothing an advance does reaches the caller as an error. A failure is
//! logged and counted in the [`AdvanceReport`]: the write that led to it has
//! already committed, and the tick will go on from the log.

use crate::state::AppState;
use jamjet_core::workflow::ExecutionId;
use jamjet_models::ModelRegistry;
use jamjet_scheduler::{Dispatched, Scheduler};
use jamjet_state::ClaimFilter;
use jamjet_worker::{Shutdown, WorkerPool};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::warn;

pub use crate::startup::DriverConfig;

/// The node kinds an advance runs itself: quick, and needing nothing from
/// outside the process. A model or eval step may take minutes and costs
/// money, so it waits for the tick, which has a budget made for it.
pub const QUICK_KINDS: &[&str] = &["condition", "human_approval", "limit_exceeded"];

/// The most passes one advance builds. A run that keeps handing the engine a
/// quick step still returns to its caller, and the tick goes on from there.
const MAX_PASSES: usize = 64;

/// What one [`Driver::advance`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AdvanceReport {
    /// The nodes the passes dispatched, each with an item on its queue.
    pub dispatched: u32,
    /// The engine's own steps it ran.
    pub engine_steps: u32,
    /// Passes the store refused as stale and the scheduler built again: some
    /// other writer (a tick, another engine, a worker) moved the run's log
    /// while this advance was deciding.
    pub stale_passes: u32,
    /// Failures, logged and not returned: a pass that failed, a step that
    /// failed or panicked.
    pub errors: u32,
    /// The budget ran out with a quick step still to run. Its item is left on
    /// the queue for the tick.
    pub out_of_budget: bool,
    /// The run is finished.
    pub terminal: bool,
}

/// Advances runs and runs the engine's own steps on request.
pub struct Driver {
    scheduler: Arc<Scheduler>,
    /// The engine's own workers: the server's in-process executors on the
    /// unscoped backend, sealing approval requests into the audit chain. Not
    /// spawned: a worker is built from it for each step.
    engine: WorkerPool,
    shutdown: Shutdown,
    config: DriverConfig,
    /// Names this process in the ids of the workers it builds.
    instance: String,
}

impl Driver {
    /// A driver for the engine `state` holds, with the server's own
    /// in-process executors. `state.backend` is the unscoped store, which the
    /// scheduler and the workers read across tenants, as the loops do.
    pub fn new(
        state: &AppState,
        models: Arc<ModelRegistry>,
        shutdown: Shutdown,
        config: DriverConfig,
    ) -> Self {
        let engine = jamjet_worker::server_pool(state.backend.clone(), models)
            .with_sealer(state.enricher.clone())
            .with_agents(state.agents.clone());
        Self::with_engine(state, engine, shutdown, config)
    }

    /// [`new`](Self::new) with the engine's workers given, for a test that
    /// swaps an executor. `engine` must be built on `state.backend`.
    pub fn with_engine(
        state: &AppState,
        engine: WorkerPool,
        shutdown: Shutdown,
        config: DriverConfig,
    ) -> Self {
        let scheduler = Scheduler::new(state.backend.clone()).with_sealer(state.enricher.clone());
        Self {
            scheduler: Arc::new(scheduler),
            engine,
            shutdown,
            config,
            instance: short_id(),
        }
    }

    /// The scheduler every pass of this driver goes through.
    pub fn scheduler(&self) -> &Arc<Scheduler> {
        &self.scheduler
    }

    /// The settings this driver was built with.
    pub fn config(&self) -> &DriverConfig {
        &self.config
    }

    /// Take `execution` as far as the engine can alone, within `budget`.
    ///
    /// At least one engine step runs per advance whatever the budget, so a
    /// tiny one still makes progress; after that, the budget is checked
    /// before each step, and a step in hand always finishes. The advance
    /// counts as running work for the drain
    /// ([`Shutdown::track`](jamjet_worker::Shutdown::track)) and starts
    /// nothing once the server is stopping.
    ///
    /// A run that is finished, not running or unknown writes nothing.
    pub async fn advance(&self, execution: &ExecutionId, budget: Duration) -> AdvanceReport {
        // A budget too large to add to the clock is no deadline at all.
        let deadline = Instant::now().checked_add(budget);
        let past_deadline = || deadline.is_some_and(|d| Instant::now() >= d);
        let _running = self.shutdown.track();
        let mut report = AdvanceReport::default();
        for _ in 0..MAX_PASSES {
            if self.shutdown.stopping() {
                break;
            }
            let pass = match self.scheduler.advance_one(execution).await {
                Ok(pass) => pass,
                Err(e) => {
                    warn!(execution_id = %execution, "advance: the scheduler pass failed: {e}");
                    report.errors += 1;
                    break;
                }
            };
            report.stale_passes += pass.stale;
            report.dispatched += pass.dispatched.len() as u32;
            if pass.terminal {
                report.terminal = true;
                break;
            }
            // Only what this pass dispatched. A quick item an earlier advance
            // left (it ran out of budget), or another engine's, is the
            // tick's: it polls the engine queues without a filter.
            let quick: Vec<&Dispatched> = pass
                .dispatched
                .iter()
                .filter(|d| QUICK_KINDS.contains(&d.kind.as_str()))
                .collect();
            if quick.is_empty() {
                break;
            }
            for step in quick {
                if self.shutdown.stopping() {
                    return report;
                }
                if past_deadline() && report.engine_steps > 0 {
                    report.out_of_budget = true;
                    return report;
                }
                match self.run_step(execution, step).await {
                    Step::Ran => report.engine_steps += 1,
                    Step::Nothing => {}
                    Step::Failed => report.errors += 1,
                }
            }
        }
        report
    }

    /// Run the item this pass dispatched for `step`, as the engine: a worker
    /// of its own (`engine-<instance>-<id>`) claims it by run and node, so it
    /// takes that item and no other, and runs it on the engine's executors.
    ///
    /// The step runs in a task of its own that counts as running work. A
    /// request dropped by its client halfway through must not cut the step
    /// off with its lease held until it expires: the step finishes. The same
    /// task turns a panic in an executor into a counted failure.
    async fn run_step(&self, execution: &ExecutionId, step: &Dispatched) -> Step {
        let worker = self.engine.build_worker(
            format!("engine-{}-{}", self.instance, short_id()),
            vec![step.queue.clone()],
        );
        let filter = ClaimFilter {
            execution_id: Some(execution.clone()),
            node_id: Some(step.node_id.clone()),
        };
        let running = self.shutdown.track();
        let task = tokio::spawn(async move {
            let _running = running;
            worker
                .run_item_for(&filter)
                .await
                .map_err(|e| e.to_string())
        });
        match task.await {
            Ok(Ok(Some(()))) => Step::Ran,
            Ok(Ok(None)) => Step::Nothing,
            Ok(Err(e)) => {
                warn!(
                    execution_id = %execution,
                    node_id = %step.node_id,
                    "advance: an engine step failed: {e}"
                );
                Step::Failed
            }
            Err(e) => {
                warn!(
                    execution_id = %execution,
                    node_id = %step.node_id,
                    "advance: an engine step did not finish: {e}"
                );
                Step::Failed
            }
        }
    }
}

/// How one engine step of an advance ended.
enum Step {
    /// An item was claimed and run to the end.
    Ran,
    /// No pending item matched: another process took it.
    Nothing,
    Failed,
}

/// Eight hex characters, enough to tell two workers or two instances apart.
fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamjet_worker::executable::IN_PROCESS_KINDS;

    /// An advance runs these itself, so the engine must have an executor for
    /// each, and none may need a model or leave the process.
    #[test]
    fn quick_kinds_are_engine_kinds_and_never_a_model_call() {
        for kind in QUICK_KINDS {
            assert!(
                IN_PROCESS_KINDS.contains(kind),
                "{kind} has no executor in the engine's pool"
            );
        }
        for slow in ["model", "eval"] {
            assert!(!QUICK_KINDS.contains(&slow), "{slow} waits for the tick");
        }
    }

    /// A quick step spends nothing: it is one of the kinds the run's caps
    /// never hold back, so an advance cannot spend a run's budget.
    #[test]
    fn quick_kinds_spend_nothing() {
        for kind in QUICK_KINDS {
            assert!(
                jamjet_worker::node_guard::ZERO_COST_KINDS.contains(kind),
                "{kind} can spend"
            );
        }
    }
}
