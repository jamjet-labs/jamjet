use crate::artifact::ArtifactRef;
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

/// Where a page of [`StateBackend::list_pending_approvals`] starts: the sort
/// key of the last row of the page before, `(updated_at, execution_id,
/// node_id)`, compared as the stored text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApprovalCursor {
    pub updated_at: String,
    /// The hyphenated uuid, as the execution id is stored.
    pub execution_id: String,
    pub node_id: String,
}

impl From<&PendingApprovalRow> for ApprovalCursor {
    /// The cursor of the page after the one `row` ends.
    fn from(row: &PendingApprovalRow) -> Self {
        Self {
            updated_at: row.updated_at.clone(),
            execution_id: row.execution_id.0.to_string(),
            node_id: row.node_id.clone(),
        }
    }
}

/// A request waiting for a decision, from
/// [`StateBackend::list_pending_approvals`]: the projected row and the run's
/// workflow.
#[derive(Debug, Clone)]
pub struct PendingApprovalRow {
    pub execution_id: ExecutionId,
    pub workflow_id: String,
    pub workflow_version: String,
    pub node_id: String,
    pub tool_name: Option<String>,
    pub approver: Option<String>,
    pub context: Option<serde_json::Value>,
    /// The sequence of the `ToolApprovalRequired` the row was projected from.
    pub request_sequence: i64,
    /// When the row was projected, as stored.
    pub updated_at: String,
}

/// A pending or claimed item of a run, from [`StateBackend::open_work_items`].
#[derive(Debug, Clone)]
pub struct OpenWorkItem {
    pub id: WorkItemId,
    pub node_id: String,
    pub queue_type: String,
    /// Leased to a worker. The lease may have lapsed and not yet been taken
    /// back: see `lease_expires_at`.
    pub claimed: bool,
    pub lease_expires_at: Option<chrono::DateTime<chrono::Utc>>,
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

    /// A workflow version is already registered with a different definition.
    /// Versions are immutable: an approval and the policy it was granted
    /// under are bound to one definition, so a version cannot be redefined.
    #[error("workflow version already registered with a different definition: {0}")]
    VersionConflict(String),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// The store could not answer now: a deadlock or a lock timeout that
    /// outlasted its retries, a lost connection, a timeout, or no free
    /// connection. A commit whose outcome is unknown answers this too, so the
    /// call may have landed. Worth a retry later; the API answers 503.
    #[error("store unavailable: {0}")]
    Unavailable(String),
}

pub type BackendResult<T> = Result<T, StateBackendError>;

/// Which items a claim may take; see
/// [`StateBackend::claim_work_item_filtered`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaimFilter {
    pub execution_id: Option<ExecutionId>,
    pub node_id: Option<String>,
}

impl ClaimFilter {
    /// No restriction: the oldest claimable item on the queues.
    pub fn is_empty(&self) -> bool {
        self.execution_id.is_none() && self.node_id.is_none()
    }
}

/// Where the claim gate stands on an item under its current lease
/// ([`StateBackend::mark_gate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateMark {
    /// No gate decided under this lease: one withheld it and left the lease,
    /// or none ran. A claim may decide it again.
    Undecided,
    /// A gate is deciding under this lease, or was cut short (its request
    /// dropped). Not to be decided a second time beside it; the lease
    /// expires and the next claim decides.
    Gating,
    /// The gate released the payload under this lease.
    Released,
}

/// An item leased now, as [`StateBackend::leased_items`] lists it.
#[derive(Debug, Clone)]
pub struct LeasedItem {
    pub item: WorkItem,
    /// The lease length the claim was granted, which every renewal honours.
    pub lease_secs: u64,
    /// The claim gate's mark under THIS lease.
    pub gate: GateMark,
}

/// What [`StateBackend::claim_for_execution`] found.
#[derive(Debug, Clone)]
pub enum ExecutionClaim {
    /// A pending item, now leased to the caller.
    Claimed(WorkItem),
    /// An item already leased to this worker, returned with its lease.
    AlreadyLeased(LeasedItem),
    /// Another worker holds the matching item.
    LeasedToOther,
    /// No matching item is pending or leased.
    Nothing,
}

/// Whether two stored IRs are the same definition: equal canonical JSON, so
/// key order and whitespace do not matter. Every backend's `store_workflow`
/// uses this to tell an idempotent re-register from a redefinition.
pub fn same_definition(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    crate::hashing::content_hash(a) == crate::hashing::content_hash(b)
}

/// The refusal for a redefinition of `workflow_id` v`version`.
pub fn version_conflict(workflow_id: &str, version: &str) -> StateBackendError {
    StateBackendError::VersionConflict(format!(
        "{workflow_id} v{version}; register a new version instead"
    ))
}

/// The run key a start claims, with what it binds the key to
/// ([`StateBackend::start_execution_keyed`]). The store compares only the key
/// and `replaces`; whether the workflow, version and input match a key
/// already held is the caller's to judge from the [`RunKeyRecord`].
#[derive(Debug, Clone)]
pub struct RunKeyClaim {
    pub run_key: String,
    pub workflow_id: String,
    pub workflow_version: String,
    pub input_hash: String,
    /// failed_only: move the key from this run, and only from it.
    pub replaces: Option<ExecutionId>,
}

/// What a run key names: the run it was last given to, and the workflow,
/// version and input hash it was first claimed with.
#[derive(Debug, Clone, PartialEq)]
pub struct RunKeyRecord {
    pub execution_id: ExecutionId,
    pub workflow_id: String,
    pub workflow_version: String,
    pub input_hash: String,
}

/// The answer of [`StateBackend::start_execution_keyed`].
#[derive(Debug, Clone, PartialEq)]
pub enum KeyedStart {
    /// The run is stored and the key names it.
    Started,
    /// The key names another run, given here. Nothing was written.
    Taken(RunKeyRecord),
}

/// Trait abstracting the durable state storage backend.
///
/// Implementors: [`InMemoryBackend`](crate::InMemoryBackend) (tests and dev),
/// [`SqliteBackend`](crate::SqliteBackend) with its tenant view
/// [`TenantScopedSqliteBackend`](crate::TenantScopedSqliteBackend), and [`PgBackend`](crate::PgBackend) with its
/// tenant views. Each must make each state transition one transactional
/// write, and pass the conformance suite in `jamjet-testkit`.
#[async_trait]
pub trait StateBackend: Send + Sync {
    // ── Workflow definitions ─────────────────────────────────────────────

    /// Store a compiled workflow IR.
    ///
    /// A version is IMMUTABLE. Storing a version that already exists with the
    /// same definition (see [`same_definition`]) is a no-op `Ok`; with a
    /// different definition it is refused with
    /// [`StateBackendError::VersionConflict`] and the stored IR is untouched.
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

