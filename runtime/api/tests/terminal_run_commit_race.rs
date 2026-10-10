//! PR #140 review, finding I3: `/complete` checked the run's status and then
//! committed in a separate step, so a cancel landing between the two still
//! appended `node_completed` and a state patch after `workflow_cancelled`.
//! The refusal now lives inside the fenced commit's transaction.
//!
//! The cancel itself used to be two writes: `WorkflowCancelled` appended,
//! then the status set. A fenced commit landing between them still saw
//! `Running` and recorded `NodeCompleted` after `WorkflowCancelled` (PR #140
//! re-review, I-A). The cancel is now one write that does both.
mod support;

use axum::http::StatusCode;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::{StateBackend, WorkItem, WorkflowDefinition};
use jamjet_state::event::EventKind;
use jamjet_state::Event;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use support::{backends, make_state, post, python_fn_start_ir, register_and_start};

/// A real backend that, once armed, cancels the execution the moment anyone
/// reads it, and hands the reader the status from BEFORE the cancel. That is
/// a cancel landing right after the route's read: the read is stale by the
/// time anything acts on it.
///
/// Once `commit_between` is set, it also runs a fenced commit for that item
/// right after each write a cancel makes, before the next one: the latest
/// point a `/complete` can land while a cancel is in flight.
struct CancelAfterRead {
    inner: Arc<dyn StateBackend>,
    armed: AtomicBool,
    commit_between: Mutex<Option<(uuid::Uuid, i64)>>,
    between_outcome: Mutex<Option<jamjet_state::backend::BackendResult<jamjet_state::TurnCommit>>>,
}

impl CancelAfterRead {
    fn new(inner: Arc<dyn StateBackend>) -> Self {
        Self {
            inner,
            armed: AtomicBool::new(false),
            commit_between: Mutex::new(None),
            between_outcome: Mutex::new(None),
        }
    }

    async fn commit_if_armed(&self, execution_id: &ExecutionId) {
        let armed = self.commit_between.lock().unwrap().take();
        if let Some((item_id, fence)) = armed {
            let outcome = self
                .inner
                .commit_turn_unless_terminal(
                    item_id,
                    fence,
                    Event::new(execution_id.clone(), 0, late_completion()),
                    true,
                )
                .await;
            *self.between_outcome.lock().unwrap() = Some(outcome);
        }
    }
}

fn late_completion() -> EventKind {
    EventKind::NodeCompleted {
        node_id: "start".into(),
        output: json!({"gpu_result": "late"}),
        state_patch: json!({"gpu_result": "late"}),
        duration_ms: 1,
        gen_ai_system: None,
        gen_ai_model: None,
        input_tokens: None,
        output_tokens: None,
        finish_reason: None,
        cost_usd: None,
        provenance: None,
        idempotency_key: None,
    }
}

#[async_trait::async_trait]
impl StateBackend for CancelAfterRead {
    async fn get_execution(
        &self,
        id: &ExecutionId,
    ) -> jamjet_state::backend::BackendResult<Option<WorkflowExecution>> {
        let read = self.inner.get_execution(id).await?;
        if self.armed.swap(false, Ordering::SeqCst) {
            self.inner
                .update_execution_status(id, WorkflowStatus::Cancelled)
                .await?;
        }
        Ok(read)
    }

    async fn get_events(
        &self,
        execution_id: &ExecutionId,
    ) -> jamjet_state::backend::BackendResult<Vec<Event>> {
        self.inner.get_events(execution_id).await
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
        self.inner.create_execution(execution).await
    }

