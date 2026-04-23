//! In-memory state backend — no persistence, no external dependencies.
//!
//! Suitable for sandbox deployments (Glama), integration testing, and
//! quick prototyping where durability is not needed.

use crate::backend::{
    ApiToken, BackendResult, ReclaimResult, StateBackend, StateBackendError, WorkItem, WorkItemId,
    WorkflowDefinition,
};
use crate::event::{Event, EventSequence};
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
        }
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

    async fn latest_snapshot(&self, execution_id: &ExecutionId) -> BackendResult<Option<Snapshot>> {
        Ok(self
            .snapshots
            .get(execution_id)
            .and_then(|r| r.value().last().cloned()))
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
        for mut entry in self.work_items.iter_mut() {
            let item = entry.value_mut();
            if item.worker_id.is_none() && queue_types.contains(&item.queue_type.as_str()) {
                item.worker_id = Some(worker_id.to_string());
                item.lease_expires_at = Some(Utc::now() + chrono::Duration::seconds(30));
                return Ok(Some(item.clone()));
            }
        }
        Ok(None)
    }

    async fn renew_lease(&self, item_id: WorkItemId, worker_id: &str) -> BackendResult<()> {
        match self.work_items.get_mut(&item_id) {
            Some(mut entry) => {
                if entry.worker_id.as_deref() == Some(worker_id) {
                    entry.lease_expires_at = Some(Utc::now() + chrono::Duration::seconds(30));
                    Ok(())
                } else {
                    Err(StateBackendError::NotFound(format!(
                        "lease not held by {worker_id}"
                    )))
                }
            }
            None => Err(StateBackendError::NotFound(item_id.to_string())),
        }
    }

    async fn complete_work_item(&self, item_id: WorkItemId) -> BackendResult<()> {
        self.work_items.remove(&item_id);
        Ok(())
    }

    async fn fail_work_item(&self, item_id: WorkItemId, error: &str) -> BackendResult<()> {
        match self.work_items.get_mut(&item_id) {
            Some(mut entry) => {
                entry.attempt += 1;
                entry.worker_id = None;
                entry.lease_expires_at = None;
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
