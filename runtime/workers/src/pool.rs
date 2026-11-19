//! Worker pool — manages a fleet of concurrent worker Tokio tasks.
//!
//! `WorkerPool` spawns N worker tasks, each with its own queue affinity.
//! Workers share the same `StateBackend` (via `Arc`) and executor registry.
//!
//! Queue isolation is achieved by assigning specific `queue_types` to each
//! worker. For example, model workers only claim from the `["model"]` queue,
//! while general workers handle `["general", "tool"]`.

use crate::executor::NodeExecutor;
use crate::worker::Worker;
use jamjet_state::backend::StateBackend;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::info;

/// Configuration for a worker pool group (workers sharing the same queues).
#[derive(Clone)]
pub struct WorkerGroupConfig {
    /// Prefix for worker IDs (e.g. "model-worker"). IDs become `{prefix}-{n}`.
    pub id_prefix: String,
    /// Number of concurrent workers in this group.
    pub concurrency: usize,
    /// Queue types this group handles.
    pub queue_types: Vec<String>,
}

impl WorkerGroupConfig {
    pub fn new(id_prefix: impl Into<String>, concurrency: usize, queue_types: Vec<String>) -> Self {
        Self {
            id_prefix: id_prefix.into(),
            concurrency,
            queue_types,
        }
    }
}

/// A pool of concurrent worker tasks.
///
/// Call `spawn()` to start all workers. The returned handles can be awaited
/// or aborted by the caller (e.g. on shutdown signal).
pub struct WorkerPool {
    backend: Arc<dyn StateBackend>,
    groups: Vec<WorkerGroupConfig>,
    executors: Vec<(String, Arc<dyn NodeExecutor>)>,
    sealer: Option<Arc<dyn jamjet_state::approvals::ApprovalSealer>>,
    agents: Option<Arc<dyn jamjet_agents::AgentRegistry>>,
    shutdown: Option<crate::shutdown::Shutdown>,
}

impl WorkerPool {
    pub fn new(backend: Arc<dyn StateBackend>) -> Self {
        Self {
            backend,
            groups: Vec::new(),
            executors: Vec::new(),
            sealer: None,
            agents: None,
            shutdown: None,
        }
    }

    /// Stop with `shutdown`: on its trigger each worker finishes the item it
    /// holds, claims nothing more and exits, and the trigger waits for them.
    /// Without one the workers run until their handles are aborted.
    pub fn with_shutdown(mut self, shutdown: crate::shutdown::Shutdown) -> Self {
        self.shutdown = Some(shutdown);
        self
    }

    /// The agent registry every worker reads Agent Cards from: autonomy
    /// limits, and the `allowed_tools` of the agent a node acts for. Without
    /// one, a node attributed to an agent is refused.
    pub fn with_agents(mut self, agents: Arc<dyn jamjet_agents::AgentRegistry>) -> Self {
        self.agents = Some(agents);
        self
    }

    /// Seal the approval requests every worker raises into the audit chain.
    pub fn with_sealer(mut self, sealer: Arc<dyn jamjet_state::approvals::ApprovalSealer>) -> Self {
        self.sealer = Some(sealer);
        self
    }

    /// Add a worker group (workers with a specific queue affinity).
    pub fn with_group(mut self, group: WorkerGroupConfig) -> Self {
        self.groups.push(group);
        self
    }

    /// Register an executor for a node kind tag.
    pub fn with_executor(
        mut self,
        kind: impl Into<String>,
        executor: Arc<dyn NodeExecutor>,
    ) -> Self {
        self.executors.push((kind.into(), executor));
        self
    }

    /// The node kinds this pool has an executor for, sorted.
    pub fn executor_kinds(&self) -> Vec<String> {
        let mut kinds: Vec<String> = self.executors.iter().map(|(k, _)| k.clone()).collect();
        kinds.sort();
        kinds.dedup();
        kinds
    }

    /// One worker with this pool's executors, sealer and agent registry,
    /// claiming `queue_types`. [`spawn`](Self::spawn) builds every worker
    /// with it; a test drives one directly.
    pub fn build_worker(&self, worker_id: String, queue_types: Vec<String>) -> Worker {
        let mut worker = Worker::new(worker_id, Arc::clone(&self.backend), queue_types);
        if let Some(sealer) = &self.sealer {
            worker = worker.with_sealer(Arc::clone(sealer));
        }
        if let Some(agents) = &self.agents {
            worker = worker.with_agents(Arc::clone(agents));
        }
        for (kind, executor) in &self.executors {
            worker = worker.register_executor(kind.clone(), Arc::clone(executor));
        }
        worker
    }

