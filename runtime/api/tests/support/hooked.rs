//! A delegating [`StateBackend`] that calls a hook after each write a test
//! wants to interleave with: the latest point another component (a scheduler
//! tick, an expiry sweep) can land between two writes of one operation.
#![allow(dead_code)]

use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::{StateBackend, WorkItem, WorkflowDefinition};
use jamjet_state::event::EventKind;
use jamjet_state::Event;
use serde_json::Value;
use std::sync::Arc;

/// A write the wrapper reports to its hook, after it landed.
#[derive(Debug, Clone)]
pub enum Write {
    CreateExecution(ExecutionId),
    AppendEvent(ExecutionId, Box<EventKind>),
    EnqueueWorkItem(ExecutionId),
    FinishExecution(ExecutionId),
    StartExecution(ExecutionId),
    /// A full read of an execution's log (after it returned).
    GetEvents(ExecutionId),
    AppendEventIf(ExecutionId),
    /// A conditional append is about to run (reported BEFORE it): where a
    /// lock wait or a stall delays the append past the caller's own checks.
    BeforeAppendEventIf(ExecutionId),
}

#[async_trait::async_trait]
pub trait AfterWrite: Send + Sync {
    async fn after(&self, write: Write);
}

pub struct HookedBackend {
    pub inner: Arc<dyn StateBackend>,
    pub hook: Arc<dyn AfterWrite>,
}

impl HookedBackend {
    pub fn new(inner: Arc<dyn StateBackend>, hook: Arc<dyn AfterWrite>) -> Self {
        Self { inner, hook }
    }
}

#[async_trait::async_trait]
impl StateBackend for HookedBackend {
    async fn get_execution(
        &self,
        id: &ExecutionId,
    ) -> jamjet_state::backend::BackendResult<Option<WorkflowExecution>> {
        self.inner.get_execution(id).await
    }

    async fn get_events(
        &self,
        execution_id: &ExecutionId,
    ) -> jamjet_state::backend::BackendResult<Vec<Event>> {
        let events = self.inner.get_events(execution_id).await?;
        self.hook
            .after(Write::GetEvents(execution_id.clone()))
            .await;
        Ok(events)
    }

    async fn store_workflow(
        &self,
        def: WorkflowDefinition,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.store_workflow(def).await
    }

    async fn get_workflow(
        &self,
        workflow_id: &str,
        version: &str,
    ) -> jamjet_state::backend::BackendResult<Option<WorkflowDefinition>> {
        self.inner.get_workflow(workflow_id, version).await
    }

    async fn create_execution(
        &self,
        execution: WorkflowExecution,
    ) -> jamjet_state::backend::BackendResult<()> {
        let execution_id = execution.execution_id.clone();
        self.inner.create_execution(execution).await?;
        self.hook.after(Write::CreateExecution(execution_id)).await;
        Ok(())
    }

