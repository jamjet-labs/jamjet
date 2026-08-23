//! Shared test doubles for the worker crate.
//!
//! `InMemoryBackend` never errors, so without a double every fail-closed arm in
//! the worker and the dispatch guard is unreachable from a test — and an
//! implementation that returned the WRONG outcome on a backend failure would
//! pass the whole suite. Delegating every other method to a real backend is what
//! makes "nothing was written" an observation rather than an artefact of a stub
//! that cannot write at all.
//!
//! Lives here rather than inside one module's `mod tests` so both
//! `dispatch_guard` and `worker` can drive the same failure injection.

use jamjet_core::workflow::ExecutionId;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::tenant::Tenant;
use jamjet_state::{Event, InMemoryBackend};

/// An `InMemoryBackend` whose `get_events` fails, and only `get_events`.
///
/// `InMemoryBackend` never errors, so without this every `Unavailable` arm in
/// the guard is unreachable from a test and an implementation that returned
/// `Blocked` on a backend failure would pass the whole suite — while writing
/// a `PolicyViolation` saying policy denied something the engine merely could
/// not read. Delegating every other method to a real backend is what makes
/// "nothing was written" an observation rather than an artefact of a stub
/// that cannot write at all: the append path here works perfectly.
pub(crate) struct FailingGetEvents {
    inner: InMemoryBackend,
    fail_events: bool,
    fail_tenant: bool,
}

impl FailingGetEvents {
    pub(crate) fn new() -> Self {
        Self {
            inner: InMemoryBackend::new(),
            fail_events: true,
            fail_tenant: false,
        }
    }

    /// Fail `get_tenant` instead, so the tenant layer is unreadable rather
    /// than absent — the two must not be confused.
    pub(crate) fn failing_tenant() -> Self {
        Self {
            inner: InMemoryBackend::new(),
            fail_events: false,
            fail_tenant: true,
        }
    }

    /// Read the log the guard could not, bypassing the injected failure.
    pub(crate) async fn recorded(&self, execution_id: &ExecutionId) -> Vec<Event> {
        self.inner.get_events(execution_id).await.unwrap()
    }
}

#[async_trait::async_trait]
impl StateBackend for FailingGetEvents {
    // ── The one injected failure ──────────────────────────────────────────

    async fn get_events(
        &self,
        execution_id: &ExecutionId,
    ) -> jamjet_state::backend::BackendResult<Vec<Event>> {
        if !self.fail_events {
            return self.inner.get_events(execution_id).await;
        }
        Err(jamjet_state::backend::StateBackendError::Database(
            "injected: event log unreadable".into(),
        ))
    }

    // ── Everything else is the real backend ───────────────────────────────

    async fn store_workflow(
        &self,
        def: jamjet_state::backend::WorkflowDefinition,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.store_workflow(def).await
    }

    async fn get_workflow(
        &self,
        workflow_id: &str,
        version: &str,
    ) -> jamjet_state::backend::BackendResult<Option<jamjet_state::backend::WorkflowDefinition>>
    {
        self.inner.get_workflow(workflow_id, version).await
    }

    async fn create_execution(
        &self,
        execution: jamjet_core::workflow::WorkflowExecution,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.create_execution(execution).await
    }

    async fn get_execution(
        &self,
        id: &ExecutionId,
    ) -> jamjet_state::backend::BackendResult<Option<jamjet_core::workflow::WorkflowExecution>>
    {
        self.inner.get_execution(id).await
    }

    async fn update_execution_status(
        &self,
        id: &ExecutionId,
        status: jamjet_core::workflow::WorkflowStatus,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.update_execution_status(id, status).await
    }

    async fn update_execution_current_state(
        &self,
        id: &ExecutionId,
        current_state: &serde_json::Value,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner
            .update_execution_current_state(id, current_state)
            .await
    }

    async fn patch_append_array(
        &self,
        execution_id: &ExecutionId,
        key: &str,
        value: serde_json::Value,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner
            .patch_append_array(execution_id, key, value)
            .await
    }