    /// The tenant an execution belongs to, from its execution row: the one
    /// authoritative record of a run's tenant. `None` for an unknown
    /// execution (or, on a tenant-scoped backend, one of another tenant).
    /// Components that run unscoped (the scheduler) use it to attribute what
    /// they record about a run, such as the audit entries of its expiries.
    async fn execution_tenant(&self, id: &ExecutionId) -> BackendResult<Option<TenantId>>;

    /// Update the status of a workflow execution.
    async fn update_execution_status(
        &self,
        id: &ExecutionId,
        status: WorkflowStatus,
    ) -> BackendResult<()>;

    /// End a live execution: set its terminal `status` and append its
    /// terminal `event` (`WorkflowCancelled`, `WorkflowCompleted`,
    /// `WorkflowFailed`) as one atomic step. The event's sequence is assigned
    /// here, like [`append_event`](Self::append_event).
    ///
    /// The status column and the event log are two records of one fact, and
    /// [`commit_turn_unless_terminal`](Self::commit_turn_unless_terminal)
    /// reads the status. Written apart, a fenced commit landing between the two
    /// writes saw `Running` and recorded `NodeCompleted` after the terminal
    /// event (PR #140 re-review, I-A). Written together, a commit sees either
    /// neither or both.
    ///
    /// An execution that is already terminal is left untouched and its status
    /// returned as [`ExecutionFinish::AlreadyTerminal`], so two racing
    /// terminations record one terminal event, not two. A non-terminal
    /// `status` is an error; an unknown execution is `NotFound`.
    async fn finish_execution(
        &self,
        event: Event,
        status: WorkflowStatus,
    ) -> BackendResult<ExecutionFinish>;

    /// Append `event` only if `condition` holds for the execution's whole
    /// event log, read in the same step as the append, and the execution is
    /// live. The event's sequence is assigned here.
    ///
    /// The compare-and-set the approval loop needs: a human decision and the
    /// deadline sweep each resolve a request only while it is still pending
    /// (`approvals::still_pending`). Read and appended apart, a decision and
    /// an expiry of one request could both land and the record would hold two
    /// outcomes. Under this method exactly one does; the other is
    /// [`ConditionalAppend::Refused`] and writes nothing.
    ///
    /// `condition` must be a pure function of the log. A terminal execution
    /// is [`ConditionalAppend::ExecutionTerminal`] and an unknown one
    /// `NotFound`; nothing is written either way.
    async fn append_event_if(
        &self,
        event: Event,
        condition: &crate::backend::EventCondition,
    ) -> BackendResult<ConditionalAppend>;

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

