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
}

impl WorkerPool {
    pub fn new(backend: Arc<dyn StateBackend>) -> Self {
        Self {
            backend,
            groups: Vec::new(),
            executors: Vec::new(),
        }
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

    /// Spawn all worker tasks and return their join handles.
    ///
    /// Workers run indefinitely until their `JoinHandle` is aborted.
    pub fn spawn(self) -> Vec<JoinHandle<()>> {
        let executors: Vec<(String, Arc<dyn NodeExecutor>)> = self.executors;
        let mut handles = Vec::new();

        for group in &self.groups {
            for i in 0..group.concurrency {
                let worker_id = format!("{}-{}", group.id_prefix, i);
                let queue_types = group.queue_types.clone();
                let backend = Arc::clone(&self.backend);

                let mut worker = Worker::new(worker_id.clone(), backend, queue_types);
                for (kind, executor) in &executors {
                    worker = worker.register_executor(kind.clone(), Arc::clone(executor));
                }

                info!(
                    worker_id = %worker_id,
                    queues = ?group.queue_types,
                    "Spawning worker"
                );

                let handle = tokio::spawn(async move {
                    worker.run().await;
                });
                handles.push(handle);
            }
        }

        handles
    }
}

/// Convenience: build a default pool with standard queue groups.
///
/// - 2 general workers (`["general", "tool", "condition"]`)
/// - 2 model workers (`["model"]`)
/// - 1 privileged worker (`["privileged"]`)
pub fn default_pool(backend: Arc<dyn StateBackend>) -> WorkerPool {
    WorkerPool::new(backend)
        .with_group(WorkerGroupConfig::new(
            "general-worker",
            2,
            vec![
                "general".into(),
                "tool".into(),
                "condition".into(),
                "mcp_tool".into(),
            ],
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
}
