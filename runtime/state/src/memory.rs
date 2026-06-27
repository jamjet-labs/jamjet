//! In-memory state backend — no persistence, no external dependencies.
//!
//! Suitable for sandbox deployments (Glama), integration testing, and
//! quick prototyping where durability is not needed.

use crate::backend::{
    ApiToken, BackendResult, ReclaimResult, StateBackend, StateBackendError, WorkItem, WorkItemId,
    WorkflowDefinition,
};
use crate::event::{Event, EventKind, EventSequence};
use crate::snapshot::Snapshot;
use crate::tenant::{Tenant, TenantId};
use async_trait::async_trait;
use chrono::Utc;
use dashmap::DashMap;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use std::sync::atomic::{AtomicI64, Ordering};
use uuid::Uuid;

pub struct InMemoryBackend {
    /// (workflow_id, version) -> definition
    workflows: DashMap<(String, String), WorkflowDefinition>,
    executions: DashMap<ExecutionId, WorkflowExecution>,
    /// execution_id -> ordered list of events
    events: DashMap<ExecutionId, Vec<Event>>,
    /// execution_id -> ordered list of snapshots
    snapshots: DashMap<ExecutionId, Vec<Snapshot>>,
    /// Work queue: item_id -> item
    work_items: DashMap<WorkItemId, WorkItem>,
    /// Dead-letter queue
    dead_letter: DashMap<WorkItemId, WorkItem>,
    /// token plaintext -> ApiToken
    tokens: DashMap<String, ApiToken>,
    tenants: DashMap<TenantId, Tenant>,
    /// Global event sequence counter (monotonic across all executions).
    next_sequence: AtomicI64,
    /// Per-work-item lease epoch (bumped on each claim/reclaim/fail).
    lease_epochs: DashMap<WorkItemId, i64>,
    /// Store failover generation (settable to simulate promotion).
    store_term: AtomicI64,
    /// Idempotency cache: idempotency_key -> result_json.
    /// Mirrors the `tool_effects` table in the SQLite backends.
    tool_effects: DashMap<String, serde_json::Value>,
    /// Projected approval read-model. Key = (execution_id as String, node_id).
    proj_approvals: DashMap<(String, String), crate::backend::ApprovalProjectionRow>,
    /// Projector checkpoints. Key = (projection_name, execution_id as String).
    projector_checkpoints: DashMap<(String, String), i64>,
}

impl InMemoryBackend {
    pub fn new() -> Self {
        Self {
            workflows: DashMap::new(),
            executions: DashMap::new(),
            events: DashMap::new(),
            snapshots: DashMap::new(),
            work_items: DashMap::new(),
            dead_letter: DashMap::new(),
            tokens: DashMap::new(),
            tenants: DashMap::new(),
            next_sequence: AtomicI64::new(1),
            lease_epochs: DashMap::new(),
            store_term: AtomicI64::new(0),
            tool_effects: DashMap::new(),
            proj_approvals: DashMap::new(),
            projector_checkpoints: DashMap::new(),
        }
    }

    /// Simulate a store failover by bumping the in-memory store term.
    pub fn bump_store_term(&self) -> i64 {
        self.store_term.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Raise the in-memory store term to at least `term`, monotonically. Mirrors
    /// `SqliteBackend::set_store_term_at_least` (the production failover-generation seam).
    pub fn set_store_term_at_least(&self, term: i64) -> i64 {
        self.store_term.fetch_max(term, Ordering::SeqCst).max(term)
    }

    /// Insert a key directly into the idempotency cache.
    /// Only available on this in-memory (dev/test) backend; SQLite backends
    /// record effects exclusively via `commit_turn`.
    pub fn seed_tool_effect_for_test(&self, key: String, value: serde_json::Value) {
        self.tool_effects.insert(key, value);
    }
}

impl Default for InMemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StateBackend for InMemoryBackend {
    // ── Workflow definitions ─────────────────────────────────────────

    async fn store_workflow(&self, def: WorkflowDefinition) -> BackendResult<()> {
        self.workflows
            .insert((def.workflow_id.clone(), def.version.clone()), def);
        Ok(())
    }