    /// Up to `limit` running executions whose id sorts after `after` (all
    /// of them for `None`), in id order: a keyset page.
    ///
    /// The scheduler pages through this so it visits every running run each
    /// tick. It used to read one `list_executions` page of 100, ordered by
    /// `updated_at`, so past 100 running runs the rest never advanced; and
    /// offset paging on `updated_at` skips rows as runs update and finish
    /// under it. The id order does not move.
    ///
    /// A row that cannot be read is left out of `executions` but still moves
    /// the cursor, so it cannot stop the walk. `next_after` is the id of the
    /// last row scanned, `None` once a page scans nothing.
    ///
    /// The default reads every running execution through `list_executions`
    /// and pages in memory; the SQLite and in-memory backends override it
    /// with a direct query.
    async fn list_running_executions_after(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> BackendResult<RunningPage> {
        const PAGE: u32 = 500;
        let mut all = Vec::new();
        let mut offset = 0;
        loop {
            let page = self
                .list_executions(Some(WorkflowStatus::Running), PAGE, offset)
                .await?;
            let n = page.len() as u32;
            all.extend(page);
            if n < PAGE {
                break;
            }
            offset += n;
        }
        all.sort_by_key(|e| e.execution_id.0.to_string());
        all.dedup_by_key(|e| e.execution_id.0);
        let executions: Vec<WorkflowExecution> = all
            .into_iter()
            .filter(|e| after.is_none_or(|a| e.execution_id.0.to_string().as_str() > a))
            .take(limit as usize)
            .collect();
        let next_after = executions.last().map(|e| e.execution_id.0.to_string());
        Ok(RunningPage {
            executions,
            next_after,
        })
    }

    // ── Event log ────────────────────────────────────────────────────────

    /// Append an event to the event log.
    /// Must be transactional — either fully written or not at all.
    async fn append_event(&self, event: Event) -> BackendResult<EventSequence>;

    /// Load all events for an execution, ordered by sequence.
    async fn get_events(&self, execution_id: &ExecutionId) -> BackendResult<Vec<Event>>;

    /// Load events since a given sequence number (exclusive).
    /// Returns events in ascending `sequence` order.
    async fn get_events_since(
        &self,
        execution_id: &ExecutionId,
        since_sequence: EventSequence,
    ) -> BackendResult<Vec<Event>>;

    /// At most `limit` events with a sequence above `after`, in ascending
    /// order, and none after the one at which their serialized length reaches
    /// `max_bytes` ([`within_budget`]): one page of the run's log. The
    /// SQLite backends read only that page, the budget applied in the query;
    /// the default reads [`Self::get_events_since`] and cuts it.
    async fn get_events_page(
        &self,
        execution_id: &ExecutionId,
        after: EventSequence,
        limit: u32,
        max_bytes: usize,
    ) -> BackendResult<Vec<Event>> {
        let events = self.get_events_since(execution_id, after).await?;
        Ok(within_budget(events, limit, max_bytes))
    }

    /// Get the latest event sequence for an execution.
    async fn latest_sequence(&self, execution_id: &ExecutionId) -> BackendResult<EventSequence>;

    /// Work items held under a lease that has not expired, ordered by
    /// worker, at most `limit`. The default refuses: a backend that cannot
    /// answer must not report "no workers".
    async fn list_claimed_work_items(&self, limit: u32) -> BackendResult<Vec<ClaimedWorkItem>> {
        let _ = limit;
        Err(StateBackendError::Database(
            "this backend does not list claimed work items".into(),
        ))
    }

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

    /// Start a fresh run in one atomic step: the execution row, its
    /// `WorkflowStarted`, the start node's `NodeScheduled` and the start node's
    /// work item. A reader sees all four or none.
    ///
    /// Written apart, a scheduler tick landing after the execution row and
    /// before `NodeScheduled` found a running execution whose start node (a
    /// root of the graph) had not been scheduled, and scheduled it too: the
    /// start node ran twice. [`create_segment_atomic`](Self::create_segment_atomic)
    /// is the same step for a continuation segment, which also has a seed
    /// snapshot. The execution id MUST be fresh.
    async fn start_execution_atomic(
        &self,
        execution: WorkflowExecution,
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

    /// Claim an idempotency key BEFORE running the effect it names.
    ///
    /// [`Self::get_tool_effect`] can only answer "did this already run to
    /// COMPLETION". Between two live work items for the same node both read
    /// `None` and both fire, so the side effect happens twice — check-then-fire.
    /// This is the reservation the spec calls load-bearing: whoever wins runs the
    /// effect, and everyone else waits for its result instead of repeating it.
    ///
    /// Atomic and fail-safe by construction: it is a single conditional upsert,
    /// so there is no window between testing for a holder and becoming one.
    ///
    /// `ttl` is what keeps a reservation from becoming a WORSE failure than the
    /// double-fire it prevents. A worker that dies mid-tool leaves its row
    /// behind; expiry is what lets the next worker take the key over instead of
    /// the node being unrunnable forever. An expired reservation is therefore
    /// acquirable, and this returns [`ReserveOutcome::Acquired`] for it.
    ///
    /// Reservations are consulted ONLY when no committed effect exists, so a row
    /// left behind after a successful commit blocks nothing — the caller finds
    /// the recorded result first and replays it.
    ///
    /// The holder renews it under the same `lease_fence`. Another attempt of
    /// the same `owner` (a new fence) waits for the TTL like any other worker:
    /// worker ids are fixed (`general-worker-0`), and a worker that panicked
    /// after its effect used to take its own reservation straight back and
    /// fire again. A parked attempt (rate limited) releases it
    /// ([`Self::release_tool_reservation`]), so its retry does not wait.
    async fn reserve_tool_effect(
        &self,
        key: &str,
        execution_id: &ExecutionId,
        node_id: &str,
        owner: &str,
        lease_fence: i64,
        ttl: std::time::Duration,
    ) -> BackendResult<ReserveOutcome>;

    /// Give up a reservation this worker holds, without having recorded a result.
    ///
    /// A reservation left standing is not a correctness problem (it lapses on
    /// its TTL), but until then any other attempt at the node, the same
    /// worker's included, must wait it out. Releasing on a path that ends
    /// without committing (a park, a failure) turns that wait from minutes
    /// into nothing.
    ///
    /// Guarded by owner AND lease fence, because the owner alone is not enough.
    /// Worker ids are configurable and reused, so an older attempt from the
    /// same worker id finishing late would otherwise delete a NEWER attempt's
    /// reservation and let a third worker acquire a key that is actively held.
    /// The fence is what distinguishes the two attempts. A non-matching owner
    /// or fence is a silent no-op, like every other fenced write here.
    async fn release_tool_reservation(
        &self,
        key: &str,
        owner: &str,
        lease_fence: i64,
    ) -> BackendResult<()>;

    // ── Content-addressed artifact store ─────────────────────────────────────

    /// Store bytes in the CAS, keyed by their SHA-256 hash.
    ///
    /// Uses `INSERT OR IGNORE` so writing the same bytes twice is idempotent:
    /// only one row is ever stored per `(tenant_id, hash)`.
    ///
    /// Returns the `ArtifactRef` describing the stored content. Callers must
    /// call this BEFORE `commit_turn` so that no event ever references an
    /// artifact row that doesn't exist yet (put-then-commit write-order invariant).
    async fn put_artifact(
        &self,
        bytes: &[u8],
        media_type: Option<&str>,
    ) -> BackendResult<ArtifactRef>;

    /// Retrieve artifact bytes by hash (tenant-scoped). Returns `None` if the
    /// artifact does not exist for this tenant.
    async fn get_artifact(&self, hash: &str) -> BackendResult<Option<Vec<u8>>>;

    /// The stored size of an artifact, in bytes, without reading it: what a
    /// caller with a byte budget checks before fetching. `None` if it does not
    /// exist. The default reads the bytes; the SQL backends read the size the
    /// engine recorded when it stored them.
    async fn artifact_size(&self, hash: &str) -> BackendResult<Option<u64>> {
        Ok(self.get_artifact(hash).await?.map(|b| b.len() as u64))
    }

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

    /// Claim with a lease of `lease_secs` seconds (the backend default, 30 s,
    /// when `None`), which every renewal honours until the item settles. A
    /// worker running ten-minute steps asks for a lease it can heartbeat
    /// comfortably (PAS spike finding G9, 2026-09-06).
    ///
    /// Required, not defaulted: a default that ignored the length let a wrapper
    /// hand out a 30 s lease while the claim route told the worker it held the
    /// one it asked for. The length is bounded by [`granted_lease_secs`].
    async fn claim_work_item_with_lease(
        &self,
        worker_id: &str,
        queue_types: &[&str],
        lease_secs: Option<u64>,
    ) -> BackendResult<Option<WorkItem>>;

    /// [`Self::claim_work_item_with_lease`], limited by `filter`: with an
    /// `execution_id`, only that execution's items on `queue_types` can be
    /// claimed (and with a `node_id` too, only that node's). A filtered claim
    /// may take an item marked claimable only by execution
    /// (`jamjet_core::node::ClaimMode::ByExecution`); the unfiltered claim
    /// never does. An empty filter claims exactly as
    /// `claim_work_item_with_lease` does.
    ///
    /// The default refuses a non-empty filter: a backend that cannot honour
    /// it must not hand out some other execution's item instead.
    async fn claim_work_item_filtered(
        &self,
        worker_id: &str,
        queue_types: &[&str],
        lease_secs: Option<u64>,
        filter: &ClaimFilter,
    ) -> BackendResult<Option<WorkItem>> {
        if filter.is_empty() {
            return self
                .claim_work_item_with_lease(worker_id, queue_types, lease_secs)
                .await;
        }
        Err(StateBackendError::Database(
            "this backend cannot claim by execution".into(),
        ))
    }

    /// The items matching `filter` on `queue_types` that are leased now (a
    /// lease that has not expired), whoever holds them. The default refuses.
    async fn leased_items(
        &self,
        queue_types: &[&str],
        filter: &ClaimFilter,
    ) -> BackendResult<Vec<LeasedItem>> {
        let _ = (queue_types, filter);
        Err(StateBackendError::Database(
            "this backend cannot list leased items".into(),
        ))
    }

    /// Record the claim gate's mark for `item_id` under `lease_fence`.
    /// `Ok(false)` if the item is not claimed under that fence any more. The
    /// default refuses, so a backend without it cannot hand a lease out again.
    async fn mark_gate(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        mark: GateMark,
    ) -> BackendResult<bool> {
        let _ = (item_id, lease_fence, mark);
        Err(StateBackendError::Database(
            "this backend cannot record the claim gate's mark".into(),
        ))
    }

    /// Move `item_id`'s gate mark under `lease_fence` from `from` to `to`,
    /// only if it is still `from`: a compare-and-swap, so of two claims that
    /// saw the same mark only one goes on. `Ok(false)` if the mark or the
    /// lease moved. The default refuses.
    async fn swap_gate(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        from: GateMark,
        to: GateMark,
    ) -> BackendResult<bool> {
        let _ = (item_id, lease_fence, from, to);
        Err(StateBackendError::Database(
            "this backend cannot record the claim gate's mark".into(),
        ))
    }

    /// [`Self::swap_gate`] to [`GateMark::Released`], for a lease that has not
    /// expired, renewing it for `lease_secs` in the same write. A gate that
    /// outlived its lease used to hand out a lease the sweep was about to take
    /// back, and the node then ran a second time. `Ok(false)` if the mark,
    /// the fence or the expiry moved. The default refuses.
    async fn release_lease(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        from: GateMark,
        lease_secs: u64,
    ) -> BackendResult<bool> {
        let _ = (item_id, lease_fence, from, lease_secs);
        Err(StateBackendError::Database(
            "this backend cannot record the claim gate's release".into(),
        ))
    }

    /// Whether `filter` (which names an execution) matches an item that is
    /// pending and visible now, or claimed: a read-only probe a waiting claim
    /// polls instead of taking the write lock each time. The default says
    /// yes, which only costs a claim attempt.
    async fn has_claimable(
        &self,
        queue_types: &[&str],
        filter: &ClaimFilter,
    ) -> BackendResult<bool> {
        let _ = (queue_types, filter);
        Ok(true)
    }

    /// A claim naming an execution: [`Self::claim_work_item_filtered`] plus
    /// what a caller needs to know when it gets nothing, and idempotence for
    /// one worker.
    ///
    /// 1. An item matching `filter` already leased to `worker_id` comes back
    ///    as it is, same lease fence ([`ExecutionClaim::AlreadyLeased`]): a
    ///    worker whose claim response was lost gets its lease, not a refusal.
    ///    Whether it may be handed out again is the caller's to decide.
    /// 2. Otherwise a pending item is claimed ([`ExecutionClaim::Claimed`]).
    /// 3. Otherwise, if another worker holds one, [`ExecutionClaim::LeasedToOther`];
    ///    else [`ExecutionClaim::Nothing`].
    async fn claim_for_execution(
        &self,
        worker_id: &str,
        queue_types: &[&str],
        lease_secs: Option<u64>,
        filter: &ClaimFilter,
    ) -> BackendResult<ExecutionClaim> {
        if filter.execution_id.is_none() {
            return Err(StateBackendError::Database(
                "a claim for an execution needs its execution_id".into(),
            ));
        }
        let leased = self.leased_items(queue_types, filter).await?;
        if let Some(mine) = leased
            .iter()
            .find(|l| l.item.worker_id.as_deref() == Some(worker_id))
        {
            return Ok(ExecutionClaim::AlreadyLeased(mine.clone()));
        }
        if let Some(item) = self
            .claim_work_item_filtered(worker_id, queue_types, lease_secs, filter)
            .await?
        {
            return Ok(ExecutionClaim::Claimed(item));
        }
        // Read again before answering: the matching item may have been
        // claimed since by this same worker (a concurrent request of its
        // own), even when another lease was seen first. That is its lease,
        // and it gets it back, not `claimed_by_other`.
        let again = self.leased_items(queue_types, filter).await?;
        if let Some(mine) = again
            .iter()
            .find(|l| l.item.worker_id.as_deref() == Some(worker_id))
        {
            return Ok(ExecutionClaim::AlreadyLeased(mine.clone()));
        }
        if !again.is_empty() {
            return Ok(ExecutionClaim::LeasedToOther);
        }
        Ok(ExecutionClaim::Nothing)
    }

    /// Renew the lease, or re-attach it when the presented fence was lost only
    /// to expiry and nobody has claimed the item since, as after an engine
    /// restart longer than the lease (PAS spike finding G8). The fence in the
    /// answer is the one the worker must present from now on.
    async fn heartbeat_lease(
        &self,
        item_id: WorkItemId,
        worker_id: &str,
        lease_fence: i64,
    ) -> BackendResult<LeaseRenewal> {
        self.renew_lease(item_id, worker_id, lease_fence).await?;
        Ok(LeaseRenewal {
            lease_fence,
            reattached: false,
        })
    }

    /// How many times this node has already completed in this execution.
    ///
    /// The `step` component of a tool-effect idempotency key. The default
    /// materialises the whole log and counts, which is correct everywhere and
    /// what every backend did before; SQL backends override it so the claim path
    /// stops transferring and deserializing every event body to reach a number.
    ///
    /// An override MUST return exactly what the default returns. The key is a
    /// persisted identity, so a count that is off by one — or that silently
    /// returns 0 because a JSON predicate stopped matching — changes every key
    /// and re-fires every in-flight tool. `count_node_completions_matches_the_scan`
    /// pins the two against each other.
    async fn count_node_completions(
        &self,
        execution_id: &ExecutionId,
        node_id: &str,
    ) -> BackendResult<u64> {
        let events = self.get_events(execution_id).await?;
        Ok(events
            .iter()
            .filter(|e| {
                matches!(&e.kind, crate::event::EventKind::NodeCompleted { node_id: nid, .. } if nid == node_id)
            })
            .count() as u64)
    }

    /// The work item as the ENGINE recorded it, or `None` if there is no such
    /// item (for this tenant, on a tenant-scoped backend).
    ///
    /// Exists so a route can answer "which execution and node is this item"
    /// without believing the request body. `POST /work-items/:id/complete`
    /// derives the tool-effect idempotency key from these coordinates: taking
    /// them from the body would let a caller holding one item's lease file its
    /// output under another item's key.
    async fn get_work_item(&self, item_id: WorkItemId) -> BackendResult<Option<WorkItem>>;

    /// Settle a work item that is still claimed, with no fence: the legacy
    /// unfenced completion. Returns `true` iff a row was updated.
    ///
    /// Returns `false`, never an error, for an item that is absent, pending
    /// (never claimed, or freed by a reclaim) or already settled, as
    /// [`Self::complete_work_item_fenced`] does. A completion sent again
    /// then settles nothing, so its caller records nothing twice. A scoped
    /// store settles only its own tenant's items; another tenant's is `false`.
    async fn complete_work_item(&self, item_id: WorkItemId) -> BackendResult<bool>;

    /// Fence-guarded completion of a work item. Settles the item ONLY if the
    /// stored `lease_fence` still matches AND the item is still claimable
    /// (`status = 'claimed'`), returning `true` iff a row was actually updated.
    ///
    /// Returns `false` — never an error — on a fence mismatch, an already-settled
    /// item, or one that was reclaimed under a newer fence. This is the
    /// exactly-once-COMMIT guard for the EXTERNAL HTTP worker path: a reclaimed /
    /// replayed worker holding a stale fence is rejected (`false`) so the caller
    /// can refuse to emit a duplicate terminal event. Mirrors the fence semantics
    /// (`term * 2^32 + epoch`) the internal worker tier gets from `commit_turn`.
    async fn complete_work_item_fenced(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
    ) -> BackendResult<bool>;

    /// Mark a work item as failed by writing the terminal `'failed'` status.
    ///
    /// WARNING — this settles the item and NOTHING ELSE. No `NodeFailed` is
    /// appended, and no sweep ever selects `status = 'failed'`, so the scheduler
    /// fold keeps the node in `scheduled` and the execution never reaches a
    /// terminal state. It is the right call only where the caller emits the
    /// terminal event itself.
    ///
    /// For a worker reporting that its node failed, use
    /// [`Self::fail_work_item_fenced`], which applies the same retry and
    /// dead-letter semantics as lease reclamation and tells the caller which
    /// events to emit.
    async fn fail_work_item(&self, item_id: WorkItemId, error: &str) -> BackendResult<()>;

    /// Fence-guarded failure of a claimed work item, with retry semantics.
    ///
    /// The failure counterpart of [`Self::complete_work_item_fenced`], and the
    /// per-item counterpart of [`Self::reclaim_expired_leases`]: it applies the
    /// SAME attempt/backoff/dead-letter rules that reclamation applies to a lease
    /// that expired, so a worker that reports a failure and a worker that dies
    /// holding the item converge on identical state and identical events.
    ///
    /// Returns `Ok(None)` — never an error — when the fence does not match or the
    /// item is no longer `claimed`, exactly like `complete_work_item_fenced`, so a
    /// reclaimed or replayed worker cannot fail an item it no longer owns or
    /// double-emit its terminal event.
    ///
    /// The returned [`FailOutcome`] carries the updated item so the caller can
    /// emit `NodeFailed` (and `RetryScheduled`) with the right attempt number
    /// without re-reading it.
    async fn fail_work_item_fenced(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        error: &str,
    ) -> BackendResult<Option<FailOutcome>>;

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

    /// [`commit_turn`](Self::commit_turn), unless the execution has reached a
    /// terminal state (cancelled, completed, failed, limit exceeded). Then the
    /// item is withdrawn instead, as [`withdraw_work_item_fenced`]
    /// (Self::withdraw_work_item_fenced) does, and nothing is recorded.
    ///
    /// The status check is part of the commit's transaction, so a cancel
    /// cannot land between the check and the write (PR #140 review, finding
    /// I3). The fence is checked first either way: a stale or forged fence
    /// returns `FenceLost` and touches nothing, terminal run or not.
    async fn commit_turn_unless_terminal(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        terminal_event: Event,
        write_snapshot: bool,
    ) -> BackendResult<TurnCommit>;

    /// Settle a claimed work item as failed and record nothing, because its
    /// execution is terminal and nobody wants its result. Fence-guarded like
    /// every other settle: only the holder of a still-claimed item can
    /// withdraw it, so `Ok(false)` for a stale or forged fence, an item that
    /// already settled, or one never claimed. Memory drops the item; SQLite
    /// marks it `'failed'`.
    async fn withdraw_work_item_fenced(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
    ) -> BackendResult<bool>;

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

    /// Return a leased item to `pending` now, same attempt, fence-guarded as
    /// [`Self::park_work_item`], and mark its payload `claim: by_execution`.
    ///
    /// For an item the oldest-first claim leased although its node is
    /// claimable only by its execution: the item lost the marker, and putting
    /// it back plain would hand it to the same claim on its next poll, ahead of
    /// everything queued behind it. Marked, that claim skips it.
    ///
    /// The default parks it without the mark: refused still, but not repaired.
    async fn park_for_execution(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        attempt: u32,
    ) -> BackendResult<bool> {
        let now = chrono::Utc::now().to_rfc3339();
        self.park_work_item(item_id, lease_fence, &now, attempt)
            .await
    }

    /// Fence-guarded finalization of a continue-as-new rollover on the OLD execution.
    ///
    /// In one transaction:
    /// 1. Attempt `UPDATE work_items SET status='completed', completed_at=?, lease_expires_at=NULL
    ///    WHERE id=? AND lease_fence=? AND status='claimed' AND execution_id=?`. Zero rows means
    ///    the fence no longer matches (lease was stolen or item already settled), the item is
    ///    not claimed, or it is another run's: rollback and return `Ok(false)`.
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
    ///
    /// Forward only: a stored row with a higher `last_sequence` is kept, and
    /// so is a higher stored checkpoint, so a second projector's stale write
    /// never moves the read model back. Equal values are written again.
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
    ///
    /// Forward only, row by row and for the checkpoint, as
    /// [`apply_approval_projection`](Self::apply_approval_projection).
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
    ///
    /// Forward only: a higher stored checkpoint is kept.
    async fn set_projector_checkpoint(
        &self,
        projection_name: &str,
        execution_id: &ExecutionId,
        new_checkpoint: i64,
    ) -> BackendResult<()>;

    // ── Features lane ───────────────────────────────────────────────────

    /// Pending rows of live runs (running or paused), oldest projected first,
    /// after `after`, at most `limit`.
    ///
    /// A scoped backend lists its tenant's runs only, judged by the run's own
    /// row (`workflow_executions.tenant_id`). The projection's `tenant_id`
    /// column is never read: the projector writes `default` there for every
    /// row. Whether a run is live comes from its row too: a run that ended
    /// with a request open keeps a `pending` row the projector no longer
    /// visits.
    ///
    /// The rows lag the event log by up to one projector tick. The default
    /// refuses.
    async fn list_pending_approvals(
        &self,
        after: Option<&ApprovalCursor>,
        limit: u32,
    ) -> BackendResult<Vec<PendingApprovalRow>> {
        let _ = (after, limit);
        Err(StateBackendError::Database(
            "this backend cannot list pending approvals across runs".into(),
        ))
    }

    /// The run's pending and claimed items, oldest first. Settled, failed and
    /// dead-lettered ones are left out. A scoped backend answers for its
    /// tenant's runs only, judged by the run's own row, not the item's: items
    /// the scheduler wrote before its pass stamped the run's tenant carry
    /// `default` in their own tenant column. An unknown run, or another
    /// tenant's, has none. The default refuses.
    async fn open_work_items(
        &self,
        execution_id: &ExecutionId,
    ) -> BackendResult<Vec<OpenWorkItem>> {
        let _ = execution_id;
        Err(StateBackendError::Database(
            "this backend cannot list a run's open work items".into(),
        ))
    }

    /// [`start_execution_atomic`](Self::start_execution_atomic) with the run
    /// key claimed in the same step, so two starts with one key make one run.
    ///
    /// A key no one holds is given to this run. A key held for the run
    /// `key.replaces` names moves to this run; the old run keeps its rows.
    /// Any other held key is [`KeyedStart::Taken`], with what it names, and
    /// nothing is written. A start that fails leaves neither the key nor the
    /// run. The store writes `key.run_key` into the `WorkflowStarted`.
    ///
    /// Keys are per tenant: a scoped backend claims under its tenant, an
    /// unscoped one under the work item's. The default refuses.
    async fn start_execution_keyed(
        &self,
        execution: WorkflowExecution,
        started_event: EventKind,
        scheduled_event: EventKind,
        work_item: WorkItem,
        key: RunKeyClaim,
    ) -> BackendResult<KeyedStart> {
        let _ = (execution, started_event, scheduled_event, work_item, key);
        Err(StateBackendError::Database(
            "this backend cannot start a run by key".into(),
        ))
    }

    /// What `run_key` names in this backend's tenant, or `None`. An unscoped
    /// backend reads the default tenant's keys. The default refuses.
    async fn get_run_key(&self, run_key: &str) -> BackendResult<Option<RunKeyRecord>> {
        let _ = run_key;
        Err(StateBackendError::Database(
            "this backend cannot look up a run key".into(),
        ))
    }

    // ── Tenant management ───────────────────────────────────────────────

    /// Create a new tenant.
    async fn create_tenant(&self, _tenant: Tenant) -> BackendResult<()> {
        Err(StateBackendError::Database(
            "tenant management not supported".into(),
        ))
    }

    /// Get a tenant by ID.
    ///
    /// The default refuses rather than answering "no such tenant": the
    /// tenant's policy is a ceiling every node is checked under, and a backend
    /// that forgot this method would drop it silently. A backend with no
    /// tenants returns `Ok(None)` on purpose.
    async fn get_tenant(&self, _id: &TenantId) -> BackendResult<Option<Tenant>> {
        Err(StateBackendError::Database(
            "this backend cannot read tenants; refusing to evaluate without the tenant policy"
                .into(),
        ))
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

    // ── Storage lane ─────────────────────────────────────────────────────

    /// Write one scheduler pass over one run in one atomic step: its events
    /// in order, its work items, then its finish, or nothing at all.
    ///
    /// The step is refused, with nothing written, when the run is terminal
    /// ([`PassCommit::ExecutionTerminal`]), when the run's log holds anything
    /// after `pass.seen_sequence` ([`PassCommit::Stale`]), or when a node the
    /// pass dispatches already has a pending or claimed item (also `Stale`).
    /// The scheduler then folds the log again and builds a new pass. So two
    /// engines that read the same log cannot both dispatch a node, skip it,
    /// fail it or finish the run: the second pass finds the log moved.
    ///
    /// Each item is stamped with the run's tenant, read from the run's row,
    /// and so are the events. A pass that does not hold together (an item of
    /// another run, an item whose node the pass does not schedule, two items
    /// for one node, a finish whose status is not terminal) is a
    /// [`StateBackendError::Database`] error and writes nothing. An unknown
    /// run is `NotFound`. An empty pass writes nothing and answers
    /// `Committed { last_sequence: seen_sequence }` without taking a lock.
    ///
    /// The default writes in steps, as the scheduler did before this method:
    /// `append_event` for each event, `enqueue_work_item` for each item, then
    /// `finish_execution`. It refuses a pass that does not hold together, but
    /// checks neither the log, nor live items, nor the run's status, leaves
    /// the tenant to those methods, and a failure part way leaves what was
    /// already written. It is there so a backend outside this repository
    /// keeps compiling and behaving as it did; a backend that runs with more
    /// than one engine must override it.
    async fn commit_scheduler_pass(&self, pass: SchedulerPass) -> BackendResult<PassCommit> {
        crate::legacy::scheduler_pass_in_steps(self, pass).await
    }

    /// [`reclaim_expired_leases`](Self::reclaim_expired_leases), with each
    /// reclaimed item's events written in the same atomic step as its settle:
    /// for a requeued item `NodeFailed { retryable: true }` and
    /// `RetryScheduled`, for a dead-lettered one `NodeFailed { retryable:
    /// false }`, as [`crate::lease_events::sweep_events`] builds them. One step
    /// per item, so a failure leaves each item either reclaimed and recorded
    /// or untouched, never settled with no history. The scheduler's sweep
    /// calls this and appends nothing itself.
    ///
    /// Each item is read again inside its step, and reclaimed only while it
    /// is still claimed under the lease that expired and that lease has not
    /// been renewed since. An item whose run is terminal is settled failed
    /// and records nothing, as the claim's fast reclaim does, and is left out
    /// of the result. The result holds only the items that were requeued or
    /// dead-lettered and recorded, with their new attempt numbers.
    ///
    /// The default reclaims with `reclaim_expired_leases` and then appends
    /// the events one by one, as the scheduler did before this method: no
    /// check of the run's status, and a failure part way leaves settled items
    /// with no events. A backend that runs with more than one engine must
    /// override it.
    async fn reclaim_expired_leases_recorded(&self) -> BackendResult<ReclaimResult> {
        crate::legacy::reclaim_in_steps(self).await
    }

    /// [`fail_work_item_fenced`](Self::fail_work_item_fenced), with its
    /// `NodeFailed` (and, for a retry, `RetryScheduled`) written in the same
    /// atomic step as the settle, as [`crate::lease_events::fail_events`]
    /// builds them from the outcome and `error`. The fail route calls this
    /// and appends nothing itself.
    ///
    /// `Ok(None)`, with nothing written, for a fence that does not match or
    /// an item that is no longer claimed. When the item's run is terminal the
    /// item is settled failed, nothing is recorded, and the answer is also
    /// `Ok(None)`: a cancel that lands after the caller read the run cannot
    /// be followed by a failure trail.
    ///
    /// The default fails the item with `fail_work_item_fenced` and then
    /// appends the events one by one, as the fail route did before this
    /// method: no check of the run's status, and a failure after the settle
    /// leaves it with no events. A backend that runs with more than one
    /// engine must override it.
    async fn fail_work_item_fenced_recorded(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        error: &str,
    ) -> BackendResult<Option<FailOutcome>> {
        crate::legacy::fail_in_steps(self, item_id, lease_fence, error).await
    }

    /// The time decisions use: the database's on Postgres, this process's
    /// elsewhere.
    ///
    /// Approval deadlines are stamped and checked with it, so engines whose
    /// clocks disagree still agree on when a request expires. The default is
    /// [`crate::clock::process_now`], which is right for a store that one
    /// process owns.
    async fn store_now(&self) -> BackendResult<chrono::DateTime<chrono::Utc>> {
        Ok(crate::clock::process_now())
    }

    // ── Request-driven mode ──────────────────────────────────────────────

    /// Take the tick lease for `holder` until `lease` from now by the
    /// store's clock, when no one holds it or its holder's lease has lapsed.
    /// [`TickBegin::Began`] carries where the last finished tick stopped.
    /// [`TickBegin::Busy`] names the holder of a live lease, and nothing is
    /// written.
    ///
    /// A store has one lease: every handle on it, scoped to a tenant or not,
    /// takes the same one. The lease saves work, it does not make it
    /// correct: a tick that runs past its lease still writes through the
    /// store's conditional writes. A lease longer than [`MAX_TICK_LEASE`] is
    /// refused. The default refuses.
    async fn begin_tick(
        &self,
        holder: &str,
        lease: std::time::Duration,
    ) -> BackendResult<TickBegin> {
        let _ = (holder, lease);
        Err(cannot_coordinate_ticks())
    }

    /// Give the tick lease back and store `outcome`, if the tick row still
    /// names `holder`. `false`, with nothing written, when it does not: the
    /// lease lapsed and another holder took it, `holder` never held it, or
    /// it was given back already. The default refuses.
    async fn finish_tick(&self, holder: &str, outcome: &TickOutcome) -> BackendResult<bool> {
        let _ = (holder, outcome);
        Err(cannot_coordinate_ticks())
    }

    /// The tick row as it stands, `None` until the first tick begins. The
    /// default refuses.
    async fn tick_status(&self) -> BackendResult<Option<TickStatus>> {
        Err(cannot_coordinate_ticks())
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
/// One keyset page of running executions; see
/// [`StateBackend::list_running_executions_after`].
#[derive(Debug, Clone, Default)]
pub struct RunningPage {
    /// The rows of the page that could be read.
    pub executions: Vec<WorkflowExecution>,
    /// The id of the last row scanned, readable or not: the `after` of the
    /// next page. `None` once a page scans nothing.
    pub next_after: Option<String>,
}

/// A work item a worker holds a live lease on: what `GET /workers` reports.
/// There is no worker registry, so a worker shows up only while it holds one.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClaimedWorkItem {
    pub worker_id: String,
    pub work_item_id: WorkItemId,
    pub execution_id: ExecutionId,
    pub node_id: String,
    pub queue_type: String,
    pub lease_expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

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

/// The lease a claim gets when the worker does not say how long it needs.
pub const DEFAULT_LEASE_SECS: u64 = 30;

/// The longest an effect reservation is held, whatever is asked: far past
/// any node timeout registration allows, and far inside what a timestamp can
/// hold. A huge TTL used to overflow the expiry and panic the worker.
pub const MAX_RESERVATION_TTL: std::time::Duration =
    std::time::Duration::from_secs(100 * 365 * 24 * 60 * 60);

/// When a reservation of `ttl` taken at `now` expires: `ttl` held to
/// [`MAX_RESERVATION_TTL`], added without overflowing.
pub fn reservation_expiry(
    now: chrono::DateTime<chrono::Utc>,
    ttl: std::time::Duration,
) -> chrono::DateTime<chrono::Utc> {
    let held = chrono::Duration::from_std(ttl.min(MAX_RESERVATION_TTL))
        .unwrap_or_else(|_| chrono::Duration::days(36_500));
    now.checked_add_signed(held)
        .unwrap_or_else(|| now + chrono::Duration::days(36_500))
}
/// The shortest lease a claim may ask for: long enough to be renewable.
pub const MIN_LEASE_SECS: u64 = 5;
/// The longest lease a claim may ask for: short enough that a dead worker's
/// item is not stranded for hours.
pub const MAX_LEASE_SECS: u64 = 3600;

/// The start of `events` a page takes: at most `limit` events, and none
/// after the one at which their serialized length reaches `max_bytes` (so at
/// least one, when there is one). The SQL backends do the same in the query,
/// with an estimate no smaller than the serialized length, so a page of large
/// events is never read past its budget.
pub fn within_budget(events: Vec<Event>, limit: u32, max_bytes: usize) -> Vec<Event> {
    let mut total = 0usize;
    events
        .into_iter()
        .take(limit as usize)
        .take_while(|e| {
            let fits = total < max_bytes;
            total += serde_json::to_vec(e).map(|b| b.len()).unwrap_or(0);
            fits
        })
        .collect()
}

/// The lease a claim is granted for what it asked: the default when it asked
/// for nothing, otherwise the request held to `[MIN_LEASE_SECS,
/// MAX_LEASE_SECS]`. Every backend applies it, and the claim route echoes it,
/// so the worker paces its heartbeats from the lease it actually holds.
pub fn granted_lease_secs(requested: Option<u64>) -> u64 {
    requested.map_or(DEFAULT_LEASE_SECS, |s| {
        s.clamp(MIN_LEASE_SECS, MAX_LEASE_SECS)
    })
}

/// The lease epoch a fence was minted at. A fence is `term * 2^32 + epoch`,
/// and every transition that takes a lease away or hands one out (claim,
/// expiry, fail, park, re-attach) moves the item's epoch on by one.
pub fn fence_epoch(lease_fence: i64) -> i64 {
    lease_fence & 0xFFFF_FFFF
}

/// The epoch an item is at when the lease minted as `lost_fence` expired and
/// NOTHING has happened to the item since. Re-attach requires exactly this:
/// any later claim, failure or park has moved the epoch further, so a zombie
/// cannot take back an item another worker claimed and then released (PR #140
/// review, finding C1).
pub fn epoch_after_expiry_of(lost_fence: i64) -> i64 {
    fence_epoch(lost_fence) + 1
}

/// What [`StateBackend::finish_execution`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionFinish {
    /// The status was set and the terminal event landed at this sequence.
    Finished(EventSequence),
    /// The execution was already terminal: nothing was written.
    AlreadyTerminal(WorkflowStatus),
}

/// The condition [`StateBackend::append_event_if`] appends under: a pure
/// function of an execution's whole event log.
pub type EventCondition = dyn for<'e> Fn(&'e [Event]) -> bool + Send + Sync;

/// What [`StateBackend::append_event_if`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConditionalAppend {
    /// The condition held: the event landed at this sequence.
    Appended(EventSequence),
    /// The condition did not hold: nothing was written.
    Refused,
    /// The execution is terminal: nothing was written.
    ExecutionTerminal(WorkflowStatus),
}

/// What [`StateBackend::commit_turn_unless_terminal`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnCommit {
    /// The item settled and the terminal event landed at this sequence.
    Committed(EventSequence),
    /// The execution was already terminal: the item was withdrawn and nothing
    /// was recorded.
    ExecutionTerminal(WorkflowStatus),
}

/// What a heartbeat hands back: the fence the worker must present from now
/// on, and whether the lease had to be re-attached after an expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRenewal {
    pub lease_fence: i64,
    pub reattached: bool,
}

