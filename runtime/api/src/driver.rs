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
//! [`Driver::tick`] is the sweep a loop would make, once, within a budget:
//! lapsed leases, the audit outbox, an advance of each running run, the
//! engine steps still on the queues (model and eval calls among them), and
//! the projector. Two ticks, on one engine or two, never run at once: a lock
//! in the process and a lease in the store collapse them to one. The lease
//! saves work and the lock saves a store round trip; neither is what keeps a
//! record from being written twice, the store's conditional writes do.
//!
//! Nothing an advance does reaches the caller as an error. A failure is
//! logged and counted in the [`AdvanceReport`]: the write that led to it has
//! already committed, and the tick will go on from the log.
//!
//! [`TickWatch`] notices when nobody ticks: with the loops off and no tick
//! for three intervals, `/health` says `tick_overdue: true` and requests
//! write an error line saying what has stopped.

use crate::routes::Draining;
use crate::startup::Background;
use crate::state::AppState;
use axum::Extension;
use jamjet_audit::AuditEnricher;
use jamjet_core::workflow::ExecutionId;
use jamjet_models::ModelRegistry;
use jamjet_scheduler::{Dispatched, Projector, Reclaimed, Scheduler};
use jamjet_state::{
    ClaimFilter, PgBackend, StateBackend, StateBackendError, TickBegin, TickOutcome,
};
use jamjet_worker::executable::ENGINE_QUEUES;
use jamjet_worker::{Shutdown, WorkerPool};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

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
    /// Approval requests past their deadline that the passes expired, and
    /// escalated. An expiry is folded by the pass that follows it, so a
    /// fallback or a failed node lands in the same advance.
    pub expired: u32,
    pub escalated: u32,
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
    /// Names this process in the ids of the workers it builds, and in the
    /// holder of the tick lease.
    instance: String,
    /// The unscoped store: ticks are taken and listed on it.
    backend: Arc<dyn StateBackend>,
    projector: Projector,
    enricher: Arc<AuditEnricher>,
    /// The Postgres store whose audit outbox a tick drains. `None` on every
    /// other store: only Postgres has an outbox.
    outbox: Option<PgBackend>,
    /// At most one tick runs in this process, and a second caller is told so
    /// without a round trip to the store.
    tick_lock: Arc<tokio::sync::Mutex<()>>,
    /// Whether the engine's own loops run beside this driver, as the tick's
    /// report says.
    background: Background,
    /// Told when a tick of this driver has finished and saved its state.
    watch: Option<Arc<TickWatch>>,
    /// The tick's lease in the store, and its renewal.
    lease: TickLease,
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
            backend: state.backend.clone(),
            projector: Projector::new(state.backend.clone()),
            enricher: state.enricher.clone(),
            outbox: None,
            tick_lock: Arc::new(tokio::sync::Mutex::new(())),
            background: Background::Off,
            watch: None,
            lease: TickLease::default(),
        }
    }

    /// Have a tick drain the audit outbox of this Postgres store. Without
    /// it a tick drains nothing: only Postgres has an outbox.
    pub fn with_outbox(mut self, postgres: PgBackend) -> Self {
        self.outbox = Some(postgres);
        self
    }

    /// Say whether the engine's loops run beside this driver. Only the
    /// tick's report reads it.
    pub fn with_background(mut self, background: Background) -> Self {
        self.background = background;
        self
    }

    /// Whether the engine's loops run beside this driver.
    pub fn background(&self) -> Background {
        self.background
    }

    /// Tell `watch` when a tick of this driver has finished and saved its
    /// state, so `/health` stops calling the tick overdue at once.
    pub fn with_watch(mut self, watch: Arc<TickWatch>) -> Self {
        self.watch = Some(watch);
        self
    }

    /// Another tick lease than the default, for a test that cannot wait
    /// for half a minute.
    pub fn with_tick_lease(mut self, lease: TickLease) -> Self {
        self.lease = lease;
        self
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
            report.expired += pass.expired;
            report.escalated += pass.escalated;
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
                // A pass whose sweep expired an approval wrote the expiry and
                // nothing else, and one that recorded a fallback taken or a
                // node failed or skipped dispatched nothing yet. The next
                // pass folds what was written and dispatches what it
                // unblocks.
                if pass.expired > 0 || pass.recorded > 0 {
                    continue;
                }
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

// ── Request-driven mode: the tick ───────────────────────────────────────────

/// The shortest budget a tick takes. A smaller one cannot visit a run.
pub const MIN_TICK_BUDGET: Duration = Duration::from_millis(100);

/// The tick's lease in the store, and how often a running tick renews it.
///
/// The lease is short and renewed, not sized to the longest step: a tick
/// waits for every engine step it started, and a model call alone may take
/// minutes. So a lease lapses only once its holder has stopped renewing it,
/// and a tick that dies frees the store's tick within one lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickLease {
    /// 30 s: half the default tick interval, so a tick that died never makes
    /// the next scheduled one answer busy.
    pub lease: Duration,
    /// 10 s, a third of the lease, as a worker heartbeats its item: a
    /// renewal that fails, or waits for a connection, leaves time for the
    /// next before the lease lapses.
    pub renew_every: Duration,
}