    async fn finish_execution(
        &self,
        event: jamjet_state::Event,
        status: WorkflowStatus,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::ExecutionFinish> {
        let execution_id = event.execution_id.clone();
        let finished = self.inner.finish_execution(event, status).await?;
        self.commit_if_armed(&execution_id).await;
        Ok(finished)
    }

    async fn update_execution_status(
        &self,
        id: &ExecutionId,
        status: WorkflowStatus,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.update_execution_status(id, status).await?;
        self.commit_if_armed(id).await;
        Ok(())
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
        let cancel = matches!(event.kind, EventKind::WorkflowCancelled { .. });
        let seq = self.inner.append_event(event).await?;
        if cancel {
            self.commit_if_armed(&execution_id).await;
        }
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
        self.inner
            .start_execution_atomic(execution, started_event, scheduled_event, work_item)
            .await
    }

    async fn append_event_if(
        &self,
        event: jamjet_state::Event,
        condition: &jamjet_state::backend::EventCondition,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::ConditionalAppend> {
        self.inner.append_event_if(event, condition).await
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
        self.inner.enqueue_work_item(item).await
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

#[tokio::test]
async fn a_cancel_landing_between_the_read_and_the_commit_records_nothing() {
    for tb in backends().await {
        let name = tb.name();
        let racy = Arc::new(CancelAfterRead::new(tb.dyn_backend()));
        let backend: Arc<dyn StateBackend> = racy.clone();
        let state = make_state(backend.clone());
        register_and_start(&state, python_fn_start_ir("wf-cancel-race"), json!({})).await;
        let item = backend
            .claim_work_item("py-worker", &["python_tool"])
            .await
            .unwrap()
            .expect("start node item");

        racy.armed.store(true, Ordering::SeqCst);
        let (st, body) = post(
            &state,
            &format!("/work-items/{}/complete", item.id),
            json!({
                "output": { "gpu_result": "late" },
                "state_patch": { "gpu_result": "late" },
                "duration_ms": 1,
                "lease_fence": item.lease_fence
            }),
        )
        .await;

        let exec = backend
            .get_execution(&item.execution_id)
            .await
            .unwrap()
            .expect("execution");
        assert_eq!(
            exec.status,
            WorkflowStatus::Cancelled,
            "{name}: the cancel landed"
        );
        let events = backend.get_events(&item.execution_id).await.unwrap();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.kind, EventKind::NodeCompleted { .. })),
            "{name}: a completion was recorded into a run cancelled before the commit \
             ({st} {body})"
        );
        assert_eq!(st, StatusCode::CONFLICT, "{name}: {body}");
    }
}

/// A tenant-scoped view over its own database, for a tenant other than the
/// default so the tenant clauses are exercised.
async fn open_tenant_scoped() -> Arc<dyn StateBackend> {
    let mut path = std::env::temp_dir();
    path.push(format!("jamjet_route_{}.db", uuid::Uuid::new_v4().simple()));
    let url = format!("sqlite://{}", path.display());
    let raw = jamjet_state::SqliteBackend::open(&url)
        .await
        .expect("open test db");
    let now = chrono::Utc::now();
    raw.for_tenant(jamjet_state::TenantId::default())
        .create_tenant(jamjet_state::Tenant {
            id: jamjet_state::TenantId::from("acme"),
            name: "Acme".into(),
            status: jamjet_state::TenantStatus::Active,
            policy: None,
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("register tenant");
    Arc::new(raw.for_tenant(jamjet_state::TenantId::from("acme")))
}

/// I-A (PR #140 re-review): the cancel route appended `WorkflowCancelled`
/// and then set the status in a second write. A fenced commit landing between
/// the two read `Running` and recorded `NodeCompleted` after the cancel. The
/// wrapper runs that commit after every write the real cancel route makes, so
/// whatever the route's ordering, the commit sees the cancel or none of it.
#[tokio::test]
async fn a_commit_landing_inside_the_cancel_route_records_nothing() {
    let mut matrix: Vec<(&'static str, Arc<dyn StateBackend>)> = backends()
        .await
        .into_iter()
        .map(|tb| (tb.name(), tb.dyn_backend()))
        .collect();
    matrix.push(("tenant_scoped", open_tenant_scoped().await));

    // Every backend is checked before failing, so a regression names them all.
    let mut failures = Vec::new();
    for (name, inner) in matrix {
        let racy = Arc::new(CancelAfterRead::new(inner));
        let backend: Arc<dyn StateBackend> = racy.clone();
        let state = make_state(backend.clone());
        let exec_id =
            register_and_start(&state, python_fn_start_ir("wf-cancel-writes"), json!({})).await;
        let item = backend
            .claim_work_item("py-worker", &["python_tool"])
            .await
            .unwrap()
            .expect("start node item");

        *racy.commit_between.lock().unwrap() = Some((item.id, item.lease_fence));
        let (st, body) = post(&state, &format!("/executions/{exec_id}/cancel"), json!({})).await;
        assert_eq!(st, StatusCode::OK, "{name}: cancel failed: {body}");

        let outcome = racy
            .between_outcome
            .lock()
            .unwrap()
            .take()
            .expect("the commit ran inside the cancel");
        let events = backend.get_events(&item.execution_id).await.unwrap();
        let kinds: Vec<String> = events
            .iter()
            .map(|e| serde_json::to_value(&e.kind).unwrap()["type"].to_string())
            .collect();
        let cancelled_at = events
            .iter()
            .position(|e| matches!(e.kind, EventKind::WorkflowCancelled { .. }))
            .expect("the cancel was recorded");
        if events[cancelled_at..]
            .iter()
            .any(|e| matches!(e.kind, EventKind::NodeCompleted { .. }))
        {
            failures.push(format!(
                "{name}: a completion landed after WorkflowCancelled: {kinds:?}"
            ));
        }
        if !matches!(
            outcome,
            Ok(jamjet_state::TurnCommit::ExecutionTerminal(
                WorkflowStatus::Cancelled
            ))
        ) {
            failures.push(format!(
                "{name}: the completion was not refused: {outcome:?}"
            ));
        }
        let exec = backend
            .get_execution(&item.execution_id)
            .await
            .unwrap()
            .expect("execution");
        assert_eq!(exec.status, WorkflowStatus::Cancelled, "{name}");
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