/// Result of reclaiming expired work item leases.
#[derive(Debug, Default)]
pub struct ReclaimResult {
    /// Items whose lease expired and have been reset to `pending` for retry.
    pub retryable: Vec<WorkItem>,
    /// Items that exhausted all attempts and were moved to dead-letter.
    pub exhausted: Vec<WorkItem>,
}

/// The result of trying to claim an idempotency key before firing its effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveOutcome {
    /// The key is ours: no live reservation existed, or the previous holder's
    /// TTL had lapsed and we took it over. Run the effect.
    Acquired,
    /// Another worker holds a LIVE reservation. Do NOT run the effect — wait for
    /// its result instead. `expires_at` is when the claim lapses, which bounds
    /// how long waiting can be worthwhile.
    Held { owner: String, expires_at: String },
}

/// What a fenced failure did to the item, and the item as it now stands.
///
/// The caller emits the terminal events from this, so the variants mirror the
/// two arms of [`ReclaimResult`] deliberately: a worker-reported failure and an
/// expired lease must leave the same event trail, or the scheduler fold learns a
/// different story depending on how the node died.
#[derive(Debug, Clone)]
pub enum FailOutcome {
    /// Attempts remain: the item is back to `pending` with an incremented
    /// attempt and a backoff, and a fresh lease epoch. Emit
    /// `NodeFailed { retryable: true }` then `RetryScheduled`.
    Retryable {
        /// The item with its NEW attempt number.
        item: Box<WorkItem>,
        /// Backoff applied before the item becomes claimable again.
        delay_ms: u64,
    },
    /// Attempts are spent: the item is dead-lettered. Emit
    /// `NodeFailed { retryable: false }` and nothing else.
    Exhausted {
        /// The item with its FINAL attempt number.
        item: Box<WorkItem>,
    },
}