impl Default for TickLease {
    fn default() -> Self {
        Self {
            lease: Duration::from_secs(30),
            renew_every: Duration::from_secs(10),
        }
    }
}

/// Outbox rows one tick seals.
const OUTBOX_LIMIT: u32 = 500;

/// Runs listed at a time by the walk.
const WALK_PAGE: u32 = 100;

/// The projector gets at least this long, whatever the steps left of the
/// budget: it is incremental, and a tick whose steps overran would otherwise
/// never project.
const PROJECTOR_FLOOR: Duration = Duration::from_secs(1);

/// What a tick reclaimed: items whose lease had lapsed, put back for another
/// attempt, and items that had used every attempt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReclaimedReport {
    pub retried: u64,
    pub dead_lettered: u64,
}

/// What a tick's walk did to the running runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RunsReport {
    /// Runs it advanced.
    pub visited: u64,
    /// Of those, the ones that moved: a node dispatched or run, an approval
    /// expired or escalated, or the run finished.
    pub advanced: u64,
    /// Passes the store refused as stale and the scheduler built again.
    pub stale_passes: u64,
    pub errors: u64,
}

/// The engine's own steps a tick ran.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StepsReport {
    /// Run by an advance, right after the pass that dispatched them.
    pub quick: u64,
    /// Run from the queues after the walk: model and eval calls, and any
    /// step no advance reached.
    pub slow: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DeadlinesReport {
    pub expired: u64,
    pub escalated: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct OutboxReport {
    pub sealed: u64,
    pub failed: u64,
}

/// What one tick did, as `POST /ops/tick` answers and the store keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TickReport {
    /// Always true here: a tick that did not run is a [`TickResult::Busy`].
    pub ran: bool,
    /// The tick got through everything within its budget: the walk met the
    /// end of the running runs, the queues were found empty, and nothing it
    /// read failed. When false the next tick goes on.
    pub complete: bool,
    pub elapsed_ms: u64,
    pub budget_ms: u64,
    /// How far past its budget the tick ran, waiting for the steps it had
    /// started. Zero for a tick that fit.
    pub over_budget_ms: u64,
    pub reclaimed: ReclaimedReport,
    pub runs: RunsReport,
    pub engine_steps: StepsReport,
    pub deadlines: DeadlinesReport,
    pub audit_outbox: OutboxReport,
    /// Approval rows the projector wrote.
    pub projected: u64,
    /// The run the walk stopped after, where the next tick resumes. `None`
    /// when the walk reached the end.
    pub next_after: Option<String>,
    /// `on` or `off`: whether the engine's loops run beside the tick.
    pub background: &'static str,
}

/// How a call to [`Driver::tick`] ended.
#[derive(Debug)]
pub enum TickResult {
    Ran(Box<TickReport>),
    /// Another tick holds the lease: one in this process
    /// (`held_by == "this instance"`) or the holder the store names.
    Busy {
        held_by: String,
        lease_until: Option<chrono::DateTime<chrono::Utc>>,
    },
    /// The server is stopping, and a tick starts nothing new.
    Stopping,
}

/// The seam after a tick has taken its lease. An error a test arms here
/// leaves the tick as an engine that stopped there would: the lease held
/// until it lapses, nothing else done.
async fn after_begin_seam() -> jamjet_state::BackendResult<()> {
    jamjet_state::fault_point!("tick.after_begin", 1);
    Ok(())
}

/// The seam after each run the walk advanced. An error a test arms here
/// stops the tick between two runs, as a killed engine would.
async fn mid_walk_seam() -> jamjet_state::BackendResult<()> {
    jamjet_state::fault_point!("tick.mid_walk", 1);
    Ok(())
}