    async fn get_workflow(
        &self,
        workflow_id: &str,
        version: &str,
    ) -> BackendResult<Option<WorkflowDefinition>> {
        Ok(self
            .workflows
            .get(&(workflow_id.to_string(), version.to_string()))
            .map(|r| r.value().clone()))
    }

    // ── Workflow executions ──────────────────────────────────────────

    async fn create_execution(&self, execution: WorkflowExecution) -> BackendResult<()> {
        self.executions
            .insert(execution.execution_id.clone(), execution);
        Ok(())
    }

    async fn get_execution(&self, id: &ExecutionId) -> BackendResult<Option<WorkflowExecution>> {
        Ok(self.executions.get(id).map(|r| r.value().clone()))
    }

    async fn update_execution_status(
        &self,
        id: &ExecutionId,
        status: WorkflowStatus,
    ) -> BackendResult<()> {
        match self.executions.get_mut(id) {
            Some(mut entry) => {
                entry.status = status;
                entry.updated_at = Utc::now();
                Ok(())
            }
            None => Err(StateBackendError::NotFound(id.to_string())),
        }
    }

    async fn update_execution_current_state(
        &self,
        id: &ExecutionId,
        current_state: &serde_json::Value,
    ) -> BackendResult<()> {
        match self.executions.get_mut(id) {
            Some(mut entry) => {
                entry.current_state = current_state.clone();
                entry.updated_at = Utc::now();
                Ok(())
            }
            None => Err(StateBackendError::NotFound(id.to_string())),
        }
    }

    async fn patch_append_array(
        &self,
        execution_id: &ExecutionId,
        key: &str,
        value: serde_json::Value,
    ) -> BackendResult<()> {
        match self.executions.get_mut(execution_id) {
            Some(mut entry) => {
                let state = &mut entry.current_state;
                if let Some(obj) = state.as_object_mut() {
                    let arr = obj
                        .entry(key.to_string())
                        .or_insert_with(|| serde_json::Value::Array(vec![]));
                    if let Some(a) = arr.as_array_mut() {
                        a.push(value);
                    }
                }
                entry.updated_at = Utc::now();
                Ok(())
            }
            None => Err(StateBackendError::NotFound(execution_id.to_string())),
        }
    }

    async fn list_executions(
        &self,
        status: Option<WorkflowStatus>,
        limit: u32,
        offset: u32,
    ) -> BackendResult<Vec<WorkflowExecution>> {
        let mut results: Vec<WorkflowExecution> = self
            .executions
            .iter()
            .filter(|r| {
                if let Some(ref s) = status {
                    &r.value().status == s
                } else {
                    true
                }
            })
            .map(|r| r.value().clone())
            .collect();
        results.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        Ok(results
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect())
    }

    // ── Event log ────────────────────────────────────────────────────

    async fn append_event(&self, mut event: Event) -> BackendResult<EventSequence> {
        let seq = self.next_sequence.fetch_add(1, Ordering::SeqCst);
        event.sequence = seq;
        self.events
            .entry(event.execution_id.clone())
            .or_default()
            .push(event);
        Ok(seq)
    }

    async fn get_events(&self, execution_id: &ExecutionId) -> BackendResult<Vec<Event>> {
        Ok(self
            .events
            .get(execution_id)
            .map(|r| r.value().clone())
            .unwrap_or_default())
    }