    /// Spawn all worker tasks and return their join handles.
    ///
    /// Workers run until their `JoinHandle` is aborted, or, with
    /// [`with_shutdown`](Self::with_shutdown), until it triggers.
    pub fn spawn(self) -> Vec<JoinHandle<()>> {
        let (_never, idle) = tokio::sync::watch::channel(false);
        let mut handles = Vec::new();

        for group in &self.groups {
            for i in 0..group.concurrency {
                let worker_id = format!("{}-{}", group.id_prefix, i);
                let worker = self.build_worker(worker_id.clone(), group.queue_types.clone());

                info!(
                    worker_id = %worker_id,
                    queues = ?group.queue_types,
                    "Spawning worker"
                );

                let (stop, running) = match &self.shutdown {
                    Some(s) => (s.receiver(), Some(s.enter())),
                    None => (idle.clone(), None),
                };
                // A worker that panics is started again. Nothing restarted it,
                // so one bad item emptied the pool for the life of the process.
                //
                // In this task, not a task of its own: aborting this handle
                // must stop the worker, and an inner task outlived the abort
                // and went on claiming. The wait between restarts doubles up
                // to a minute, and after RESTARTS_WITHOUT_PROGRESS panics in a
                // row with no item finished between them the worker stays
                // down: a panic every item hits would otherwise run the whole
                // queue into the dead-letter queue, one charged attempt each.
                let handle = tokio::spawn(async move {
                    use futures::FutureExt;
                    let _running = running;
                    let mut in_a_row = 0u32;
                    loop {
                        let finished = worker.items_finished();
                        let run = std::panic::AssertUnwindSafe(worker.run_until(stop.clone()))
                            .catch_unwind()
                            .await;
                        if run.is_ok() || *stop.borrow() {
                            break;
                        }
                        in_a_row = if worker.items_finished() > finished {
                            1
                        } else {
                            in_a_row + 1
                        };
                        if in_a_row >= RESTARTS_WITHOUT_PROGRESS {
                            tracing::error!(
                                worker_id = %worker_id,
                                panics = in_a_row,
                                "Worker panicked {in_a_row} times in a row without finishing an \
                                 item; not starting it again. The items it took are retried \
                                 when their leases expire. Restart the server once the cause is \
                                 fixed."
                            );
                            break;
                        }
                        let wait = restart_wait(in_a_row);
                        tracing::error!(
                            worker_id = %worker_id,
                            panics = in_a_row,
                            wait_secs = wait.as_secs(),
                            "Worker panicked; starting it again"
                        );
                        tokio::time::sleep(wait).await;
                    }
                });
                handles.push(handle);
            }
        }

        handles
    }
}

/// Panics in a row, with no item finished between them, after which a pool
/// worker is not started again.
pub const RESTARTS_WITHOUT_PROGRESS: u32 = 5;

/// How long a pool worker waits before it is started again after its
/// `in_a_row`-th panic in a row: 1 s, doubling, at most a minute.
pub fn restart_wait(in_a_row: u32) -> std::time::Duration {
    let secs = 1u64 << in_a_row.saturating_sub(1).min(6);
    std::time::Duration::from_secs(secs.min(60))
}

/// The server's embedded pool: [`default_pool`]'s queue groups with an
/// executor for every kind in [`crate::executable::IN_PROCESS_KINDS`], and no
/// other. Registration (`POST /workflows`) refuses a workflow with a node of
/// a kind outside that list and [`crate::executable::EXTERNAL_WORKER_KINDS`],
/// so the two must agree; a test below holds them to each other.
pub fn server_pool(
    backend: Arc<dyn StateBackend>,
    models: Arc<jamjet_models::ModelRegistry>,
) -> WorkerPool {
    use crate::executors::{
        ConditionNodeExecutor, EvalExecutor, HumanApprovalExecutor, LimitExceededExecutor,
        ModelNodeExecutor,
    };
    default_pool(Arc::clone(&backend))
        .with_executor("model", Arc::new(ModelNodeExecutor::new(models.clone())))
        .with_executor("eval", Arc::new(EvalExecutor::new(models)))
        .with_executor("condition", Arc::new(ConditionNodeExecutor::new()))
        .with_executor(
            "human_approval",
            Arc::new(HumanApprovalExecutor::new(backend)),
        )
        .with_executor("limit_exceeded", Arc::new(LimitExceededExecutor))
}