/// Renews a tick's lease every [`TickLease::renew_every`] until it is
/// dropped. A renewal the store refuses because another holder has the
/// lease ends it; one that fails is tried again at the next turn.
struct Renewal {
    /// Dropped with the guard, which stops the renewals. A renewal in
    /// flight then finishes, and the loop ends at its next turn.
    _stop: tokio::sync::oneshot::Sender<()>,
}

impl Renewal {
    fn start(backend: Arc<dyn StateBackend>, holder: String, lease: TickLease) -> Self {
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stopped => return,
                    _ = tokio::time::sleep(lease.renew_every) => {}
                }
                match backend.renew_tick(&holder, lease.lease).await {
                    Ok(true) => {}
                    Ok(false) => {
                        warn!("tick lease lost: another tick took over while this one ran");
                        return;
                    }
                    Err(e) => warn!("tick: the lease could not be renewed: {e}"),
                }
            }
        });
        Self { _stop: stop }
    }
}

/// What the walk over the running runs did.
#[derive(Default)]
struct Walk {
    runs: RunsReport,
    quick: u64,
    expired: u64,
    escalated: u64,
    /// The last run advanced.
    last: Option<String>,
    /// The budget ran out, the server began to stop, or a page could not be
    /// read, before the end.
    cut: bool,
}

/// What the engine-step phase did.
#[derive(Default)]
struct Queued {
    slow: u64,
    errors: u64,
    /// Some poll found the queues empty.
    drained: bool,
}