    async fn finish_execution(
        &self,
        event: jamjet_state::Event,
        status: WorkflowStatus,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::ExecutionFinish> {
        let execution_id = event.execution_id.clone();
        let out = self.inner.finish_execution(event, status).await?;
        self.hook.after(Write::FinishExecution(execution_id)).await;
        Ok(out)
    }

    async fn update_execution_status(
        &self,
        id: &ExecutionId,
        status: WorkflowStatus,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.update_execution_status(id, status).await
    }

    async fn update_execution_current_state(
        &self,
        id: &ExecutionId,
        current_state: &Value,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner
            .update_execution_current_state(id, current_state)
            .await
    }

    async fn patch_append_array(
        &self,
        execution_id: &ExecutionId,
        key: &str,
        value: Value,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner
            .patch_append_array(execution_id, key, value)
            .await
    }

    async fn list_executions(
        &self,
        status: Option<WorkflowStatus>,
        limit: u32,
        offset: u32,
    ) -> jamjet_state::backend::BackendResult<Vec<WorkflowExecution>> {
        self.inner.list_executions(status, limit, offset).await
    }

    async fn append_event(
        &self,
        event: Event,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::EventSequence> {
        let execution_id = event.execution_id.clone();
        let kind = Box::new(event.kind.clone());
        let seq = self.inner.append_event(event).await?;
        self.hook
            .after(Write::AppendEvent(execution_id, kind))
            .await;
        Ok(seq)
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
        execution: WorkflowExecution,
        seed_snapshot: jamjet_state::Snapshot,
        started_event: EventKind,
        scheduled_event: EventKind,
        work_item: WorkItem,
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

    async fn start_execution_atomic(
        &self,
        execution: WorkflowExecution,
        started_event: EventKind,
        scheduled_event: EventKind,
        work_item: WorkItem,
    ) -> jamjet_state::backend::BackendResult<()> {
        let execution_id = execution.execution_id.clone();
        self.inner
            .start_execution_atomic(execution, started_event, scheduled_event, work_item)
            .await?;
        self.hook.after(Write::StartExecution(execution_id)).await;
        Ok(())
    }

    async fn append_event_if(
        &self,
        event: jamjet_state::Event,
        condition: &jamjet_state::backend::EventCondition,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::ConditionalAppend> {
        let execution_id = event.execution_id.clone();
        self.hook
            .after(Write::BeforeAppendEventIf(execution_id.clone()))
            .await;
        let out = self.inner.append_event_if(event, condition).await?;
        self.hook.after(Write::AppendEventIf(execution_id)).await;
        Ok(out)
    }

    async fn execution_tenant(
        &self,
        id: &ExecutionId,
    ) -> jamjet_state::backend::BackendResult<Option<jamjet_state::TenantId>> {
        self.inner.execution_tenant(id).await
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
    ) -> jamjet_state::backend::BackendResult<Option<Value>> {
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
        item: WorkItem,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::backend::WorkItemId> {
        let execution_id = item.execution_id.clone();
        let id = self.inner.enqueue_work_item(item).await?;
        self.hook.after(Write::EnqueueWorkItem(execution_id)).await;
        Ok(id)
    }

    async fn claim_work_item(
        &self,
        worker_id: &str,
        queue_types: &[&str],
    ) -> jamjet_state::backend::BackendResult<Option<WorkItem>> {
        self.inner.claim_work_item(worker_id, queue_types).await
    }

    async fn claim_work_item_with_lease(
        &self,
        worker_id: &str,
        queue_types: &[&str],
        lease_secs: Option<u64>,
    ) -> jamjet_state::backend::BackendResult<Option<jamjet_state::backend::WorkItem>> {
        self.inner
            .claim_work_item_with_lease(worker_id, queue_types, lease_secs)
            .await
    }

    async fn heartbeat_lease(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
        worker_id: &str,
        lease_fence: i64,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::LeaseRenewal> {
        self.inner
            .heartbeat_lease(item_id, worker_id, lease_fence)
            .await
    }

    async fn commit_turn_unless_terminal(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
        lease_fence: i64,
        terminal_event: Event,
        write_snapshot: bool,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::TurnCommit> {
        self.inner
            .commit_turn_unless_terminal(item_id, lease_fence, terminal_event, write_snapshot)
            .await
    }

    async fn withdraw_work_item_fenced(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
        lease_fence: i64,
    ) -> jamjet_state::backend::BackendResult<bool> {
        self.inner
            .withdraw_work_item_fenced(item_id, lease_fence)
            .await
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
    ) -> jamjet_state::backend::BackendResult<Option<WorkItem>> {
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

    async fn create_tenant(
        &self,
        tenant: jamjet_state::Tenant,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.create_tenant(tenant).await
    }

    async fn get_tenant(
        &self,
        id: &jamjet_state::TenantId,
    ) -> jamjet_state::backend::BackendResult<Option<jamjet_state::Tenant>> {
        self.inner.get_tenant(id).await
    }

    async fn list_tenants(
        &self,
    ) -> jamjet_state::backend::BackendResult<Vec<jamjet_state::Tenant>> {
        self.inner.list_tenants().await
    }

    async fn update_tenant(
        &self,
        tenant: jamjet_state::Tenant,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.update_tenant(tenant).await
    }
}