// ── Storage lane ─────────────────────────────────────────────────────────────

/// One scheduler pass over one run, written in one transaction by
/// [`StateBackend::commit_scheduler_pass`].
#[derive(Debug, Clone)]
pub struct SchedulerPass {
    pub execution_id: ExecutionId,
    /// The highest sequence the pass folded. Refused if the log holds anything later.
    pub seen_sequence: EventSequence,
    /// Appended in this order; the backend assigns the sequences.
    pub events: Vec<EventKind>,
    /// Inserted with the events. Each item's execution is this run, its node has a
    /// NodeScheduled in `events`, and the backend stamps the run's tenant from its row.
    pub work_items: Vec<WorkItem>,
    /// Ends the run after `events`: a terminal status and its terminal event.
    pub finish: Option<(WorkflowStatus, EventKind)>,
}

impl SchedulerPass {
    /// Nothing to write: no events, no items, no finish.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty() && self.work_items.is_empty() && self.finish.is_none()
    }

    /// The shape every backend refuses before writing anything: an item of
    /// another run, an item whose node has no `NodeScheduled` in `events`,
    /// two items for one node, a finish whose status is not terminal.
    pub(crate) fn check_shape(&self) -> BackendResult<()> {
        let refuse = |why: String| Err(StateBackendError::Database(why));
        let mut nodes = std::collections::HashSet::new();
        for item in &self.work_items {
            if item.execution_id != self.execution_id {
                return refuse(format!(
                    "scheduler pass for {} carries an item of {}",
                    self.execution_id, item.execution_id
                ));
            }
            let scheduled = self.events.iter().any(|kind| {
                matches!(kind, EventKind::NodeScheduled { node_id, .. } if *node_id == item.node_id)
            });
            if !scheduled {
                return refuse(format!(
                    "scheduler pass carries an item for {} with no NodeScheduled",
                    item.node_id
                ));
            }
            if !nodes.insert(item.node_id.as_str()) {
                return refuse(format!(
                    "scheduler pass carries two items for {}",
                    item.node_id
                ));
            }
        }
        if let Some((status, _)) = &self.finish {
            if !status.is_terminal() {
                return refuse(format!(
                    "scheduler pass finish requires a terminal status, got {status:?}"
                ));
            }
        }
        Ok(())
    }
}