impl Driver {
    /// One tick, within `budget` (held to `[MIN_TICK_BUDGET, the configured
    /// tick budget]`): lapsed leases, the audit outbox, an advance of each
    /// running run from where the last tick stopped, the engine steps still
    /// on the queues, and the projector.
    ///
    /// At most one tick runs at a time: a second caller on this process is
    /// told so by the lock, and one on another engine by the store's lease.
    /// The tick counts as running work for the drain, and runs in a task of
    /// its own: a caller that gives up (a scheduler's attempt deadline) does
    /// not cut it off between two phases with the lease held. At least one
    /// run is advanced per tick whatever the budget, so a tick always moves
    /// the cursor on.
    ///
    /// An `Err` is the store failing a call the tick cannot do without: it
    /// cannot take the lease (or has no tick state at all).
    pub async fn tick(self: &Arc<Self>, budget: Duration) -> Result<TickResult, StateBackendError> {
        // Counted before anything is decided, so a stop signal that arrives
        // from here on waits for the tick.
        let running = self.shutdown.track();
        if self.shutdown.stopping() {
            return Ok(TickResult::Stopping);
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let _running = running;
            this.run_tick(budget).await
        })
        .await
        .map_err(|e| StateBackendError::Database(format!("the tick did not finish: {e}")))?
    }

    async fn run_tick(&self, budget: Duration) -> Result<TickResult, StateBackendError> {
        let started = Instant::now();
        let budget = budget
            .max(MIN_TICK_BUDGET)
            .min(self.config.tick_budget.max(MIN_TICK_BUDGET));
        let Ok(_local) = self.tick_lock.clone().try_lock_owned() else {
            return Ok(TickResult::Busy {
                held_by: "this instance".into(),
                lease_until: None,
            });
        };
        let holder = format!("{}:{}", self.instance, uuid::Uuid::new_v4().simple());
        let cursor = match self.backend.begin_tick(&holder, self.lease.lease).await? {
            TickBegin::Began { cursor, .. } => cursor,
            TickBegin::Busy {
                holder,
                lease_until,
            } => {
                return Ok(TickResult::Busy {
                    held_by: holder,
                    lease_until: Some(lease_until),
                })
            }
        };
        // Renewed while this tick runs, however long the steps it waits for
        // take, and no longer once it ends, by any path.
        let renewal = Renewal::start(self.backend.clone(), holder.clone(), self.lease);
        after_begin_seam().await?;
        let mut complete = true;

        // 1. Leases that lapsed: put the items back, or give up on them.
        let reclaimed = match self.scheduler.reclaim_once().await {
            Ok(r) => r,
            Err(e) => {
                warn!("tick: lapsed leases could not be reclaimed: {e}");
                complete = false;
                Reclaimed::default()
            }
        };

        // 2. The audit entries whose inline seal was lost (Postgres only).
        let mut outbox = OutboxReport::default();
        if let Some(pg) = &self.outbox {
            let drained = jamjet_audit::outbox::drain(
                pg,
                &self.enricher,
                jamjet_audit::outbox::DUE_AFTER,
                OUTBOX_LIMIT,
            )
            .await;
            outbox.sealed = drained.sealed as u64;
            outbox.failed = drained.failed as u64;
            if drained.error.is_some() {
                complete = false;
            }
        }

        // 3. Each running run, from where the last tick stopped.
        let walk = self.walk(cursor.clone(), started, budget).await?;
        complete &= !walk.cut;

        // 4. The engine steps still on the queues.
        let queued = self.run_queued_steps(started, budget).await;
        complete &= queued.drained && queued.errors == 0;

        // 5. The read model.
        let mut projected = 0;
        if self.shutdown.stopping() {
            complete = false;
        } else {
            let left = budget
                .saturating_sub(started.elapsed())
                .max(PROJECTOR_FLOOR);
            match tokio::time::timeout(left, self.projector.tick()).await {
                Ok(Ok(n)) => projected = n as u64,
                Ok(Err(e)) => {
                    warn!("tick: the projector failed: {e}");
                    complete = false;
                }
                Err(_) => {
                    warn!("tick: the projector did not finish in {left:?}");
                    complete = false;
                }
            }
        }

        // Where the next tick resumes: after the last run this one advanced,
        // or from the start once the walk met the end. A walk cut before it
        // advanced anything leaves the cursor as it was.
        let next_after = if walk.cut {
            walk.last.clone().or(cursor)
        } else {
            None
        };
        let elapsed = started.elapsed();
        let report = TickReport {
            ran: true,
            complete,
            elapsed_ms: elapsed.as_millis() as u64,
            budget_ms: budget.as_millis() as u64,
            over_budget_ms: elapsed.saturating_sub(budget).as_millis() as u64,
            reclaimed: ReclaimedReport {
                retried: reclaimed.retried as u64,
                dead_lettered: reclaimed.dead_lettered as u64,
            },
            runs: walk.runs,
            engine_steps: StepsReport {
                quick: walk.quick,
                slow: queued.slow,
            },
            deadlines: DeadlinesReport {
                expired: walk.expired,
                escalated: walk.escalated,
            },
            audit_outbox: outbox,
            projected,
            next_after: next_after.clone(),
            background: self.background.label(),
        };
        let worked = report.runs.advanced
            + report.engine_steps.quick
            + report.engine_steps.slow
            + report.reclaimed.retried
            + report.reclaimed.dead_lettered
            + report.deadlines.expired
            + report.deadlines.escalated
            + report.audit_outbox.sealed;
        if worked > 0 || !report.complete {
            info!(
                complete = report.complete,
                elapsed_ms = report.elapsed_ms,
                over_budget_ms = report.over_budget_ms,
                visited = report.runs.visited,
                advanced = report.runs.advanced,
                quick = report.engine_steps.quick,
                slow = report.engine_steps.slow,
                expired = report.deadlines.expired,
                "tick finished"
            );
        }
        let outcome = TickOutcome {
            cursor: next_after,
            complete,
            report: serde_json::to_value(&report).unwrap_or(serde_json::Value::Null),
        };
        // A renewal still in flight names this holder, so once the finish
        // has given the lease back it changes nothing.
        drop(renewal);
        match self.backend.finish_tick(&holder, &outcome).await {
            // Only a finish the store saved: a tick it did not record is one
            // no other engine sees either.
            Ok(true) => {
                if let Some(watch) = &self.watch {
                    watch.tick_finished();
                }
            }
            Ok(false) => warn!("tick lease lost: another tick took over before this one finished"),
            Err(e) => warn!("tick: its state could not be saved: {e}"),
        }
        Ok(TickResult::Ran(Box::new(report)))
    }

    /// Advance the running runs after `cursor` to the end, then from the
    /// start up to `cursor`, one at a time, each within the tick's remaining
    /// budget or the inline budget, whichever is less. Stops when the budget
    /// is spent (after at least one run) or the server begins to stop.
    async fn walk(
        &self,
        cursor: Option<String>,
        started: Instant,
        budget: Duration,
    ) -> Result<Walk, StateBackendError> {
        let mut walk = Walk::default();
        // Runs are listed in id order, so the cursor splits the order in two.
        let mut segments = vec![(cursor.clone(), None)];
        if cursor.is_some() {
            segments.push((None, cursor));
        }
        'segments: for (from, upto) in segments {
            let mut after = from;
            'pages: loop {
                let page = match self
                    .backend
                    .list_running_executions_after(after.as_deref(), WALK_PAGE)
                    .await
                {
                    Ok(page) => page,
                    Err(e) => {
                        warn!("tick: the running runs could not be listed: {e}");
                        walk.runs.errors += 1;
                        walk.cut = true;
                        break 'segments;
                    }
                };
                for run in &page.executions {
                    let id = run.execution_id.0.to_string();
                    if upto.as_deref().is_some_and(|end| id.as_str() > end) {
                        break 'pages;
                    }
                    let spent = started.elapsed() >= budget;
                    if self.shutdown.stopping() || (spent && walk.runs.visited > 0) {
                        walk.cut = true;
                        break 'segments;
                    }
                    let room = budget
                        .saturating_sub(started.elapsed())
                        .min(self.config.inline_budget);
                    let advanced = self.advance(&run.execution_id, room).await;
                    walk.runs.visited += 1;
                    walk.runs.stale_passes += u64::from(advanced.stale_passes);
                    walk.runs.errors += u64::from(advanced.errors);
                    walk.quick += u64::from(advanced.engine_steps);
                    walk.expired += u64::from(advanced.expired);
                    walk.escalated += u64::from(advanced.escalated);
                    if advanced.dispatched > 0
                        || advanced.engine_steps > 0
                        || advanced.expired > 0
                        || advanced.escalated > 0
                        || advanced.terminal
                    {
                        walk.runs.advanced += 1;
                    }
                    walk.last = Some(id);
                    mid_walk_seam().await?;
                }
                match page.next_after {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
        }
        Ok(walk)
    }

    /// Run the steps left on the engine's queues: model and eval calls, and
    /// any quick step no advance ran. Up to `engine_concurrency` workers take
    /// items while the tick has used less than half its budget (at least one
    /// poll whatever the budget), until one finds the queues empty. The tick
    /// waits for every step it started, however long it runs: a step in hand
    /// always finishes.
    async fn run_queued_steps(&self, started: Instant, budget: Duration) -> Queued {
        let half = budget / 2;
        let polled = Arc::new(AtomicBool::new(false));
        let drained = Arc::new(AtomicBool::new(false));
        let slow = Arc::new(AtomicU32::new(0));
        let errors = Arc::new(AtomicU32::new(0));
        let mut tasks = Vec::new();
        for _ in 0..self.config.engine_concurrency.max(1) {
            // Unfiltered: any item on the engine's own queues.
            let worker = self.engine.build_worker(
                format!("engine-{}-{}", self.instance, short_id()),
                ENGINE_QUEUES.iter().map(|q| q.to_string()).collect(),
            );
            let shutdown = self.shutdown.clone();
            let running = self.shutdown.track();
            let (polled, drained) = (polled.clone(), drained.clone());
            let (slow, errors) = (slow.clone(), errors.clone());
            tasks.push(tokio::spawn(async move {
                let _running = running;
                loop {
                    if shutdown.stopping() {
                        break;
                    }
                    let first = !polled.swap(true, Ordering::SeqCst);
                    if !first && started.elapsed() >= half {
                        break;
                    }
                    match worker.poll_and_execute().await {
                        Ok(true) => {
                            slow.fetch_add(1, Ordering::SeqCst);
                        }
                        Ok(false) => {
                            drained.store(true, Ordering::SeqCst);
                            break;
                        }
                        Err(e) => {
                            warn!("tick: an engine step failed: {e}");
                            errors.fetch_add(1, Ordering::SeqCst);
                            break;
                        }
                    }
                }
            }));
        }
        for task in tasks {
            if let Err(e) = task.await {
                warn!("tick: an engine step did not finish: {e}");
                errors.fetch_add(1, Ordering::SeqCst);
            }
        }
        Queued {
            slow: u64::from(slow.load(Ordering::SeqCst)),
            errors: u64::from(errors.load(Ordering::SeqCst)),
            drained: drained.load(Ordering::SeqCst),
        }
    }
}

