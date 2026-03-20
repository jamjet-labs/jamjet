use crate::event::{Event, EventSequence};
use crate::snapshot::Snapshot;
use crate::tenant::{Tenant, TenantId, DEFAULT_TENANT};
use async_trait::async_trait;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use thiserror::Error;

/// Workflow definition stored in the registry (the compiled IR as JSON).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkflowDefinition {
    pub workflow_id: String,
    pub version: String,
    /// The canonical IR JSON value.
    pub ir: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Tenant that owns this workflow definition.
    #[serde(default = "default_tenant_string")]
    pub tenant_id: String,
}

fn default_tenant_string() -> String {
    DEFAULT_TENANT.to_string()
}

#[derive(Debug, Error)]
pub enum StateBackendError {
    #[error("execution not found: {0}")]
    NotFound(String),

    #[error("optimistic concurrency conflict: sequence mismatch for {0}")]
    SequenceConflict(String),

    #[error("database error: {0}")]
    Database(String),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

pub type BackendResult<T> = Result<T, StateBackendError>;

/// Trait abstracting the durable state storage backend.
///
/// Implementors: `SqliteBackend`, `PostgresBackend`.
/// Both must guarantee transactional writes per state transition.
#[async_trait]
pub trait StateBackend: Send + Sync {
    // ── Workflow definitions ─────────────────────────────────────────────

    /// Store a compiled workflow IR.
    async fn store_workflow(&self, def: WorkflowDefinition) -> BackendResult<()>;

    /// Load a workflow definition by id and version.
    async fn get_workflow(
        &self,
        workflow_id: &str,
        version: &str,
    ) -> BackendResult<Option<WorkflowDefinition>>;

    // ── Workflow executions ──────────────────────────────────────────────

    /// Create a new workflow execution record.
    async fn create_execution(&self, execution: WorkflowExecution) -> BackendResult<()>;

    /// Load a workflow execution by id.
    async fn get_execution(&self, id: &ExecutionId) -> BackendResult<Option<WorkflowExecution>>;

    /// Update the status of a workflow execution.
    async fn update_execution_status(
        &self,
        id: &ExecutionId,
        status: WorkflowStatus,
    ) -> BackendResult<()>;

    /// Update the current_state of a workflow execution (apply state patches).
    async fn update_execution_current_state(
        &self,
        id: &ExecutionId,
        current_state: &serde_json::Value,
    ) -> BackendResult<()>;

    /// Append a value to an array field in the execution's current_state.
    /// Creates the array if it doesn't exist.
    async fn patch_append_array(
        &self,
        execution_id: &ExecutionId,
        key: &str,
        value: serde_json::Value,
    ) -> BackendResult<()>;

    /// List executions, optionally filtered by status.
    async fn list_executions(
        &self,
        status: Option<WorkflowStatus>,
        limit: u32,
        offset: u32,
    ) -> BackendResult<Vec<WorkflowExecution>>;

    // ── Event log ────────────────────────────────────────────────────────

    /// Append an event to the event log.
    /// Must be transactional — either fully written or not at all.
    async fn append_event(&self, event: Event) -> BackendResult<EventSequence>;

    /// Load all events for an execution, ordered by sequence.
    async fn get_events(&self, execution_id: &ExecutionId) -> BackendResult<Vec<Event>>;

    /// Load events since a given sequence number (exclusive).
    async fn get_events_since(
        &self,
        execution_id: &ExecutionId,
        since_sequence: EventSequence,
    ) -> BackendResult<Vec<Event>>;

    /// Get the latest event sequence for an execution.
    async fn latest_sequence(&self, execution_id: &ExecutionId) -> BackendResult<EventSequence>;

    // ── Snapshots ────────────────────────────────────────────────────────

    /// Write a snapshot.
    async fn write_snapshot(&self, snapshot: Snapshot) -> BackendResult<()>;

    /// Load the latest snapshot for an execution.
    async fn latest_snapshot(&self, execution_id: &ExecutionId) -> BackendResult<Option<Snapshot>>;