/// Convenience: build a default pool with standard queue groups.
///
/// - 2 general workers (`["general", "tool", "retrieval"]`). `retrieval` is
///   claimed so that a `memory_retrieval` item (no executor; registration
///   refuses the kind) fails instead of waiting forever on a queue nothing
///   polls. They used to claim `condition` and `mcp_tool` too, queues no node
///   kind is routed to, and so took the items of an `external_fn` node that
///   named its queue `condition` or `mcp_tool` (registration allows both).
/// - 2 model workers (`["model"]`)
/// - 1 privileged worker (`["privileged"]`)
/// - 0 python workers (`["python_tool"]`) — intent registration only. Durable
///   Python `@tool` nodes are executed by an external `jamjet worker` process;
///   no internal Rust worker claims this queue. Start the sidecar with:
///   `jamjet worker --queue python_tool`
/// - 0 java workers (`["java_tool"]`) — intent registration only, mirroring
///   `python_tool`. Durable Java `@Tool` nodes are executed by an external Java
///   tool-worker that claims this queue over the same HTTP claim/complete API;
///   no internal Rust worker claims it.
pub fn default_pool(backend: Arc<dyn StateBackend>) -> WorkerPool {
    WorkerPool::new(backend)
        .with_group(WorkerGroupConfig::new(
            "general-worker",
            2,
            vec!["general".into(), "tool".into(), "retrieval".into()],
        ))
        .with_group(WorkerGroupConfig::new(
            "model-worker",
            2,
            vec!["model".into()],
        ))
        .with_group(WorkerGroupConfig::new(
            "privileged-worker",
            1,
            vec!["privileged".into()],
        ))
        // Register python_tool as a known queue without spawning internal workers.
        // Items in this queue are claimed exclusively by an external `jamjet worker`.
        .with_group(WorkerGroupConfig::new(
            "python-worker",
            0,
            vec!["python_tool".into()],
        ))
        // Register java_tool as a known queue without spawning internal workers,
        // mirroring python_tool. Items in this queue are claimed exclusively by an
        // external Java tool-worker over the same HTTP claim/complete API.
        .with_group(WorkerGroupConfig::new(
            "java-worker",
            0,
            vec!["java_tool".into()],
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Registration accepts a kind as in-process because this pool runs it.
    /// If one list changes without the other, a workflow either registers
    /// and fails every run, or is refused although it would run.
    #[test]
    fn the_server_pool_runs_exactly_the_in_process_kinds() {
        let backend: Arc<dyn StateBackend> = Arc::new(jamjet_state::InMemoryBackend::new());
        let pool = server_pool(backend, Arc::new(jamjet_models::ModelRegistry::new()));
        let mut expected: Vec<String> = crate::executable::IN_PROCESS_KINDS
            .iter()
            .map(|k| k.to_string())
            .collect();
        expected.sort();
        assert_eq!(pool.executor_kinds(), expected);
    }

    /// The embedded workers claim only queues the engine routes its own
    /// kinds to. Any other name is free for an `external_fn` node, whose
    /// items an in-process worker would fail (it has no executor for them).
    #[test]
    fn the_embedded_pool_leaves_external_queue_names_alone() {
        let backend: Arc<dyn StateBackend> = Arc::new(jamjet_state::InMemoryBackend::new());
        let pool = default_pool(backend);
        for group in pool.groups.iter().filter(|g| g.concurrency > 0) {
            for q in &group.queue_types {
                assert!(
                    !matches!(
                        jamjet_core::node::QueueType::from_name(q),
                        jamjet_core::node::QueueType::Named(_)
                    ),
                    "the embedded pool claims `{q}`, a name an external_fn node may use"
                );
            }
        }
    }

    /// Nothing executes `memory_retrieval`, and its items go to the
    /// `retrieval` queue. The embedded pool claims that queue so such an item
    /// fails (no executor) instead of waiting forever for a worker.
    #[test]
    fn the_embedded_pool_claims_the_retrieval_queue() {
        let backend: Arc<dyn StateBackend> = Arc::new(jamjet_state::InMemoryBackend::new());
        let pool = default_pool(backend);
        assert!(pool
            .groups
            .iter()
            .any(|g| g.concurrency > 0 && g.queue_types.iter().any(|q| q == "retrieval")));
    }
}