// ── Request-driven mode: inline advance ─────────────────────────────────────

/// How long a write's answer waits for its advance past the inline budget:
/// the advance finishes the engine step in hand when its budget runs out,
/// and a quick step takes far less than this.
pub const INLINE_ANSWER_MARGIN: Duration = Duration::from_secs(1);

/// The seam between a write that has committed and the advance that follows
/// it. An error a test arms here skips the advance, as an engine that stopped
/// there would: the write is on record, the run is not advanced, and the next
/// tick goes on from the log.
async fn after_write_seam() -> jamjet_state::BackendResult<()> {
    jamjet_state::fault_point!("inline.after_write", 1);
    Ok(())
}

/// After the handler's write has committed and before it answers: one advance
/// of that run within the inline budget. Never changes the answer. Nothing
/// happens with loops on, without the extension (every existing test builds
/// `AppState` without it), or while draining (the tick catches up).
///
/// Why before the answer: on a request-billed platform the process may get no
/// CPU after it answers, so work spawned for later may never run.
///
/// The advance runs in a task of its own that this call awaits, for at most
/// the inline budget and [`INLINE_ANSWER_MARGIN`], and not past a stop
/// signal. Past either the write answers as it would have, and the advance
/// goes on in its task, counted as running work, so the drain still waits
/// for it. The answer never waits on a slow store or a step that hangs. A
/// panic in the advance is a log line and not a failed request, and a client
/// that hangs up halfway does not cut it off between two writes: the write
/// that led here has committed, and what the advance leaves undone the tick
/// does.
pub async fn after_write(
    driver: Option<&Extension<Arc<Driver>>>,
    draining: Option<&Extension<Draining>>,
    execution: &ExecutionId,
) {
    let Some(Extension(driver)) = driver else {
        return;
    };
    if driver.background == Background::On {
        return;
    }
    if draining.is_some_and(|Extension(d)| d.0.load(Ordering::SeqCst)) {
        return;
    }
    if let Err(e) = after_write_seam().await {
        warn!(execution_id = %execution, "inline advance skipped: {e}");
        return;
    }
    let advancing = Arc::clone(driver);
    let run = execution.clone();
    let budget = driver.config.inline_budget;
    // Counted from before the task starts, so a drain that begins now waits
    // for it whether or not this request still does.
    let running = driver.shutdown.track();
    let mut advance = tokio::spawn(async move {
        let _running = running;
        advancing.advance(&run, budget).await
    });
    let waited = budget.saturating_add(INLINE_ANSWER_MARGIN);
    let mut stop = driver.shutdown.receiver();
    let stopping = async move {
        // A process whose trigger is gone never stops: wait for the rest.
        if stop.wait_for(|stopping| *stopping).await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        done = &mut advance => match done {
            Ok(report) => debug!(
                execution_id = %execution,
                dispatched = report.dispatched,
                engine_steps = report.engine_steps,
                errors = report.errors,
                out_of_budget = report.out_of_budget,
                "inline advance"
            ),
            Err(e) => warn!(execution_id = %execution, "inline advance did not finish: {e}"),
        },
        _ = tokio::time::sleep(waited) => warn!(
            execution_id = %execution,
            "inline advance still running after {waited:?}: the write answers, the advance goes on"
        ),
        _ = stopping => info!(
            execution_id = %execution,
            "the server is stopping: the write answers, its advance goes on until the drain ends"
        ),
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

// ── Request-driven mode: noticing that nobody ticks ─────────────────────────

/// How often a [`TickWatch`] reads the tick row and writes its error line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchTiming {
    /// 30 s. The requests in between answer from what the last read found.
    pub read_every: Duration,
    /// 5 minutes: at most one error line per window, per process.
    pub log_every: Duration,
    /// 2 s. A read that takes longer counts as a store that could not be
    /// read, so `/health` never waits long on a store that hangs.
    pub read_timeout: Duration,
}

impl Default for WatchTiming {
    fn default() -> Self {
        Self {
            read_every: Duration::from_secs(30),
            log_every: Duration::from_secs(300),
            read_timeout: Duration::from_secs(2),
        }
    }
}

/// Notices when nobody ticks an engine whose loops are off.
///
/// With the loops off a tick is the only thing that expires approvals,
/// reclaims lapsed leases and runs the engine's own steps, and when the
/// caller that should tick stops, nothing else fails. A tick is overdue when
/// the last one finished more than three tick intervals ago, or when none has
/// finished and this process has been up for longer than that. `/health` says
/// so in `tick_overdue`, and while it is overdue any request writes one error
/// line, at most once per [`WatchTiming::log_every`]. A tick that holds a
/// live lease in the store is running, so nothing is overdue while it runs,
/// however long the steps it waits for take.
///
/// The tick row is read at most once per [`WatchTiming::read_every`], with
/// the store's clock, so the age is taken on the clock that stamped it. A
/// tick this process finishes tells the watch at once; one another engine
/// finishes is seen at the next read. With the loops on nothing is overdue
/// and nothing is read.
pub struct TickWatch {
    backend: Arc<dyn StateBackend>,
    background: Background,
    interval: Duration,
    timing: WatchTiming,
    up_since: Instant,
    /// What the last read found, or what a tick of this process told it.
    seen: std::sync::Mutex<Option<Seen>>,
    /// A request is reading the row now. The others do not wait for it.
    reading: AtomicBool,
    /// When the error line was last written.
    logged_at: std::sync::Mutex<Option<Instant>>,
}

/// What the watch knows about the tick row, and since when.
#[derive(Clone, Copy)]
struct Seen {
    at: Instant,
    /// `None`: the row or the store's clock could not be read.
    found: Option<Found>,
}

#[derive(Clone, Copy)]
struct Found {
    last_finished_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The store's clock less this process's, at the read.
    clock_offset: chrono::Duration,
    /// At the read, a tick held a lease that had not lapsed.
    running: bool,
}

impl Found {
    /// What a read of `row` says, the store's clock `clock_offset` from this
    /// process's.
    fn read(row: Option<&jamjet_state::TickStatus>, clock_offset: chrono::Duration) -> Self {
        let store_now = jamjet_state::clock::process_now() + clock_offset;
        Self {
            last_finished_at: row.and_then(|r| r.last_finished_at),
            clock_offset,
            running: row.is_some_and(|r| {
                r.holder.is_some() && r.lease_until.is_some_and(|until| until > store_now)
            }),
        }
    }
}

/// How long since the last tick finished, and whether one is running.
#[derive(Clone, Copy)]
struct Since {
    age: Duration,
    running: bool,
}

/// The turn to read the row: one request at a time holds it, and dropping it
/// (a request that ends or is dropped mid-read) gives it back.
struct Turn<'a>(&'a AtomicBool);

