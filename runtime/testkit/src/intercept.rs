//! A [`StateBackend`] that hands every call to a test's hooks.
//!
//! [`Intercept`] delegates each trait method to `inner`, the defaults
//! included: a wrapper that left one to its default would run the trait's
//! fallback instead of the wrapped backend's own code, and the test would
//! measure something the engine never runs. Around each call it asks
//! [`Hooks::before`], which can fail the call before it reaches `inner`, and
//! after the call returned `Ok` it tells [`Hooks::after`], which can hold the
//! caller there, for instance until a second engine reaches the same point.

use async_trait::async_trait;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::artifact::ArtifactRef;
use jamjet_state::backend::{
    ApiToken, ApprovalProjectionRow, BackendResult, ClaimFilter, ClaimedWorkItem,
    ConditionalAppend, EventCondition, ExecutionClaim, ExecutionFinish, FailOutcome, GateMark,
    LeaseRenewal, LeasedItem, PassCommit, ReclaimResult, ReserveOutcome, RunningPage,
    SchedulerPass, StateBackend, StateBackendError, TurnCommit, WorkItem, WorkItemId,
    WorkflowDefinition,
};
use jamjet_state::event::{Event, EventKind, EventSequence};
use jamjet_state::snapshot::Snapshot;
use jamjet_state::tenant::{Tenant, TenantId};
use serde_json::Value;
use std::sync::Arc;