/// What [`StateBackend::commit_scheduler_pass`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassCommit {
    /// Everything landed; the pass's last event is at `last_sequence`.
    Committed { last_sequence: EventSequence },
    /// Nothing written: the log moved past `seen_sequence`, or a node in `work_items`
    /// already has a pending or claimed item.
    Stale { latest_sequence: EventSequence },
    /// Nothing written: the run is terminal.
    ExecutionTerminal(WorkflowStatus),
}

// ── Request-driven mode ──────────────────────────────────────────────────────

/// The longest tick lease a store takes ([`StateBackend::begin_tick`]).
pub const MAX_TICK_LEASE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// What [`StateBackend::begin_tick`] found.
#[derive(Debug, Clone, PartialEq)]
pub enum TickBegin {
    /// The lease is the caller's now. `cursor` is where the last finished
    /// tick stopped its walk (`None`: start from the beginning), and
    /// `last_finished_at` when it finished (`None`: no tick has finished).
    Began {
        cursor: Option<String>,
        last_finished_at: Option<chrono::DateTime<chrono::Utc>>,
    },
    /// `holder`'s lease is live until `lease_until`, by the store's clock.
    /// Nothing was written.
    Busy {
        holder: String,
        lease_until: chrono::DateTime<chrono::Utc>,
    },
}