impl<'a> Turn<'a> {
    fn take(flag: &'a AtomicBool) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| Self(flag))
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn locked<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl TickWatch {
    /// A watch on `backend`, the unscoped store, for an engine whose loops are
    /// as `background` says and whose caller ticks every `interval`. The
    /// process counts as up from here.
    pub fn new(backend: Arc<dyn StateBackend>, background: Background, interval: Duration) -> Self {
        Self {
            backend,
            background,
            interval,
            timing: WatchTiming::default(),
            up_since: Instant::now(),
            seen: std::sync::Mutex::new(None),
            reading: AtomicBool::new(false),
            logged_at: std::sync::Mutex::new(None),
        }
    }

    /// Other windows than the defaults, for a test that cannot wait minutes.
    pub fn with_timing(mut self, timing: WatchTiming) -> Self {
        self.timing = timing;
        self
    }

    /// Whether the engine's loops are on, as `main` decided.
    pub fn background(&self) -> Background {
        self.background
    }

    /// How often the engine's caller is expected to tick.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Whether a tick is overdue: `None` with the loops on, or when the store
    /// could not be read.
    pub async fn overdue(&self) -> Option<bool> {
        let since = self.since_last_tick().await?;
        Some(self.is_overdue(since))
    }

    /// [`overdue`](Self::overdue), and the error line when it is, at most
    /// once per window. Every request calls it ([`watch_ticks`]).
    pub async fn check(&self) -> Option<bool> {
        let since = self.since_last_tick().await?;
        let overdue = self.is_overdue(since);
        if overdue && self.may_log() {
            tracing::error!(
                "no tick for {} s while the background loops are off: approvals do not \
                 expire, lapsed leases are not reclaimed, engine steps do not run",
                since.age.as_secs()
            );
        }
        Some(overdue)
    }

    /// The tick row as the store holds it now, read past the window, which
    /// it then starts again: what the watch says next follows this read.
    pub async fn status(&self) -> jamjet_state::BackendResult<Option<jamjet_state::TickStatus>> {
        match self.read().await {
            Ok((row, clock_offset)) => {
                self.remember(Some(Found::read(row.as_ref(), clock_offset)));
                Ok(row)
            }
            Err(e) => {
                self.remember(None);
                Err(e)
            }
        }
    }

    /// A tick of this process has finished and the store has saved it: it
    /// is not overdue now, whatever the last read found.
    pub fn tick_finished(&self) {
        let mut seen = locked(&self.seen);
        let clock_offset = seen
            .and_then(|s| s.found)
            .map_or_else(chrono::Duration::zero, |f| f.clock_offset);
        *seen = Some(Seen {
            at: seen.map_or_else(Instant::now, |s| s.at),
            found: Some(Found {
                last_finished_at: Some(jamjet_state::clock::process_now() + clock_offset),
                clock_offset,
                running: false,
            }),
        });
    }

    fn is_overdue(&self, since: Since) -> bool {
        !since.running && since.age > self.interval.saturating_mul(3)
    }

    /// How long since the last tick finished, on the store's clock, or since
    /// this process started when none has; and whether one was running at
    /// the last read.
    async fn since_last_tick(&self) -> Option<Since> {
        if self.background == Background::On {
            return None;
        }
        let found = self.found().await?;
        let age = match found.last_finished_at {
            Some(at) => (jamjet_state::clock::process_now() + found.clock_offset - at)
                .to_std()
                .unwrap_or(Duration::ZERO),
            None => self.up_since.elapsed(),
        };
        Some(Since {
            age,
            running: found.running,
        })
    }

    /// What the last read found, read again once the window has passed.
    async fn found(&self) -> Option<Found> {
        if let Some(seen) = self.fresh() {
            return seen.found;
        }
        // One request reads; one that comes meanwhile answers from what the
        // last read found rather than wait.
        let Some(_turn) = Turn::take(&self.reading) else {
            return locked(&self.seen).and_then(|s| s.found);
        };
        // A read may have ended between the look above and the turn.
        if let Some(seen) = self.fresh() {
            return seen.found;
        }
        let found = self
            .read()
            .await
            .ok()
            .map(|(row, clock_offset)| Found::read(row.as_ref(), clock_offset));
        self.remember(found)
    }

    fn fresh(&self) -> Option<Seen> {
        locked(&self.seen).filter(|s| s.at.elapsed() < self.timing.read_every)
    }

    /// The row and the store's clock, within the read timeout.
    async fn read(
        &self,
    ) -> jamjet_state::BackendResult<(Option<jamjet_state::TickStatus>, chrono::Duration)> {
        let read = async {
            let row = self.backend.tick_status().await?;
            let store_now = self.backend.store_now().await?;
            Ok((row, store_now - jamjet_state::clock::process_now()))
        };
        match tokio::time::timeout(self.timing.read_timeout, read).await {
            Ok(Ok(read)) => Ok(read),
            Ok(Err(e)) => {
                debug!("tick watch: the tick state could not be read: {e}");
                Err(e)
            }
            Err(_) => {
                debug!("tick watch: the tick state was not read within the timeout");
                Err(StateBackendError::Unavailable(format!(
                    "the tick state was not read within {:?}",
                    self.timing.read_timeout
                )))
            }
        }
    }

    /// Keep what a read found, from now. A tick this process finished while
    /// the read was out is newer than what the read saw, so it is kept.
    fn remember(&self, found: Option<Found>) -> Option<Found> {
        let mut seen = locked(&self.seen);
        let local = seen.and_then(|s| s.found).and_then(|f| f.last_finished_at);
        let found = found.map(|f| Found {
            last_finished_at: f.last_finished_at.max(local),
            ..f
        });
        *seen = Some(Seen {
            at: Instant::now(),
            found,
        });
        found
    }

    fn may_log(&self) -> bool {
        let mut logged_at = locked(&self.logged_at);
        if logged_at.is_some_and(|at| at.elapsed() < self.timing.log_every) {
            return false;
        }
        *logged_at = Some(Instant::now());
        true
    }
}

/// Put `watch` on `router`: every request checks it before it is served, so
/// any request writes the error line while a tick is overdue, and `/health`
/// and `GET /ops/status` find it in the request's extensions.
pub fn watch_ticks(router: axum::Router, watch: Arc<TickWatch>) -> axum::Router {
    router.layer(axum::middleware::from_fn_with_state(watch, check_ticks))
}

async fn check_ticks(
    axum::extract::State(watch): axum::extract::State<Arc<TickWatch>>,
    mut req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    watch.check().await;
    req.extensions_mut().insert(watch);
    next.run(req).await
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
