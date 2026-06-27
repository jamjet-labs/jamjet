use crate::event::{Event, EventKind, EventSequence};
use crate::snapshot::Snapshot;
use crate::tenant::{Tenant, TenantId, DEFAULT_TENANT};
use async_trait::async_trait;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use thiserror::Error;

/// A projected approval row — the CQRS read-model for a single (execution,
/// node) pair, maintained asynchronously by the projector.
///
/// Keyed by `(execution_id, node_id)`; `status` is the latest approval state
/// (`"pending"`, `"approved"`, `"rejected"`). `last_sequence` is the event
/// sequence that produced this row (used for idempotent re-application).
///
/// `tool_name`, `approver`, and `context` are populated only for `"pending"`
/// rows (from `ToolApprovalRequired`); they are cleared to `None` when the
/// node transitions to `"approved"` or `"rejected"`.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalProjectionRow {
    pub execution_id: ExecutionId,
    pub node_id: String,
    /// `"pending"` | `"approved"` | `"rejected"`
    pub status: String,
    pub user_id: Option<String>,
    pub comment: Option<String>,
    pub last_sequence: i64,
    /// Populated for `"pending"` rows from `ToolApprovalRequired.tool_name`.
    pub tool_name: Option<String>,
    /// Populated for `"pending"` rows from `ToolApprovalRequired.approver`.
    pub approver: Option<String>,
    /// Populated for `"pending"` rows from `ToolApprovalRequired.context`.
    pub context: Option<serde_json::Value>,
}

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

    #[error("lease fence superseded for work item {0}: lease was stolen or store failed over")]
    FenceLost(String),

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

    /// Atomically create a continuation segment: the execution row, its seed
    /// snapshot, the WorkflowStarted + NodeScheduled events, and the start-node
    /// work item, in one transaction. All-or-nothing so a crash mid-creation
    /// leaves no partial child. The caller-supplied `execution.execution_id` MUST
    /// be fresh (not yet committed); the idempotency guard lives in
    /// `start_next_segment`.
    async fn create_segment_atomic(
        &self,
        execution: WorkflowExecution,
        seed_snapshot: Snapshot,
        started_event: EventKind,
        scheduled_event: EventKind,
        work_item: WorkItem,
    ) -> BackendResult<()>;

    // ── Idempotency cache (tool_effects) ─────────────────────────────────────

    /// Look up a previously recorded node result by its idempotency key.
    ///
    /// Returns the `result_json` value — `{output, state_patch, duration_ms,
    /// gen_ai_system, gen_ai_model, input_tokens, output_tokens, finish_reason}`
    /// — if the key exists, or `None` if it has not been recorded yet.
    ///
    /// Recorded atomically inside `commit_turn` when the terminal event is a
    /// `NodeCompleted` with `idempotency_key = Some(k)`.
    async fn get_tool_effect(&self, key: &str) -> BackendResult<Option<serde_json::Value>>;

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

    /// Renew the lease on a claimed work item (heartbeat). Fails closed if the
    /// presented `lease_fence` no longer matches (lease stolen / failed over).
    async fn renew_lease(
        &self,
        item_id: WorkItemId,
        worker_id: &str,
        lease_fence: i64,
    ) -> BackendResult<()>;

    /// Mark a work item as completed and release the lease.
    async fn complete_work_item(&self, item_id: WorkItemId) -> BackendResult<()>;

    /// Mark a work item as failed. The scheduler will decide whether to retry.
    async fn fail_work_item(&self, item_id: WorkItemId, error: &str) -> BackendResult<()>;

    /// Atomically settle a work item, append the terminal event, and optionally
    /// compute and write a snapshot from the committed state, all in ONE
    /// transaction gated by `lease_fence`. When `write_snapshot` is `true` the
    /// snapshot is computed inside the same `BEGIN IMMEDIATE` transaction —
    /// after the terminal event is inserted — by reading the latest snapshot +
    /// all events since it (including the terminal event just written), folding
    /// with `apply_events_seeded`, and writing with INSERT OR REPLACE. This
    /// avoids write-skew: concurrent sibling nodes cannot produce snapshots that
    /// each miss the other's patch. A stale fence returns `FenceLost` and writes
    /// nothing.
    async fn commit_turn(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        terminal_event: Event,
        write_snapshot: bool,
    ) -> BackendResult<EventSequence>;

    /// Atomically return a leased work item to `pending`, visible again at
    /// `retry_after`, with the attempt count set to `next_attempt` and a fresh
    /// lease epoch (so the next claim mints a strictly-greater fence).
    ///
    /// Fence-guarded: if `lease_fence` no longer matches the stored value (stale
    /// worker, race, or already parked by another path) the update matches zero
    /// rows and `Ok(false)` is returned — a no-op, consistent with the
    /// `FenceLost` zombie path in `commit_turn`.
    async fn park_work_item(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        retry_after: &str,
        next_attempt: u32,
    ) -> BackendResult<bool>;

    /// Fence-guarded finalization of a continue-as-new rollover on the OLD execution.
    ///
    /// In one transaction:
    /// 1. Attempt `UPDATE work_items SET status='completed', completed_at=?, lease_expires_at=NULL
    ///    WHERE id=? AND lease_fence=?`. Zero rows means the fence no longer matches
    ///    (lease was stolen or item already settled) — rollback and return `Ok(false)`.
    /// 2. Idempotently mark the execution terminal:
    ///    `UPDATE workflow_executions SET status='limit_exceeded', updated_at=?, completed_at=COALESCE(completed_at,?)
    ///    WHERE execution_id=? AND status NOT IN ('completed','failed','cancelled','limit_exceeded')`.
    ///    (This is a no-op if the execution is already terminal, which is safe.)
    /// 3. Commit and return `Ok(true)`.
    ///
    /// A zombie worker (stale fence) gets `Ok(false)` and must not proceed further.
    async fn finalize_rollover_fenced(
        &self,
        execution_id: &ExecutionId,
        work_item_id: WorkItemId,
        lease_fence: i64,
    ) -> BackendResult<bool>;

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

    // ── Async read-model projector ───────────────────────────────────────────

    /// Atomically UPSERT an approval projection row AND advance the named
    /// projection's checkpoint for this execution to `new_checkpoint`, in one
    /// transaction.  Crash-safe: if the process dies between the UPSERT and the
    /// checkpoint write they are the same commit, so neither can be partially
    /// applied.
    async fn apply_approval_projection(
        &self,
        row: ApprovalProjectionRow,
        projection_name: &str,
        new_checkpoint: i64,
    ) -> BackendResult<()>;

    /// Atomically UPSERT all approval projection rows for one execution AND
    /// advance the named projection's checkpoint to `new_checkpoint`, in ONE
    /// transaction.
    ///
    /// This is the correct primitive to use when a tick produces multiple
    /// changed nodes: a crash after writing row 1 but before row 2 would
    /// otherwise leave the checkpoint advanced past unwritten rows.  With this
    /// method the checkpoint advances IFF every row in the batch committed.
    ///
    /// If `rows` is empty the checkpoint is still advanced (callers that want
    /// to skip the checkpoint update when there is nothing to write should
    /// check before calling).
    async fn apply_approval_projection_batch(
        &self,
        rows: Vec<ApprovalProjectionRow>,
        projection_name: &str,
        execution_id: &ExecutionId,
        new_checkpoint: i64,
    ) -> BackendResult<()>;

    /// All approval projection rows for an execution (the projected read model).
    async fn get_approval_projection(
        &self,
        execution_id: &ExecutionId,
    ) -> BackendResult<Vec<ApprovalProjectionRow>>;

    /// Current checkpoint (last consumed sequence) for (projection_name,
    /// execution_id).  Returns 0 if no checkpoint exists yet — the projector
    /// will re-read from the beginning of the event log.
    async fn get_projector_checkpoint(
        &self,
        projection_name: &str,
        execution_id: &ExecutionId,
    ) -> BackendResult<i64>;

    /// Advance the projector checkpoint for (projection_name, execution_id)
    /// to `new_checkpoint` WITHOUT writing any projection row.
    ///
    /// Used when a batch of events contained NO approval events (only
    /// non-approval events) — the checkpoint must still advance to `batch_max`
    /// so those events are not re-scanned on the next tick.
    async fn set_projector_checkpoint(
        &self,
        projection_name: &str,
        execution_id: &ExecutionId,
        new_checkpoint: i64,
    ) -> BackendResult<()>;

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
    /// Monotonic lease fence: store term * 2^32 + per-item epoch. Minted on
    /// every claim/reclaim; a leased-worker write that presents a stale fence
    /// matches zero rows and fails closed.
    #[serde(default)]
    pub lease_fence: i64,
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