    async fn get_events_since(
        &self,
        execution_id: &ExecutionId,
        since_sequence: EventSequence,
    ) -> BackendResult<Vec<Event>> {
        Ok(self
            .events
            .get(execution_id)
            .map(|r| {
                r.value()
                    .iter()
                    .filter(|e| e.sequence > since_sequence)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn latest_sequence(&self, execution_id: &ExecutionId) -> BackendResult<EventSequence> {
        Ok(self
            .events
            .get(execution_id)
            .and_then(|r| r.value().last().map(|e| e.sequence))
            .unwrap_or(0))
    }

    // ── Snapshots ────────────────────────────────────────────────────

    async fn write_snapshot(&self, snapshot: Snapshot) -> BackendResult<()> {
        self.snapshots
            .entry(snapshot.execution_id.clone())
            .or_default()
            .push(snapshot);
        Ok(())
    }

    async fn create_segment_atomic(
        &self,
        execution: WorkflowExecution,
        seed_snapshot: Snapshot,
        started_event: EventKind,
        scheduled_event: EventKind,
        work_item: WorkItem,
    ) -> BackendResult<()> {
        // In-memory has no ACID crash-durability, but we replicate the write
        // order of the SQLite impl so behavior is consistent in tests.
        let exec_id = execution.execution_id.clone();

        // 1. Execution row.
        self.executions.insert(exec_id.clone(), execution);

        // 2. Seed snapshot.
        self.snapshots
            .entry(exec_id.clone())
            .or_default()
            .push(seed_snapshot);

        // 3. WorkflowStarted — replicates append_event's fetch_add for sequence.
        let seq1 = self.next_sequence.fetch_add(1, Ordering::SeqCst);
        let ev1 = Event {
            id: Uuid::new_v4(),
            execution_id: exec_id.clone(),
            sequence: seq1,
            kind: started_event,
            created_at: Utc::now(),
        };
        self.events.entry(exec_id.clone()).or_default().push(ev1);

        // 4. NodeScheduled.
        let seq2 = self.next_sequence.fetch_add(1, Ordering::SeqCst);
        let ev2 = Event {
            id: Uuid::new_v4(),
            execution_id: exec_id.clone(),
            sequence: seq2,
            kind: scheduled_event,
            created_at: Utc::now(),
        };
        self.events.entry(exec_id.clone()).or_default().push(ev2);

        // 5. Start-node work item.
        self.work_items.insert(work_item.id, work_item);

        Ok(())
    }

    async fn latest_snapshot(&self, execution_id: &ExecutionId) -> BackendResult<Option<Snapshot>> {
        Ok(self
            .snapshots
            .get(execution_id)
            .and_then(|r| r.value().iter().max_by_key(|s| s.at_sequence).cloned()))
    }

    // ── Idempotency cache ─────────────────────────────────────────────────

    async fn get_tool_effect(&self, key: &str) -> BackendResult<Option<serde_json::Value>> {
        Ok(self.tool_effects.get(key).map(|r| r.value().clone()))
    }

    // ── Work queue ───────────────────────────────────────────────────

    async fn enqueue_work_item(&self, item: WorkItem) -> BackendResult<WorkItemId> {
        let id = item.id;
        self.work_items.insert(id, item);
        Ok(id)
    }

    async fn claim_work_item(
        &self,
        worker_id: &str,
        queue_types: &[&str],
    ) -> BackendResult<Option<WorkItem>> {
        let term = self.store_term.load(Ordering::SeqCst);
        for mut entry in self.work_items.iter_mut() {
            let item = entry.value_mut();
            if item.worker_id.is_none() && queue_types.contains(&item.queue_type.as_str()) {
                let new_epoch = self
                    .lease_epochs
                    .get(&item.id)
                    .map(|e| *e.value())
                    .unwrap_or(0)
                    + 1;
                self.lease_epochs.insert(item.id, new_epoch);
                let fence = term * 4_294_967_296_i64 + new_epoch;
                item.worker_id = Some(worker_id.to_string());
                item.lease_expires_at = Some(Utc::now() + chrono::Duration::seconds(30));
                item.lease_fence = fence;
                return Ok(Some(item.clone()));
            }
        }
        Ok(None)
    }

    async fn renew_lease(
        &self,
        item_id: WorkItemId,
        worker_id: &str,
        lease_fence: i64,
    ) -> BackendResult<()> {
        match self.work_items.get_mut(&item_id) {
            Some(mut entry) => {
                if entry.worker_id.as_deref() != Some(worker_id) {
                    return Err(StateBackendError::FenceLost(format!(
                        "lease not held by {worker_id}"
                    )));
                }
                if entry.lease_fence != lease_fence {
                    return Err(StateBackendError::FenceLost(item_id.to_string()));
                }
                entry.lease_expires_at = Some(Utc::now() + chrono::Duration::seconds(30));
                Ok(())
            }
            None => Err(StateBackendError::NotFound(item_id.to_string())),
        }
    }

    async fn complete_work_item(&self, item_id: WorkItemId) -> BackendResult<()> {
        self.work_items.remove(&item_id);
        Ok(())
    }

    async fn commit_turn(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        terminal_event: Event,
        write_snapshot: bool,
    ) -> BackendResult<EventSequence> {
        // Validate terminal event kind BEFORE mutating state. A miswired
        // caller (non-terminal event) fails loud instead of silently settling.
        match &terminal_event.kind {
            EventKind::NodeCompleted { .. } | EventKind::NodeFailed { .. } => {}
            _ => {
                return Err(StateBackendError::Database(
                    "commit_turn requires a terminal event (NodeCompleted/NodeFailed)".into(),
                ))
            }
        }
        // Fenced settle: only the current holder (matching fence) may settle.
        // Note: the fence check and remove are two DashMap operations rather than
        // one atomic step. This is acceptable for the in-memory dev/test backend;
        // the SQLite backends are the production path where atomicity is guaranteed
        // by BEGIN IMMEDIATE.
        match self.work_items.get(&item_id) {
            Some(entry) if entry.lease_fence == lease_fence => {}
            _ => return Err(StateBackendError::FenceLost(item_id.to_string())),
        }
        self.work_items.remove(&item_id);
        let seq = self.next_sequence.fetch_add(1, Ordering::SeqCst);
        let eid = terminal_event.execution_id.clone();

        // Extract idempotency info BEFORE moving terminal_event into the event vec.
        // INSERT OR IGNORE semantics: use `entry(...).or_insert(...)` to keep the
        // first recorded result if the key is already present.
        let idem_effect: Option<(String, serde_json::Value)> = if let EventKind::NodeCompleted {
            ref output,
            ref state_patch,
            duration_ms,
            ref gen_ai_system,
            ref gen_ai_model,
            input_tokens,
            output_tokens,
            ref finish_reason,
            idempotency_key: Some(ref key),
            ..
        } = terminal_event.kind
        {
            let result_json = serde_json::json!({
                "output": output,
                "state_patch": state_patch,
                "duration_ms": duration_ms,
                "gen_ai_system": gen_ai_system,
                "gen_ai_model": gen_ai_model,
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "finish_reason": finish_reason,
            });
            Some((key.clone(), result_json))
        } else {
            None
        };

        let mut ev = terminal_event;
        ev.sequence = seq;
        self.events.entry(eid.clone()).or_default().push(ev);

        // Record the idempotency effect if one was extracted above.
        if let Some((key, result_json)) = idem_effect {
            self.tool_effects.entry(key).or_insert(result_json);
        }

        if write_snapshot {
            // Mirror the SQLite in-tx pattern: read latest snapshot (or seed from
            // initial_input), fold all events since it (including the one just
            // appended), then write a new snapshot.
            let (base, base_sequence) = if let Some(snap) = self
                .snapshots
                .get(&eid)
                .and_then(|r| r.value().iter().max_by_key(|s| s.at_sequence).cloned())
            {
                let at_seq = snap.at_sequence;
                (
                    crate::materializer::MaterializedState {
                        current_state: snap.state,
                        status: snap.status,
                        completed_nodes: snap.completed_nodes,
                        active_nodes: snap.active_nodes,
                        last_sequence: snap.last_sequence,
                    },
                    at_seq,
                )
            } else {
                let initial_input = self
                    .executions
                    .get(&eid)
                    .map(|e| e.value().initial_input.clone())
                    .unwrap_or(serde_json::json!({}));
                (
                    crate::materializer::MaterializedState {
                        current_state: initial_input,
                        status: jamjet_core::workflow::WorkflowStatus::Pending,
                        completed_nodes: std::collections::HashMap::new(),
                        active_nodes: std::collections::HashSet::new(),
                        last_sequence: 0,
                    },
                    0,
                )
            };

            let tail_events: Vec<crate::event::Event> = self
                .events
                .get(&eid)
                .map(|r| {
                    r.value()
                        .iter()
                        .filter(|e| e.sequence > base_sequence)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();

            let mat = crate::materializer::apply_events_seeded(base, &tail_events);
            let snap = Snapshot::from_materialized(eid.clone(), &mat);
            self.snapshots.entry(eid).or_default().push(snap);
        }
        Ok(seq)
    }

    async fn fail_work_item(&self, item_id: WorkItemId, error: &str) -> BackendResult<()> {
        match self.work_items.get_mut(&item_id) {
            Some(mut entry) => {
                entry.attempt += 1;
                entry.worker_id = None;
                entry.lease_expires_at = None;
                // Bump epoch so any re-claim after a forced fail mints a
                // strictly greater fence than the failed worker's stale token.
                let next = self
                    .lease_epochs
                    .get(&item_id)
                    .map(|e| *e.value())
                    .unwrap_or(0)
                    + 1;
                self.lease_epochs.insert(item_id, next);
                if let Some(obj) = entry.payload.as_object_mut() {
                    obj.insert(
                        "last_error".into(),
                        serde_json::Value::String(error.to_string()),
                    );
                }
                Ok(())
            }
            None => Err(StateBackendError::NotFound(item_id.to_string())),
        }
    }

    async fn park_work_item(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        retry_after: &str,
        next_attempt: u32,
    ) -> BackendResult<bool> {
        // Suppress unused-variable warning; retry_after is only meaningful in
        // SQL backends (the claim query filters `retry_after <= now`). The
        // in-memory backend does not enforce the not-before window — it is a
        // dev/test backend. The WorkItem struct has no retry_after field.
        let _ = retry_after;
        match self.work_items.get_mut(&item_id) {
            Some(mut entry) => {
                // Fence guard: a stale worker (mismatched lease_fence) is a no-op.
                if entry.lease_fence != lease_fence {
                    return Ok(false);
                }
                // Reset to pending state, mirror the SQL SET columns.
                entry.attempt = next_attempt;
                entry.worker_id = None;
                entry.lease_expires_at = None;
                // Bump epoch so the next claim mints a strictly-greater fence.
                let next_epoch = self
                    .lease_epochs
                    .get(&item_id)
                    .map(|e| *e.value())
                    .unwrap_or(0)
                    + 1;
                self.lease_epochs.insert(item_id, next_epoch);
                // Clear the fence; next claim will mint a fresh one via term*2^32+epoch.
                entry.lease_fence = 0;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn finalize_rollover_fenced(
        &self,
        execution_id: &ExecutionId,
        work_item_id: WorkItemId,
        lease_fence: i64,
    ) -> BackendResult<bool> {
        // Fence check: only the current holder may finalize.
        match self.work_items.get(&work_item_id) {
            Some(entry) if entry.lease_fence == lease_fence => {}
            _ => return Ok(false),
        }
        // Fence matched — settle the work item.
        self.work_items.remove(&work_item_id);
        // Idempotent terminal update on the execution.
        if let Some(mut entry) = self.executions.get_mut(execution_id) {
            if !entry.status.is_terminal() {
                entry.status = WorkflowStatus::LimitExceeded;
                entry.updated_at = Utc::now();
            }
        }
        Ok(true)
    }

    async fn reclaim_expired_leases(&self) -> BackendResult<ReclaimResult> {
        let now = Utc::now();
        let mut result = ReclaimResult::default();
        let mut to_dead_letter = vec![];

        for mut entry in self.work_items.iter_mut() {
            let item = entry.value_mut();
            if let Some(expires) = item.lease_expires_at {
                if expires < now && item.worker_id.is_some() {
                    item.attempt += 1;
                    if item.attempt < item.max_attempts {
                        item.worker_id = None;
                        item.lease_expires_at = None;
                        // Bump epoch so a re-claim mints a strictly greater fence.
                        let next = self
                            .lease_epochs
                            .get(&item.id)
                            .map(|e| *e.value())
                            .unwrap_or(0)
                            + 1;
                        self.lease_epochs.insert(item.id, next);
                        result.retryable.push(item.clone());
                    } else {
                        to_dead_letter.push(item.clone());
                    }
                }
            }
        }

        for item in to_dead_letter {
            self.work_items.remove(&item.id);
            result.exhausted.push(item.clone());
            self.dead_letter.insert(item.id, item);
        }

        Ok(result)
    }

    async fn move_to_dead_letter(
        &self,
        item_id: WorkItemId,
        _last_error: &str,
    ) -> BackendResult<()> {
        if let Some((_, item)) = self.work_items.remove(&item_id) {
            self.dead_letter.insert(item_id, item);
        }
        Ok(())
    }

    // ── API tokens ───────────────────────────────────────────────────

    async fn create_token(&self, name: &str, role: &str) -> BackendResult<(String, ApiToken)> {
        let plaintext = format!("jj_{}", Uuid::new_v4().to_string().replace('-', ""));
        let token = ApiToken {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            role: role.to_string(),
            created_at: Utc::now(),
            expires_at: None,
            tenant_id: crate::tenant::DEFAULT_TENANT.to_string(),
        };
        self.tokens.insert(plaintext.clone(), token.clone());
        Ok((plaintext, token))
    }

    async fn validate_token(&self, token: &str) -> BackendResult<Option<ApiToken>> {
        Ok(self.tokens.get(token).map(|r| r.value().clone()))
    }

    // ── Async read-model projector ────────────────────────────────────

    async fn apply_approval_projection(
        &self,
        row: crate::backend::ApprovalProjectionRow,
        projection_name: &str,
        new_checkpoint: i64,
    ) -> BackendResult<()> {
        // In-memory: no ACID atomicity, but both writes happen in the same
        // synchronous step so there is no observable gap in tests.
        let key = (row.execution_id.0.to_string(), row.node_id.clone());
        self.proj_approvals.insert(key, row.clone());
        let cp_key = (projection_name.to_string(), row.execution_id.0.to_string());
        self.projector_checkpoints.insert(cp_key, new_checkpoint);
        Ok(())
    }

    async fn apply_approval_projection_batch(
        &self,
        rows: Vec<crate::backend::ApprovalProjectionRow>,
        projection_name: &str,
        execution_id: &ExecutionId,
        new_checkpoint: i64,
    ) -> BackendResult<()> {
        // In-memory: DashMap operations are individually atomic; we apply all
        // rows then the checkpoint in a tight synchronous sequence with no
        // await between them — sufficient for single-threaded test use.
        for row in rows {
            let key = (row.execution_id.0.to_string(), row.node_id.clone());
            self.proj_approvals.insert(key, row);
        }
        let cp_key = (projection_name.to_string(), execution_id.0.to_string());
        self.projector_checkpoints.insert(cp_key, new_checkpoint);
        Ok(())
    }

    async fn get_approval_projection(
        &self,
        execution_id: &ExecutionId,
    ) -> BackendResult<Vec<crate::backend::ApprovalProjectionRow>> {
        let exec_str = execution_id.0.to_string();
        let mut rows: Vec<crate::backend::ApprovalProjectionRow> = self
            .proj_approvals
            .iter()
            .filter(|e| e.key().0 == exec_str)
            .map(|e| e.value().clone())
            .collect();
        rows.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        Ok(rows)
    }

    async fn get_projector_checkpoint(
        &self,
        projection_name: &str,
        execution_id: &ExecutionId,
    ) -> BackendResult<i64> {
        let key = (projection_name.to_string(), execution_id.0.to_string());
        Ok(self
            .projector_checkpoints
            .get(&key)
            .map(|v| *v.value())
            .unwrap_or(0))
    }

    async fn set_projector_checkpoint(
        &self,
        projection_name: &str,
        execution_id: &ExecutionId,
        new_checkpoint: i64,
    ) -> BackendResult<()> {
        let key = (projection_name.to_string(), execution_id.0.to_string());
        self.projector_checkpoints.insert(key, new_checkpoint);
        Ok(())
    }

    // ── Tenants ──────────────────────────────────────────────────────

    async fn create_tenant(&self, tenant: Tenant) -> BackendResult<()> {
        self.tenants.insert(tenant.id.clone(), tenant);
        Ok(())
    }

    async fn get_tenant(&self, id: &TenantId) -> BackendResult<Option<Tenant>> {
        Ok(self.tenants.get(id).map(|r| r.value().clone()))
    }

    async fn list_tenants(&self) -> BackendResult<Vec<Tenant>> {
        Ok(self.tenants.iter().map(|r| r.value().clone()).collect())
    }

    async fn update_tenant(&self, tenant: Tenant) -> BackendResult<()> {
        self.tenants.insert(tenant.id.clone(), tenant);
        Ok(())
    }
}