    // ── Queue (simple Postgres/SQLite backed queue in v1) ────────────────

    /// Enqueue a work item for execution.
    async fn enqueue_work_item(&self, item: WorkItem) -> BackendResult<WorkItemId>;

    /// Claim the next available work item for a given queue type.
    /// Returns None if no items are available.
    async fn claim_work_item(
        &self,
        worker_id: &str,
        queue_types: &[&str],
    ) -> BackendResult<Option<WorkItem>>;

    /// Renew the lease on a claimed work item (heartbeat).
    async fn renew_lease(&self, item_id: WorkItemId, worker_id: &str) -> BackendResult<()>;

    /// Mark a work item as completed and release the lease.
    async fn complete_work_item(&self, item_id: WorkItemId) -> BackendResult<()>;

    /// Mark a work item as failed. The scheduler will decide whether to retry.
    async fn fail_work_item(&self, item_id: WorkItemId, error: &str) -> BackendResult<()>;

    /// Reclaim work items whose lease has expired (worker crashed or stalled).
    ///
    /// For each expired item:
    /// - Increments `attempt`.
    /// - If `attempt < max_attempts`: resets to `pending` with optional backoff delay
    ///   via `retry_after`, and returns it in the `retryable` list.
    /// - If `attempt >= max_attempts`: moves to `dead_letter` and returns it in
    ///   the `exhausted` list.
    async fn reclaim_expired_leases(&self) -> BackendResult<ReclaimResult>;

    /// Move a failed work item to the dead-letter queue.
    async fn move_to_dead_letter(&self, item_id: WorkItemId, last_error: &str)
        -> BackendResult<()>;

    // ── API token auth ───────────────────────────────────────────────────

    /// Create a new API token. Returns `(plaintext_token, token_info)`.
    /// The plaintext token is only returned here; only its hash is stored.
    async fn create_token(&self, name: &str, role: &str) -> BackendResult<(String, ApiToken)>;

    /// Validate a plaintext API token. Returns the token info if valid and not revoked.
    async fn validate_token(&self, token: &str) -> BackendResult<Option<ApiToken>>;

    // ── Tenant management ───────────────────────────────────────────────

    /// Create a new tenant.
    async fn create_tenant(&self, _tenant: Tenant) -> BackendResult<()> {
        Err(StateBackendError::Database(
            "tenant management not supported".into(),
        ))
    }

    /// Get a tenant by ID.
    async fn get_tenant(&self, _id: &TenantId) -> BackendResult<Option<Tenant>> {
        Ok(None)
    }

    /// List all tenants.
    async fn list_tenants(&self) -> BackendResult<Vec<Tenant>> {
        Ok(vec![])
    }

    /// Update tenant metadata (name, status, policy, limits).
    async fn update_tenant(&self, _tenant: Tenant) -> BackendResult<()> {
        Err(StateBackendError::Database(
            "tenant management not supported".into(),
        ))
    }
}

/// Metadata for an API token (does not contain the plaintext token).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApiToken {
    pub id: String,
    pub name: String,
    pub role: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Tenant this token belongs to.
    #[serde(default = "default_tenant_string")]
    pub tenant_id: String,
}

pub type WorkItemId = uuid::Uuid;

/// A unit of work dispatched to a worker queue.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkItem {
    pub id: WorkItemId,
    pub execution_id: ExecutionId,
    pub node_id: String,
    pub queue_type: String,
    pub payload: serde_json::Value,
    pub attempt: u32,
    /// Maximum number of attempts before moving to dead-letter queue.
    pub max_attempts: u32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub lease_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub worker_id: Option<String>,
    /// Tenant that owns this work item.
    #[serde(default = "default_tenant_string")]
    pub tenant_id: String,
}

/// Result of reclaiming expired work item leases.
#[derive(Debug, Default)]
pub struct ReclaimResult {
    /// Items whose lease expired and have been reset to `pending` for retry.
    pub retryable: Vec<WorkItem>,
    /// Items that exhausted all attempts and were moved to dead-letter.
    pub exhausted: Vec<WorkItem>,
}