    async fn list_executions(
        &self,
        status: Option<jamjet_core::workflow::WorkflowStatus>,
        limit: u32,
        offset: u32,
    ) -> jamjet_state::backend::BackendResult<Vec<jamjet_core::workflow::WorkflowExecution>> {
        self.inner.list_executions(status, limit, offset).await
    }

    async fn append_event(
        &self,
        event: Event,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::EventSequence> {
        self.inner.append_event(event).await
    }

    async fn get_events_since(
        &self,
        execution_id: &ExecutionId,
        since_sequence: jamjet_state::EventSequence,
    ) -> jamjet_state::backend::BackendResult<Vec<Event>> {
        self.inner
            .get_events_since(execution_id, since_sequence)
            .await
    }

    async fn latest_sequence(
        &self,
        execution_id: &ExecutionId,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::EventSequence> {
        self.inner.latest_sequence(execution_id).await
    }

    async fn write_snapshot(
        &self,
        snapshot: jamjet_state::Snapshot,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.write_snapshot(snapshot).await
    }

    async fn latest_snapshot(
        &self,
        execution_id: &ExecutionId,
    ) -> jamjet_state::backend::BackendResult<Option<jamjet_state::Snapshot>> {
        self.inner.latest_snapshot(execution_id).await
    }

    async fn create_segment_atomic(
        &self,
        execution: jamjet_core::workflow::WorkflowExecution,
        seed_snapshot: jamjet_state::Snapshot,
        started_event: EventKind,
        scheduled_event: EventKind,
        work_item: jamjet_state::backend::WorkItem,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner
            .create_segment_atomic(
                execution,
                seed_snapshot,
                started_event,
                scheduled_event,
                work_item,
            )
            .await
    }

    async fn release_tool_reservation(
        &self,
        key: &str,
        owner: &str,
        lease_fence: i64,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner
            .release_tool_reservation(key, owner, lease_fence)
            .await
    }

    async fn reserve_tool_effect(
        &self,

        key: &str,

        execution_id: &ExecutionId,

        node_id: &str,

        owner: &str,

        lease_fence: i64,

        ttl: std::time::Duration,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::backend::ReserveOutcome> {
        self.inner
            .reserve_tool_effect(key, execution_id, node_id, owner, lease_fence, ttl)
            .await
    }

    async fn get_tool_effect(
        &self,
        key: &str,
    ) -> jamjet_state::backend::BackendResult<Option<serde_json::Value>> {
        self.inner.get_tool_effect(key).await
    }

    async fn put_artifact(
        &self,
        bytes: &[u8],
        media_type: Option<&str>,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::ArtifactRef> {
        self.inner.put_artifact(bytes, media_type).await
    }

    async fn get_artifact(
        &self,
        hash: &str,
    ) -> jamjet_state::backend::BackendResult<Option<Vec<u8>>> {
        self.inner.get_artifact(hash).await
    }

    async fn enqueue_work_item(
        &self,
        item: jamjet_state::backend::WorkItem,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::backend::WorkItemId> {
        self.inner.enqueue_work_item(item).await
    }

    async fn claim_work_item(
        &self,
        worker_id: &str,
        queue_types: &[&str],
    ) -> jamjet_state::backend::BackendResult<Option<jamjet_state::backend::WorkItem>> {
        self.inner.claim_work_item(worker_id, queue_types).await
    }

    async fn renew_lease(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
        worker_id: &str,
        lease_fence: i64,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner
            .renew_lease(item_id, worker_id, lease_fence)
            .await
    }

    async fn get_work_item(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
    ) -> jamjet_state::backend::BackendResult<Option<jamjet_state::backend::WorkItem>> {
        self.inner.get_work_item(item_id).await
    }

    async fn complete_work_item(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.complete_work_item(item_id).await
    }

    async fn complete_work_item_fenced(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
        lease_fence: i64,
    ) -> jamjet_state::backend::BackendResult<bool> {
        self.inner
            .complete_work_item_fenced(item_id, lease_fence)
            .await
    }

    async fn fail_work_item(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
        error: &str,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.fail_work_item(item_id, error).await
    }

    async fn fail_work_item_fenced(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
        lease_fence: i64,
        error: &str,
    ) -> jamjet_state::backend::BackendResult<Option<jamjet_state::backend::FailOutcome>> {
        self.inner
            .fail_work_item_fenced(item_id, lease_fence, error)
            .await
    }

    async fn commit_turn(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
        lease_fence: i64,
        terminal_event: Event,
        write_snapshot: bool,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::EventSequence> {
        self.inner
            .commit_turn(item_id, lease_fence, terminal_event, write_snapshot)
            .await
    }

    async fn park_work_item(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
        lease_fence: i64,
        retry_after: &str,
        next_attempt: u32,
    ) -> jamjet_state::backend::BackendResult<bool> {
        self.inner
            .park_work_item(item_id, lease_fence, retry_after, next_attempt)
            .await
    }

    async fn finalize_rollover_fenced(
        &self,
        execution_id: &ExecutionId,
        work_item_id: jamjet_state::backend::WorkItemId,
        lease_fence: i64,
    ) -> jamjet_state::backend::BackendResult<bool> {
        self.inner
            .finalize_rollover_fenced(execution_id, work_item_id, lease_fence)
            .await
    }

    async fn reclaim_expired_leases(
        &self,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::backend::ReclaimResult> {
        self.inner.reclaim_expired_leases().await
    }

    async fn move_to_dead_letter(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
        last_error: &str,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.move_to_dead_letter(item_id, last_error).await
    }

    async fn create_token(
        &self,
        name: &str,
        role: &str,
    ) -> jamjet_state::backend::BackendResult<(String, jamjet_state::backend::ApiToken)> {
        self.inner.create_token(name, role).await
    }

    async fn validate_token(
        &self,
        token: &str,
    ) -> jamjet_state::backend::BackendResult<Option<jamjet_state::backend::ApiToken>> {
        self.inner.validate_token(token).await
    }

    async fn apply_approval_projection(
        &self,
        row: jamjet_state::ApprovalProjectionRow,
        projection_name: &str,
        new_checkpoint: i64,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner
            .apply_approval_projection(row, projection_name, new_checkpoint)
            .await
    }

    async fn apply_approval_projection_batch(
        &self,
        rows: Vec<jamjet_state::ApprovalProjectionRow>,
        projection_name: &str,
        execution_id: &ExecutionId,
        new_checkpoint: i64,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner
            .apply_approval_projection_batch(rows, projection_name, execution_id, new_checkpoint)
            .await
    }

    async fn get_approval_projection(
        &self,
        execution_id: &ExecutionId,
    ) -> jamjet_state::backend::BackendResult<Vec<jamjet_state::ApprovalProjectionRow>> {
        self.inner.get_approval_projection(execution_id).await
    }

    async fn get_projector_checkpoint(
        &self,
        projection_name: &str,
        execution_id: &ExecutionId,
    ) -> jamjet_state::backend::BackendResult<i64> {
        self.inner
            .get_projector_checkpoint(projection_name, execution_id)
            .await
    }

    async fn set_projector_checkpoint(
        &self,
        projection_name: &str,
        execution_id: &ExecutionId,
        new_checkpoint: i64,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner
            .set_projector_checkpoint(projection_name, execution_id, new_checkpoint)
            .await
    }

    async fn create_tenant(&self, tenant: Tenant) -> jamjet_state::backend::BackendResult<()> {
        self.inner.create_tenant(tenant).await
    }

    async fn get_tenant(
        &self,
        id: &jamjet_state::TenantId,
    ) -> jamjet_state::backend::BackendResult<Option<Tenant>> {
        if self.fail_tenant {
            return Err(jamjet_state::backend::StateBackendError::Database(
                "injected: tenant record unreadable".into(),
            ));
        }
        self.inner.get_tenant(id).await
    }

    async fn list_tenants(&self) -> jamjet_state::backend::BackendResult<Vec<Tenant>> {
        self.inner.list_tenants().await
    }

    async fn update_tenant(&self, tenant: Tenant) -> jamjet_state::backend::BackendResult<()> {
        self.inner.update_tenant(tenant).await
    }
}