/// What a tick stores when it gives its lease back
/// ([`StateBackend::finish_tick`]).
#[derive(Debug, Clone, PartialEq)]
pub struct TickOutcome {
    /// Where the walk stopped, for the next tick to resume from. `None`
    /// starts the next one from the beginning.
    pub cursor: Option<String>,
    /// Whether the tick got through its work within its budget.
    pub complete: bool,
    /// The tick's report, kept as it is for [`StateBackend::tick_status`].
    pub report: serde_json::Value,
}

/// The tick row as it stands ([`StateBackend::tick_status`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TickStatus {
    /// Who holds the lease, `None` while no tick does.
    pub holder: Option<String>,
    /// When the holder's lease ends, by the store's clock.
    pub lease_until: Option<chrono::DateTime<chrono::Utc>>,
    /// When the last tick began, and when the last finished one finished.
    pub last_started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_finished_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The last finished tick's outcome: whether it got through, its
    /// report, and where it stopped.
    pub last_complete: Option<bool>,
    pub last_report: Option<serde_json::Value>,
    pub cursor: Option<String>,
}

/// The refusal of a store that has no tick state.
fn cannot_coordinate_ticks() -> StateBackendError {
    StateBackendError::Database("this backend cannot coordinate ticks".into())
}

/// `lease` as a duration the store can add to its clock: refused past
/// [`MAX_TICK_LEASE`], so no backend overflows its clock.
pub(crate) fn tick_lease(lease: std::time::Duration) -> BackendResult<chrono::Duration> {
    if lease > MAX_TICK_LEASE {
        return Err(StateBackendError::Database(format!(
            "a tick lease of {lease:?} is longer than the {MAX_TICK_LEASE:?} a store takes"
        )));
    }
    chrono::Duration::from_std(lease)
        .map_err(|e| StateBackendError::Database(format!("tick lease {lease:?}: {e}")))
}

#[cfg(test)]
mod reservation_tests {
    /// A huge TTL neither panics nor leaves the four-digit years the
    /// reservation table compares as text.
    #[test]
    fn a_huge_reservation_ttl_has_a_comparable_expiry() {
        let now = chrono::Utc::now();
        // Far, not the 5-minute fallback that reopened the double-fire
        // window, and never a panic (10^13 s converts but overflows the add).
        for ttl in [
            std::time::Duration::MAX,
            std::time::Duration::from_secs(10_000_000_000_000),
        ] {
            let at = super::reservation_expiry(now, ttl);
            assert!(
                at >= now + chrono::Duration::days(365 * 50),
                "{ttl:?}: {at}"
            );
            assert_eq!(at.format("%Y").to_string().len(), 4, "{ttl:?}: {at}");
        }
    }
}