/// One call into the backend, as [`Intercept`] reports it to its hooks: one
/// variant per [`StateBackend`] method, with the arguments that say what the
/// call touches.
#[derive(Debug, Clone, Copy)]
pub enum Op<'a> {
    // ── Workflow definitions ─────────────────────────────────────────────
    StoreWorkflow(&'a WorkflowDefinition),
    GetWorkflow {
        workflow_id: &'a str,
        version: &'a str,
    },

    // ── Workflow executions ──────────────────────────────────────────────
    CreateExecution(&'a WorkflowExecution),
    GetExecution(&'a ExecutionId),
    ExecutionTenant(&'a ExecutionId),
    UpdateExecutionStatus {
        id: &'a ExecutionId,
        status: &'a WorkflowStatus,
    },
    FinishExecution {
        event: &'a Event,
        status: &'a WorkflowStatus,
    },
    AppendEventIf(&'a Event),
    UpdateExecutionCurrentState {
        id: &'a ExecutionId,
        current_state: &'a Value,
    },
    PatchAppendArray {
        execution_id: &'a ExecutionId,
        key: &'a str,
        value: &'a Value,
    },
    ListExecutions {
        status: Option<&'a WorkflowStatus>,
        limit: u32,
        offset: u32,
    },
    ListRunningExecutionsAfter {
        after: Option<&'a str>,
        limit: u32,
    },

    // ── Event log ────────────────────────────────────────────────────────
    AppendEvent(&'a Event),
    GetEvents(&'a ExecutionId),
    GetEventsSince(&'a ExecutionId),
    GetEventsPage {
        execution_id: &'a ExecutionId,
        after: EventSequence,
        limit: u32,
        max_bytes: usize,
    },
    LatestSequence(&'a ExecutionId),
    ListClaimedWorkItems {
        limit: u32,
    },

    // ── Snapshots and run creation ───────────────────────────────────────
    WriteSnapshot(&'a Snapshot),
    LatestSnapshot(&'a ExecutionId),
    CreateSegmentAtomic {
        execution: &'a WorkflowExecution,
        work_item: &'a WorkItem,
    },
    StartExecutionAtomic {
        execution: &'a WorkflowExecution,
        work_item: &'a WorkItem,
    },

    // ── Tool effects ─────────────────────────────────────────────────────
    GetToolEffect(&'a str),
    ReserveToolEffect {
        key: &'a str,
        execution_id: &'a ExecutionId,
        node_id: &'a str,
        owner: &'a str,
        lease_fence: i64,
    },
    ReleaseToolReservation {
        key: &'a str,
        owner: &'a str,
        lease_fence: i64,
    },

    // ── Artifacts ────────────────────────────────────────────────────────
    PutArtifact {
        bytes: &'a [u8],
        media_type: Option<&'a str>,
    },
    GetArtifact(&'a str),
    ArtifactSize(&'a str),

    // ── Queue ────────────────────────────────────────────────────────────
    EnqueueWorkItem(&'a WorkItem),
    ClaimWorkItem {
        worker_id: &'a str,
        queue_types: &'a [&'a str],
    },
    RenewLease {
        item_id: WorkItemId,
        worker_id: &'a str,
        lease_fence: i64,
    },
    ClaimWorkItemWithLease {
        worker_id: &'a str,
        queue_types: &'a [&'a str],
        lease_secs: Option<u64>,
    },
    ClaimWorkItemFiltered {
        worker_id: &'a str,
        queue_types: &'a [&'a str],
        lease_secs: Option<u64>,
        filter: &'a ClaimFilter,
    },
    LeasedItems {
        queue_types: &'a [&'a str],
        filter: &'a ClaimFilter,
    },
    MarkGate {
        item_id: WorkItemId,
        lease_fence: i64,
        mark: GateMark,
    },
    SwapGate {
        item_id: WorkItemId,
        lease_fence: i64,
        from: GateMark,
        to: GateMark,
    },
    ReleaseLease {
        item_id: WorkItemId,
        lease_fence: i64,
        from: GateMark,
        lease_secs: u64,
    },
    HasClaimable {
        queue_types: &'a [&'a str],
        filter: &'a ClaimFilter,
    },
    ClaimForExecution {
        worker_id: &'a str,
        queue_types: &'a [&'a str],
        lease_secs: Option<u64>,
        filter: &'a ClaimFilter,
    },
    HeartbeatLease {
        item_id: WorkItemId,
        worker_id: &'a str,
        lease_fence: i64,
    },
    CountNodeCompletions {
        execution_id: &'a ExecutionId,
        node_id: &'a str,
    },
    GetWorkItem(WorkItemId),
    CompleteWorkItem(WorkItemId),
    CompleteWorkItemFenced {
        item_id: WorkItemId,
        lease_fence: i64,
    },
    FailWorkItem {
        item_id: WorkItemId,
        error: &'a str,
    },
    FailWorkItemFenced {
        item_id: WorkItemId,
        lease_fence: i64,
        error: &'a str,
    },
    CommitTurn {
        item_id: WorkItemId,
        lease_fence: i64,
        event: &'a Event,
        write_snapshot: bool,
    },
    CommitTurnUnlessTerminal {
        item_id: WorkItemId,
        lease_fence: i64,
        event: &'a Event,
        write_snapshot: bool,
    },
    WithdrawWorkItemFenced {
        item_id: WorkItemId,
        lease_fence: i64,
    },
    ParkWorkItem {
        item_id: WorkItemId,
        lease_fence: i64,
        retry_after: &'a str,
        next_attempt: u32,
    },
    ParkForExecution {
        item_id: WorkItemId,
        lease_fence: i64,
        attempt: u32,
    },
    FinalizeRolloverFenced {
        execution_id: &'a ExecutionId,
        item_id: WorkItemId,
        lease_fence: i64,
    },
    ReclaimExpiredLeases,
    MoveToDeadLetter {
        item_id: WorkItemId,
        last_error: &'a str,
    },

    // ── API tokens ───────────────────────────────────────────────────────
    CreateToken {
        name: &'a str,
        role: &'a str,
    },
    /// The plaintext token is left out, so a hook that logs its ops does
    /// not log a credential.
    ValidateToken,

    // ── Projector ────────────────────────────────────────────────────────
    ApplyApprovalProjection {
        row: &'a ApprovalProjectionRow,
        projection_name: &'a str,
        new_checkpoint: i64,
    },
    ApplyApprovalProjectionBatch {
        rows: &'a [ApprovalProjectionRow],
        projection_name: &'a str,
        execution_id: &'a ExecutionId,
        new_checkpoint: i64,
    },
    GetApprovalProjection(&'a ExecutionId),
    GetProjectorCheckpoint {
        projection_name: &'a str,
        execution_id: &'a ExecutionId,
    },
    SetProjectorCheckpoint {
        projection_name: &'a str,
        execution_id: &'a ExecutionId,
        new_checkpoint: i64,
    },

    // ── Tenants ──────────────────────────────────────────────────────────
    CreateTenant(&'a Tenant),
    GetTenant(&'a TenantId),
    ListTenants,
    UpdateTenant(&'a Tenant),

    // ── Storage lane ─────────────────────────────────────────────────────
    CommitSchedulerPass(&'a ExecutionId),
    ReclaimExpiredLeasesRecorded,
    FailWorkItemFencedRecorded {
        item_id: WorkItemId,
        lease_fence: i64,
        error: &'a str,
    },
}

/// What a test does around each call [`Intercept`] delegates.
#[async_trait]
pub trait Hooks: Send + Sync {
    /// Before the call. `Some(e)`: the call fails with `e` and nothing
    /// reaches the inner backend.
    async fn before(&self, _op: &Op<'_>) -> Option<StateBackendError> {
        None
    }

    /// After the inner call returned `Ok`, before the caller sees the
    /// result. Not called when the inner call failed.
    async fn after(&self, _op: &Op<'_>) {}
}

/// Hooks that do nothing: [`Intercept`] then behaves as its inner backend.
pub struct NoHooks;

impl Hooks for NoHooks {}

/// A [`StateBackend`] that runs [`Hooks::before`], the inner backend's
/// method, then [`Hooks::after`], for every method of the trait.
pub struct Intercept {
    pub inner: Arc<dyn StateBackend>,
    pub hooks: Arc<dyn Hooks>,
}

impl Intercept {
    pub fn new(inner: Arc<dyn StateBackend>, hooks: Arc<dyn Hooks>) -> Self {
        Self { inner, hooks }
    }
}

/// `before`, the inner call, `after`, in that order. A value the inner call
/// takes by value is cloned into it, so the op can still point at it.
macro_rules! around {
    ($self:ident, $op:expr, $call:expr) => {{
        let op = $op;
        if let Some(e) = $self.hooks.before(&op).await {
            return Err(e);
        }
        let out = $call.await?;
        $self.hooks.after(&op).await;
        Ok(out)
    }};
}

#[async_trait]
impl StateBackend for Intercept {
    // ── Workflow definitions ─────────────────────────────────────────────

    async fn store_workflow(&self, def: WorkflowDefinition) -> BackendResult<()> {
        around!(
            self,
            Op::StoreWorkflow(&def),
            self.inner.store_workflow(def.clone())
        )
    }

    async fn get_workflow(
        &self,
        workflow_id: &str,
        version: &str,
    ) -> BackendResult<Option<WorkflowDefinition>> {
        around!(
            self,
            Op::GetWorkflow {
                workflow_id,
                version
            },
            self.inner.get_workflow(workflow_id, version)
        )
    }

    // ── Workflow executions ──────────────────────────────────────────────

    async fn create_execution(&self, execution: WorkflowExecution) -> BackendResult<()> {
        around!(
            self,
            Op::CreateExecution(&execution),
            self.inner.create_execution(execution.clone())
        )
    }

    async fn get_execution(&self, id: &ExecutionId) -> BackendResult<Option<WorkflowExecution>> {
        around!(self, Op::GetExecution(id), self.inner.get_execution(id))
    }

    async fn execution_tenant(&self, id: &ExecutionId) -> BackendResult<Option<TenantId>> {
        around!(
            self,
            Op::ExecutionTenant(id),
            self.inner.execution_tenant(id)
        )
    }

    async fn update_execution_status(
        &self,
        id: &ExecutionId,
        status: WorkflowStatus,
    ) -> BackendResult<()> {
        around!(
            self,
            Op::UpdateExecutionStatus {
                id,
                status: &status
            },
            self.inner.update_execution_status(id, status.clone())
        )
    }

    async fn finish_execution(
        &self,
        event: Event,
        status: WorkflowStatus,
    ) -> BackendResult<ExecutionFinish> {
        around!(
            self,
            Op::FinishExecution {
                event: &event,
                status: &status
            },
            self.inner.finish_execution(event.clone(), status.clone())
        )
    }

    async fn append_event_if(
        &self,
        event: Event,
        condition: &EventCondition,
    ) -> BackendResult<ConditionalAppend> {
        around!(
            self,
            Op::AppendEventIf(&event),
            self.inner.append_event_if(event.clone(), condition)
        )
    }

    async fn update_execution_current_state(
        &self,
        id: &ExecutionId,
        current_state: &Value,
    ) -> BackendResult<()> {
        around!(
            self,
            Op::UpdateExecutionCurrentState { id, current_state },
            self.inner.update_execution_current_state(id, current_state)
        )
    }

    async fn patch_append_array(
        &self,
        execution_id: &ExecutionId,
        key: &str,
        value: Value,
    ) -> BackendResult<()> {
        around!(
            self,
            Op::PatchAppendArray {
                execution_id,
                key,
                value: &value
            },
            self.inner
                .patch_append_array(execution_id, key, value.clone())
        )
    }

    async fn list_executions(
        &self,
        status: Option<WorkflowStatus>,
        limit: u32,
        offset: u32,
    ) -> BackendResult<Vec<WorkflowExecution>> {
        around!(
            self,
            Op::ListExecutions {
                status: status.as_ref(),
                limit,
                offset
            },
            self.inner.list_executions(status.clone(), limit, offset)
        )
    }

    async fn list_running_executions_after(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> BackendResult<RunningPage> {
        around!(
            self,
            Op::ListRunningExecutionsAfter { after, limit },
            self.inner.list_running_executions_after(after, limit)
        )
    }

    // ── Event log ────────────────────────────────────────────────────────

    async fn append_event(&self, event: Event) -> BackendResult<EventSequence> {
        around!(
            self,
            Op::AppendEvent(&event),
            self.inner.append_event(event.clone())
        )
    }

    async fn get_events(&self, execution_id: &ExecutionId) -> BackendResult<Vec<Event>> {
        around!(
            self,
            Op::GetEvents(execution_id),
            self.inner.get_events(execution_id)
        )
    }

    async fn get_events_since(
        &self,
        execution_id: &ExecutionId,
        since_sequence: EventSequence,
    ) -> BackendResult<Vec<Event>> {
        around!(
            self,
            Op::GetEventsSince(execution_id),
            self.inner.get_events_since(execution_id, since_sequence)
        )
    }

    async fn get_events_page(
        &self,
        execution_id: &ExecutionId,
        after: EventSequence,
        limit: u32,
        max_bytes: usize,
    ) -> BackendResult<Vec<Event>> {
        around!(
            self,
            Op::GetEventsPage {
                execution_id,
                after,
                limit,
                max_bytes
            },
            self.inner
                .get_events_page(execution_id, after, limit, max_bytes)
        )
    }

    async fn latest_sequence(&self, execution_id: &ExecutionId) -> BackendResult<EventSequence> {
        around!(
            self,
            Op::LatestSequence(execution_id),
            self.inner.latest_sequence(execution_id)
        )
    }

    async fn list_claimed_work_items(&self, limit: u32) -> BackendResult<Vec<ClaimedWorkItem>> {
        around!(
            self,
            Op::ListClaimedWorkItems { limit },
            self.inner.list_claimed_work_items(limit)
        )
    }

    // ── Snapshots and run creation ───────────────────────────────────────

    async fn write_snapshot(&self, snapshot: Snapshot) -> BackendResult<()> {
        around!(
            self,
            Op::WriteSnapshot(&snapshot),
            self.inner.write_snapshot(snapshot.clone())
        )
    }

    async fn latest_snapshot(&self, execution_id: &ExecutionId) -> BackendResult<Option<Snapshot>> {
        around!(
            self,
            Op::LatestSnapshot(execution_id),
            self.inner.latest_snapshot(execution_id)
        )
    }

    async fn create_segment_atomic(
        &self,
        execution: WorkflowExecution,
        seed_snapshot: Snapshot,
        started_event: EventKind,
        scheduled_event: EventKind,
        work_item: WorkItem,
    ) -> BackendResult<()> {
        around!(
            self,
            Op::CreateSegmentAtomic {
                execution: &execution,
                work_item: &work_item
            },
            self.inner.create_segment_atomic(
                execution.clone(),
                seed_snapshot,
                started_event,
                scheduled_event,
                work_item.clone(),
            )
        )
    }

    async fn start_execution_atomic(
        &self,
        execution: WorkflowExecution,
        started_event: EventKind,
        scheduled_event: EventKind,
        work_item: WorkItem,
    ) -> BackendResult<()> {
        around!(
            self,
            Op::StartExecutionAtomic {
                execution: &execution,
                work_item: &work_item
            },
            self.inner.start_execution_atomic(
                execution.clone(),
                started_event,
                scheduled_event,
                work_item.clone(),
            )
        )
    }

    // ── Tool effects ─────────────────────────────────────────────────────

    async fn get_tool_effect(&self, key: &str) -> BackendResult<Option<Value>> {
        around!(
            self,
            Op::GetToolEffect(key),
            self.inner.get_tool_effect(key)
        )
    }

    async fn reserve_tool_effect(
        &self,
        key: &str,
        execution_id: &ExecutionId,
        node_id: &str,
        owner: &str,
        lease_fence: i64,
        ttl: std::time::Duration,
    ) -> BackendResult<ReserveOutcome> {
        around!(
            self,
            Op::ReserveToolEffect {
                key,
                execution_id,
                node_id,
                owner,
                lease_fence
            },
            self.inner
                .reserve_tool_effect(key, execution_id, node_id, owner, lease_fence, ttl)
        )
    }

    async fn release_tool_reservation(
        &self,
        key: &str,
        owner: &str,
        lease_fence: i64,
    ) -> BackendResult<()> {
        around!(
            self,
            Op::ReleaseToolReservation {
                key,
                owner,
                lease_fence
            },
            self.inner.release_tool_reservation(key, owner, lease_fence)
        )
    }

    // ── Artifacts ────────────────────────────────────────────────────────

    async fn put_artifact(
        &self,
        bytes: &[u8],
        media_type: Option<&str>,
    ) -> BackendResult<ArtifactRef> {
        around!(
            self,
            Op::PutArtifact { bytes, media_type },
            self.inner.put_artifact(bytes, media_type)
        )
    }

    async fn get_artifact(&self, hash: &str) -> BackendResult<Option<Vec<u8>>> {
        around!(self, Op::GetArtifact(hash), self.inner.get_artifact(hash))
    }

    async fn artifact_size(&self, hash: &str) -> BackendResult<Option<u64>> {
        around!(self, Op::ArtifactSize(hash), self.inner.artifact_size(hash))
    }

    // ── Queue ────────────────────────────────────────────────────────────

    async fn enqueue_work_item(&self, item: WorkItem) -> BackendResult<WorkItemId> {
        around!(
            self,
            Op::EnqueueWorkItem(&item),
            self.inner.enqueue_work_item(item.clone())
        )
    }

    async fn claim_work_item(
        &self,
        worker_id: &str,
        queue_types: &[&str],
    ) -> BackendResult<Option<WorkItem>> {
        around!(
            self,
            Op::ClaimWorkItem {
                worker_id,
                queue_types
            },
            self.inner.claim_work_item(worker_id, queue_types)
        )
    }

    async fn renew_lease(
        &self,
        item_id: WorkItemId,
        worker_id: &str,
        lease_fence: i64,
    ) -> BackendResult<()> {
        around!(
            self,
            Op::RenewLease {
                item_id,
                worker_id,
                lease_fence
            },
            self.inner.renew_lease(item_id, worker_id, lease_fence)
        )
    }

    async fn claim_work_item_with_lease(
        &self,
        worker_id: &str,
        queue_types: &[&str],
        lease_secs: Option<u64>,
    ) -> BackendResult<Option<WorkItem>> {
        around!(
            self,
            Op::ClaimWorkItemWithLease {
                worker_id,
                queue_types,
                lease_secs
            },
            self.inner
                .claim_work_item_with_lease(worker_id, queue_types, lease_secs)
        )
    }

    async fn claim_work_item_filtered(
        &self,
        worker_id: &str,
        queue_types: &[&str],
        lease_secs: Option<u64>,
        filter: &ClaimFilter,
    ) -> BackendResult<Option<WorkItem>> {
        around!(
            self,
            Op::ClaimWorkItemFiltered {
                worker_id,
                queue_types,
                lease_secs,
                filter
            },
            self.inner
                .claim_work_item_filtered(worker_id, queue_types, lease_secs, filter)
        )
    }

    async fn leased_items(
        &self,
        queue_types: &[&str],
        filter: &ClaimFilter,
    ) -> BackendResult<Vec<LeasedItem>> {
        around!(
            self,
            Op::LeasedItems {
                queue_types,
                filter
            },
            self.inner.leased_items(queue_types, filter)
        )
    }

    async fn mark_gate(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        mark: GateMark,
    ) -> BackendResult<bool> {
        around!(
            self,
            Op::MarkGate {
                item_id,
                lease_fence,
                mark
            },
            self.inner.mark_gate(item_id, lease_fence, mark)
        )
    }

    async fn swap_gate(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        from: GateMark,
        to: GateMark,
    ) -> BackendResult<bool> {
        around!(
            self,
            Op::SwapGate {
                item_id,
                lease_fence,
                from,
                to
            },
            self.inner.swap_gate(item_id, lease_fence, from, to)
        )
    }

    async fn release_lease(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        from: GateMark,
        lease_secs: u64,
    ) -> BackendResult<bool> {
        around!(
            self,
            Op::ReleaseLease {
                item_id,
                lease_fence,
                from,
                lease_secs
            },
            self.inner
                .release_lease(item_id, lease_fence, from, lease_secs)
        )
    }

    async fn has_claimable(
        &self,
        queue_types: &[&str],
        filter: &ClaimFilter,
    ) -> BackendResult<bool> {
        around!(
            self,
            Op::HasClaimable {
                queue_types,
                filter
            },
            self.inner.has_claimable(queue_types, filter)
        )
    }

    async fn claim_for_execution(
        &self,
        worker_id: &str,
        queue_types: &[&str],
        lease_secs: Option<u64>,
        filter: &ClaimFilter,
    ) -> BackendResult<ExecutionClaim> {
        around!(
            self,
            Op::ClaimForExecution {
                worker_id,
                queue_types,
                lease_secs,
                filter
            },
            self.inner
                .claim_for_execution(worker_id, queue_types, lease_secs, filter)
        )
    }

    async fn heartbeat_lease(
        &self,
        item_id: WorkItemId,
        worker_id: &str,
        lease_fence: i64,
    ) -> BackendResult<LeaseRenewal> {
        around!(
            self,
            Op::HeartbeatLease {
                item_id,
                worker_id,
                lease_fence
            },
            self.inner.heartbeat_lease(item_id, worker_id, lease_fence)
        )
    }

    async fn count_node_completions(
        &self,
        execution_id: &ExecutionId,
        node_id: &str,
    ) -> BackendResult<u64> {
        around!(
            self,
            Op::CountNodeCompletions {
                execution_id,
                node_id
            },
            self.inner.count_node_completions(execution_id, node_id)
        )
    }

    async fn get_work_item(&self, item_id: WorkItemId) -> BackendResult<Option<WorkItem>> {
        around!(
            self,
            Op::GetWorkItem(item_id),
            self.inner.get_work_item(item_id)
        )
    }

    async fn complete_work_item(&self, item_id: WorkItemId) -> BackendResult<()> {
        around!(
            self,
            Op::CompleteWorkItem(item_id),
            self.inner.complete_work_item(item_id)
        )
    }

    async fn complete_work_item_fenced(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
    ) -> BackendResult<bool> {
        around!(
            self,
            Op::CompleteWorkItemFenced {
                item_id,
                lease_fence
            },
            self.inner.complete_work_item_fenced(item_id, lease_fence)
        )
    }

    async fn fail_work_item(&self, item_id: WorkItemId, error: &str) -> BackendResult<()> {
        around!(
            self,
            Op::FailWorkItem { item_id, error },
            self.inner.fail_work_item(item_id, error)
        )
    }

    async fn fail_work_item_fenced(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        error: &str,
    ) -> BackendResult<Option<FailOutcome>> {
        around!(
            self,
            Op::FailWorkItemFenced {
                item_id,
                lease_fence,
                error
            },
            self.inner
                .fail_work_item_fenced(item_id, lease_fence, error)
        )
    }

    async fn commit_turn(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        terminal_event: Event,
        write_snapshot: bool,
    ) -> BackendResult<EventSequence> {
        around!(
            self,
            Op::CommitTurn {
                item_id,
                lease_fence,
                event: &terminal_event,
                write_snapshot
            },
            self.inner
                .commit_turn(item_id, lease_fence, terminal_event.clone(), write_snapshot)
        )
    }

    async fn commit_turn_unless_terminal(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        terminal_event: Event,
        write_snapshot: bool,
    ) -> BackendResult<TurnCommit> {
        around!(
            self,
            Op::CommitTurnUnlessTerminal {
                item_id,
                lease_fence,
                event: &terminal_event,
                write_snapshot
            },
            self.inner.commit_turn_unless_terminal(
                item_id,
                lease_fence,
                terminal_event.clone(),
                write_snapshot
            )
        )
    }

    async fn withdraw_work_item_fenced(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
    ) -> BackendResult<bool> {
        around!(
            self,
            Op::WithdrawWorkItemFenced {
                item_id,
                lease_fence
            },
            self.inner.withdraw_work_item_fenced(item_id, lease_fence)
        )
    }

    async fn park_work_item(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        retry_after: &str,
        next_attempt: u32,
    ) -> BackendResult<bool> {
        around!(
            self,
            Op::ParkWorkItem {
                item_id,
                lease_fence,
                retry_after,
                next_attempt
            },
            self.inner
                .park_work_item(item_id, lease_fence, retry_after, next_attempt)
        )
    }

    async fn park_for_execution(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        attempt: u32,
    ) -> BackendResult<bool> {
        around!(
            self,
            Op::ParkForExecution {
                item_id,
                lease_fence,
                attempt
            },
            self.inner.park_for_execution(item_id, lease_fence, attempt)
        )
    }

    async fn finalize_rollover_fenced(
        &self,
        execution_id: &ExecutionId,
        work_item_id: WorkItemId,
        lease_fence: i64,
    ) -> BackendResult<bool> {
        around!(
            self,
            Op::FinalizeRolloverFenced {
                execution_id,
                item_id: work_item_id,
                lease_fence
            },
            self.inner
                .finalize_rollover_fenced(execution_id, work_item_id, lease_fence)
        )
    }

    async fn reclaim_expired_leases(&self) -> BackendResult<ReclaimResult> {
        around!(
            self,
            Op::ReclaimExpiredLeases,
            self.inner.reclaim_expired_leases()
        )
    }

    async fn move_to_dead_letter(
        &self,
        item_id: WorkItemId,
        last_error: &str,
    ) -> BackendResult<()> {
        around!(
            self,
            Op::MoveToDeadLetter {
                item_id,
                last_error
            },
            self.inner.move_to_dead_letter(item_id, last_error)
        )
    }

    // ── API tokens ───────────────────────────────────────────────────────

    async fn create_token(&self, name: &str, role: &str) -> BackendResult<(String, ApiToken)> {
        around!(
            self,
            Op::CreateToken { name, role },
            self.inner.create_token(name, role)
        )
    }

    async fn validate_token(&self, token: &str) -> BackendResult<Option<ApiToken>> {
        around!(self, Op::ValidateToken, self.inner.validate_token(token))
    }

    // ── Projector ────────────────────────────────────────────────────────

    async fn apply_approval_projection(
        &self,
        row: ApprovalProjectionRow,
        projection_name: &str,
        new_checkpoint: i64,
    ) -> BackendResult<()> {
        around!(
            self,
            Op::ApplyApprovalProjection {
                row: &row,
                projection_name,
                new_checkpoint
            },
            self.inner
                .apply_approval_projection(row.clone(), projection_name, new_checkpoint)
        )
    }

    async fn apply_approval_projection_batch(
        &self,
        rows: Vec<ApprovalProjectionRow>,
        projection_name: &str,
        execution_id: &ExecutionId,
        new_checkpoint: i64,
    ) -> BackendResult<()> {
        around!(
            self,
            Op::ApplyApprovalProjectionBatch {
                rows: &rows,
                projection_name,
                execution_id,
                new_checkpoint
            },
            self.inner.apply_approval_projection_batch(
                rows.clone(),
                projection_name,
                execution_id,
                new_checkpoint
            )
        )
    }

    async fn get_approval_projection(
        &self,
        execution_id: &ExecutionId,
    ) -> BackendResult<Vec<ApprovalProjectionRow>> {
        around!(
            self,
            Op::GetApprovalProjection(execution_id),
            self.inner.get_approval_projection(execution_id)
        )
    }

    async fn get_projector_checkpoint(
        &self,
        projection_name: &str,
        execution_id: &ExecutionId,
    ) -> BackendResult<i64> {
        around!(
            self,
            Op::GetProjectorCheckpoint {
                projection_name,
                execution_id
            },
            self.inner
                .get_projector_checkpoint(projection_name, execution_id)
        )
    }

    async fn set_projector_checkpoint(
        &self,
        projection_name: &str,
        execution_id: &ExecutionId,
        new_checkpoint: i64,
    ) -> BackendResult<()> {
        around!(
            self,
            Op::SetProjectorCheckpoint {
                projection_name,
                execution_id,
                new_checkpoint
            },
            self.inner
                .set_projector_checkpoint(projection_name, execution_id, new_checkpoint)
        )
    }

    // ── Tenants ──────────────────────────────────────────────────────────

    async fn create_tenant(&self, tenant: Tenant) -> BackendResult<()> {
        around!(
            self,
            Op::CreateTenant(&tenant),
            self.inner.create_tenant(tenant.clone())
        )
    }

    async fn get_tenant(&self, id: &TenantId) -> BackendResult<Option<Tenant>> {
        around!(self, Op::GetTenant(id), self.inner.get_tenant(id))
    }

    async fn list_tenants(&self) -> BackendResult<Vec<Tenant>> {
        around!(self, Op::ListTenants, self.inner.list_tenants())
    }

    async fn update_tenant(&self, tenant: Tenant) -> BackendResult<()> {
        around!(
            self,
            Op::UpdateTenant(&tenant),
            self.inner.update_tenant(tenant.clone())
        )
    }

    // ── Storage lane ─────────────────────────────────────────────────────

    async fn commit_scheduler_pass(&self, pass: SchedulerPass) -> BackendResult<PassCommit> {
        around!(
            self,
            Op::CommitSchedulerPass(&pass.execution_id),
            self.inner.commit_scheduler_pass(pass.clone())
        )
    }

    async fn reclaim_expired_leases_recorded(&self) -> BackendResult<ReclaimResult> {
        around!(
            self,
            Op::ReclaimExpiredLeasesRecorded,
            self.inner.reclaim_expired_leases_recorded()
        )
    }

    async fn fail_work_item_fenced_recorded(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        error: &str,
    ) -> BackendResult<Option<FailOutcome>> {
        around!(
            self,
            Op::FailWorkItemFencedRecorded {
                item_id,
                lease_fence,
                error
            },
            self.inner
                .fail_work_item_fenced_recorded(item_id, lease_fence, error)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamjet_state::InMemoryBackend;
    use std::sync::Mutex;

    /// Fails every append, and records what it saw before and after.
    #[derive(Default)]
    struct Recorder {
        before: Mutex<Vec<String>>,
        after: Mutex<Vec<String>>,
    }

    fn name(op: &Op<'_>) -> String {
        format!("{op:?}")
            .split([' ', '(', '{'])
            .next()
            .unwrap_or_default()
            .to_string()
    }

    #[async_trait]
    impl Hooks for Recorder {
        async fn before(&self, op: &Op<'_>) -> Option<StateBackendError> {
            self.before.lock().unwrap().push(name(op));
            matches!(op, Op::AppendEvent(_))
                .then(|| StateBackendError::Database("refused by the test".into()))
        }

        async fn after(&self, op: &Op<'_>) {
            self.after.lock().unwrap().push(name(op));
        }
    }

    /// A refused call never reaches the inner backend and gets no `after`;
    /// a call that goes through gets both, in order.
    #[tokio::test]
    async fn a_refused_call_writes_nothing_and_a_passed_call_runs_both_hooks() {
        let inner: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
        let hooks = Arc::new(Recorder::default());
        let intercept = Intercept::new(inner.clone(), hooks.clone());
        let execution_id = ExecutionId::new();

        let refused = intercept
            .append_event(Event::new(
                execution_id.clone(),
                1,
                EventKind::NodeScheduled {
                    node_id: "a".into(),
                    queue_type: "general".into(),
                },
            ))
            .await;
        assert!(
            matches!(&refused, Err(StateBackendError::Database(m)) if m == "refused by the test"),
            "{refused:?}"
        );
        assert!(inner.get_events(&execution_id).await.unwrap().is_empty());

        intercept.get_events(&execution_id).await.unwrap();
        assert_eq!(
            *hooks.before.lock().unwrap(),
            ["AppendEvent", "GetEvents"],
            "before runs for every call"
        );
        assert_eq!(
            *hooks.after.lock().unwrap(),
            ["GetEvents"],
            "after runs only for a call that returned Ok"
        );
    }
}
