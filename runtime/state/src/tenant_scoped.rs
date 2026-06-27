//! Tenant-scoped SQLite backend — wraps a shared `SqlitePool` and transparently
//! filters all queries by `tenant_id`.
//!
//! Workers serve all tenants: they claim any pending work item, then read the
//! `tenant_id` from the claimed item and use a scoped backend for the rest of
//! that item's execution.

use crate::backend::{
    ApiToken, BackendResult, ReclaimResult, StateBackend, StateBackendError, WorkItem, WorkItemId,
    WorkflowDefinition,
};
use crate::event::{Event, EventKind, EventSequence};
use crate::snapshot::Snapshot;
use crate::sqlite::{map_db_err, parse_datetime, parse_execution_id};
use crate::tenant::{Tenant, TenantId, TenantLimits, TenantStatus, DEFAULT_TENANT};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use sqlx::{Row, SqlitePool};
use tracing::instrument;
use uuid::Uuid;

/// A tenant-scoped view over a shared `SqlitePool`.
///
/// Every read/write operation is filtered by `tenant_id`, ensuring
/// complete data isolation between tenants.
pub struct TenantScopedSqliteBackend {
    pub(crate) pool: SqlitePool,
    pub(crate) tenant_id: TenantId,
}

impl TenantScopedSqliteBackend {
    pub fn new(pool: SqlitePool, tenant_id: TenantId) -> Self {
        Self { pool, tenant_id }
    }

    pub fn tenant_id(&self) -> &TenantId {
        &self.tenant_id
    }

    /// The store's current failover generation (same table as SqliteBackend).
    async fn current_term(&self, tx: &mut sqlx::SqliteConnection) -> BackendResult<i64> {
        let row = sqlx::query("SELECT term FROM store_identity WHERE id = 1")
            .fetch_one(&mut *tx)
            .await
            .map_err(map_db_err)?;
        row.try_get::<i64, _>("term").map_err(map_db_err)
    }
}

// ── Helpers (re-use sqlite.rs parsers) ────────────────────────────────────────

fn status_to_str(s: &WorkflowStatus) -> &'static str {
    match s {
        WorkflowStatus::Pending => "pending",
        WorkflowStatus::Running => "running",
        WorkflowStatus::Paused => "paused",
        WorkflowStatus::Completed => "completed",
        WorkflowStatus::Failed => "failed",
        WorkflowStatus::Cancelled => "cancelled",
        WorkflowStatus::LimitExceeded => "limit_exceeded",
    }
}

fn str_to_status(s: &str) -> BackendResult<WorkflowStatus> {
    match s {
        "pending" => Ok(WorkflowStatus::Pending),
        "running" => Ok(WorkflowStatus::Running),
        "paused" => Ok(WorkflowStatus::Paused),
        "completed" => Ok(WorkflowStatus::Completed),
        "failed" => Ok(WorkflowStatus::Failed),
        "cancelled" => Ok(WorkflowStatus::Cancelled),
        "limit_exceeded" => Ok(WorkflowStatus::LimitExceeded),
        other => Err(StateBackendError::Database(format!(
            "unknown status: {other}"
        ))),
    }
}

fn execution_id_str(id: &ExecutionId) -> String {
    id.0.to_string()
}

fn row_to_execution(row: &sqlx::sqlite::SqliteRow) -> BackendResult<WorkflowExecution> {
    let execution_id =
        parse_execution_id(row.try_get::<&str, _>("execution_id").map_err(map_db_err)?)?;
    let status = str_to_status(row.try_get::<&str, _>("status").map_err(map_db_err)?)?;
    let initial_input: serde_json::Value = serde_json::from_str(
        row.try_get::<&str, _>("initial_input")
            .map_err(map_db_err)?,
    )
    .map_err(StateBackendError::Serialization)?;
    let current_state: serde_json::Value = serde_json::from_str(
        row.try_get::<&str, _>("current_state")
            .map_err(map_db_err)?,
    )
    .map_err(StateBackendError::Serialization)?;
    let started_at = parse_datetime(row.try_get::<&str, _>("started_at").map_err(map_db_err)?)?;
    let updated_at = parse_datetime(row.try_get::<&str, _>("updated_at").map_err(map_db_err)?)?;
    let completed_at: Option<DateTime<Utc>> = row
        .try_get::<Option<&str>, _>("completed_at")
        .map_err(map_db_err)?
        .map(parse_datetime)
        .transpose()?;

    let parent_execution_id: Option<ExecutionId> = row
        .try_get::<Option<&str>, _>("parent_execution_id")
        .map_err(map_db_err)?
        .map(parse_execution_id)
        .transpose()?;
    let segment_number: u32 = row.try_get::<i64, _>("segment_number").unwrap_or(0).max(0) as u32;

    Ok(WorkflowExecution {
        execution_id,
        workflow_id: row
            .try_get::<String, _>("workflow_id")
            .map_err(map_db_err)?,
        workflow_version: row
            .try_get::<String, _>("workflow_version")
            .map_err(map_db_err)?,
        status,
        initial_input,
        current_state,
        started_at,
        updated_at,
        completed_at,
        session_type: None,
        parent_execution_id,
        segment_number,
    })
}

fn row_to_event(row: &sqlx::sqlite::SqliteRow) -> BackendResult<Event> {
    let id = Uuid::parse_str(row.try_get::<&str, _>("id").map_err(map_db_err)?)
        .map_err(|e| StateBackendError::Database(e.to_string()))?;
    let execution_id =
        parse_execution_id(row.try_get::<&str, _>("execution_id").map_err(map_db_err)?)?;
    let sequence: i64 = row.try_get("sequence").map_err(map_db_err)?;
    let kind: EventKind =
        serde_json::from_str(row.try_get::<&str, _>("kind_json").map_err(map_db_err)?)
            .map_err(StateBackendError::Serialization)?;
    let created_at = parse_datetime(row.try_get::<&str, _>("created_at").map_err(map_db_err)?)?;

    Ok(Event {
        id,
        execution_id,
        sequence,
        kind,
        created_at,
    })
}

fn row_to_work_item(row: &sqlx::sqlite::SqliteRow) -> BackendResult<WorkItem> {
    let id = Uuid::parse_str(row.try_get::<&str, _>("id").map_err(map_db_err)?)
        .map_err(|e| StateBackendError::Database(e.to_string()))?;
    let execution_id =
        parse_execution_id(row.try_get::<&str, _>("execution_id").map_err(map_db_err)?)?;
    let payload: serde_json::Value =
        serde_json::from_str(row.try_get::<&str, _>("payload_json").map_err(map_db_err)?)
            .map_err(StateBackendError::Serialization)?;
    let lease_expires_at: Option<DateTime<Utc>> = row
        .try_get::<Option<&str>, _>("lease_expires_at")
        .map_err(map_db_err)?
        .map(parse_datetime)
        .transpose()?;
    let created_at = parse_datetime(row.try_get::<&str, _>("created_at").map_err(map_db_err)?)?;
    let attempt: i64 = row.try_get("attempt").map_err(map_db_err)?;
    let max_attempts: i64 = row.try_get("max_attempts").unwrap_or(3);
    let tenant_id: String = row
        .try_get("tenant_id")
        .unwrap_or_else(|_| DEFAULT_TENANT.to_string());

    Ok(WorkItem {
        id,
        execution_id,
        node_id: row.try_get::<String, _>("node_id").map_err(map_db_err)?,
        queue_type: row.try_get::<String, _>("queue_type").map_err(map_db_err)?,
        payload,
        attempt: attempt as u32,
        max_attempts: max_attempts as u32,
        created_at,
        lease_expires_at,
        worker_id: row
            .try_get::<Option<String>, _>("worker_id")
            .map_err(map_db_err)?,
        lease_fence: row.try_get::<i64, _>("lease_fence").unwrap_or(0),
        tenant_id,
    })
}

// ── StateBackend impl ─────────────────────────────────────────────────────────

#[async_trait]
impl StateBackend for TenantScopedSqliteBackend {
    // ── Workflow definitions ──────────────────────────────────────────────

    #[instrument(skip(self, def), fields(tenant = %self.tenant_id, workflow_id = %def.workflow_id))]
    async fn store_workflow(&self, mut def: WorkflowDefinition) -> BackendResult<()> {
        def.tenant_id = self.tenant_id.0.clone();
        let ir_json = serde_json::to_string(&def.ir)?;
        let created_at = def.created_at.to_rfc3339();

        sqlx::query(
            r#"INSERT OR REPLACE INTO workflow_definitions (workflow_id, version, ir_json, created_at, tenant_id)
               VALUES (?, ?, ?, ?, ?)"#,
        )
        .bind(&def.workflow_id)
        .bind(&def.version)
        .bind(&ir_json)
        .bind(&created_at)
        .bind(&self.tenant_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

        Ok(())
    }

    #[instrument(skip(self), fields(tenant = %self.tenant_id, workflow_id = workflow_id))]
    async fn get_workflow(
        &self,
        workflow_id: &str,
        version: &str,
    ) -> BackendResult<Option<WorkflowDefinition>> {
        let row = sqlx::query(
            "SELECT * FROM workflow_definitions WHERE workflow_id = ? AND version = ? AND tenant_id = ?",
        )
        .bind(workflow_id)
        .bind(version)
        .bind(&self.tenant_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db_err)?;

        let Some(row) = row else { return Ok(None) };

        let ir: serde_json::Value =
            serde_json::from_str(row.try_get::<&str, _>("ir_json").map_err(map_db_err)?)
                .map_err(StateBackendError::Serialization)?;
        let created_at = parse_datetime(row.try_get::<&str, _>("created_at").map_err(map_db_err)?)?;

        Ok(Some(WorkflowDefinition {
            workflow_id: row
                .try_get::<String, _>("workflow_id")
                .map_err(map_db_err)?,
            version: row.try_get::<String, _>("version").map_err(map_db_err)?,
            ir,
            created_at,
            tenant_id: self.tenant_id.0.clone(),
        }))
    }

    // ── Executions ────────────────────────────────────────────────────────

    #[instrument(skip(self, execution), fields(tenant = %self.tenant_id, execution_id = %execution.execution_id))]
    async fn create_execution(&self, execution: WorkflowExecution) -> BackendResult<()> {
        let id = execution_id_str(&execution.execution_id);
        let status = status_to_str(&execution.status);
        let initial_input = serde_json::to_string(&execution.initial_input)?;
        let current_state = serde_json::to_string(&execution.current_state)?;
        let started_at = execution.started_at.to_rfc3339();
        let updated_at = execution.updated_at.to_rfc3339();
        let completed_at = execution.completed_at.map(|dt| dt.to_rfc3339());
        let parent_id = execution.parent_execution_id.as_ref().map(execution_id_str);
        let segment_number = execution.segment_number as i64;

        sqlx::query(
            r#"INSERT INTO workflow_executions
               (execution_id, workflow_id, workflow_version, status, initial_input, current_state,
                started_at, updated_at, completed_at, tenant_id, parent_execution_id, segment_number)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&id)
        .bind(&execution.workflow_id)
        .bind(&execution.workflow_version)
        .bind(status)
        .bind(&initial_input)
        .bind(&current_state)
        .bind(&started_at)
        .bind(&updated_at)
        .bind(completed_at.as_deref())
        .bind(&self.tenant_id.0)
        .bind(parent_id.as_deref())
        .bind(segment_number)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

        Ok(())
    }

    #[instrument(skip(self), fields(tenant = %self.tenant_id, execution_id = %id))]
    async fn get_execution(&self, id: &ExecutionId) -> BackendResult<Option<WorkflowExecution>> {
        let id_str = execution_id_str(id);
        let row = sqlx::query(
            "SELECT * FROM workflow_executions WHERE execution_id = ? AND tenant_id = ?",
        )
        .bind(&id_str)
        .bind(&self.tenant_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db_err)?;

        row.map(|r| row_to_execution(&r)).transpose()
    }

    #[instrument(skip(self), fields(tenant = %self.tenant_id, execution_id = %id))]
    async fn update_execution_status(
        &self,
        id: &ExecutionId,
        status: WorkflowStatus,
    ) -> BackendResult<()> {
        let id_str = execution_id_str(id);
        let status_str = status_to_str(&status);
        let now = Utc::now().to_rfc3339();
        let completed_at = if status.is_terminal() {
            Some(now.clone())
        } else {
            None
        };

        let rows_affected = sqlx::query(
            "UPDATE workflow_executions SET status = ?, updated_at = ?, completed_at = COALESCE(?, completed_at) WHERE execution_id = ? AND tenant_id = ?",
        )
        .bind(status_str)
        .bind(&now)
        .bind(completed_at.as_deref())
        .bind(&id_str)
        .bind(&self.tenant_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?
        .rows_affected();

        if rows_affected == 0 {
            return Err(StateBackendError::NotFound(id_str));
        }
        Ok(())
    }

    async fn update_execution_current_state(
        &self,
        id: &ExecutionId,
        current_state: &serde_json::Value,
    ) -> BackendResult<()> {
        let id_str = execution_id_str(id);
        let state_str =
            serde_json::to_string(current_state).map_err(StateBackendError::Serialization)?;
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE workflow_executions SET current_state = ?, updated_at = ? WHERE execution_id = ? AND tenant_id = ?",
        )
        .bind(&state_str)
        .bind(&now)
        .bind(&id_str)
        .bind(&self.tenant_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;
        Ok(())
    }

    async fn patch_append_array(
        &self,
        execution_id: &ExecutionId,
        key: &str,
        value: serde_json::Value,
    ) -> BackendResult<()> {
        let exec = self
            .get_execution(execution_id)
            .await?
            .ok_or_else(|| StateBackendError::NotFound(format!("execution {execution_id}")))?;
        let mut state = exec.current_state.clone();
        let arr = state
            .as_object_mut()
            .ok_or_else(|| StateBackendError::Database("state is not a JSON object".into()))?
            .entry(key)
            .or_insert_with(|| serde_json::json!([]));
        arr.as_array_mut()
            .ok_or_else(|| StateBackendError::Database(format!("{key} is not an array")))?
            .push(value);
        self.update_execution_current_state(execution_id, &state)
            .await
    }

    #[instrument(skip(self), fields(tenant = %self.tenant_id))]
    async fn list_executions(
        &self,
        status: Option<WorkflowStatus>,
        limit: u32,
        offset: u32,
    ) -> BackendResult<Vec<WorkflowExecution>> {
        let rows = match status {
            Some(s) => {
                let status_str = status_to_str(&s);
                sqlx::query(
                    "SELECT * FROM workflow_executions WHERE status = ? AND tenant_id = ? ORDER BY updated_at DESC LIMIT ? OFFSET ?",
                )
                .bind(status_str)
                .bind(&self.tenant_id.0)
                .bind(limit as i64)
                .bind(offset as i64)
                .fetch_all(&self.pool)
                .await
                .map_err(map_db_err)?
            }
            None => sqlx::query(
                "SELECT * FROM workflow_executions WHERE tenant_id = ? ORDER BY updated_at DESC LIMIT ? OFFSET ?",
            )
            .bind(&self.tenant_id.0)
            .bind(limit as i64)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(map_db_err)?,
        };

        rows.iter().map(row_to_execution).collect()
    }

    // ── Event log ─────────────────────────────────────────────────────────

    #[instrument(skip(self, event), fields(tenant = %self.tenant_id, execution_id = %event.execution_id))]
    async fn append_event(&self, event: Event) -> BackendResult<EventSequence> {
        let id = event.id.to_string();
        let execution_id = execution_id_str(&event.execution_id);
        let kind_json = serde_json::to_string(&event.kind)?;
        let created_at = event.created_at.to_rfc3339();

        // Assign the sequence atomically (see SqliteBackend::append_event). The
        // MAX is scoped to the execution only (not the tenant), so it agrees with
        // the base backend's sequence even when an execution's events carry mixed
        // tenant_id rows (scheduler/worker run on the base backend in dev).
        // BEGIN IMMEDIATE: a deferred SELECT→INSERT upgrade hits an instant
        // SQLITE_BUSY (busy handler skipped) under concurrency.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_db_err)?;
        let seq_row = sqlx::query(
            "SELECT COALESCE(MAX(sequence), 0) + 1 AS seq FROM events WHERE execution_id = ?",
        )
        .bind(&execution_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_db_err)?;
        let sequence: i64 = seq_row.try_get::<i64, _>("seq").map_err(map_db_err)?;

        sqlx::query(
            r#"INSERT INTO events (id, execution_id, sequence, kind_json, created_at, tenant_id)
               VALUES (?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&id)
        .bind(&execution_id)
        .bind(sequence)
        .bind(&kind_json)
        .bind(&created_at)
        .bind(&self.tenant_id.0)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?;
        tx.commit().await.map_err(map_db_err)?;

        Ok(sequence)
    }

    #[instrument(skip(self), fields(tenant = %self.tenant_id, execution_id = %execution_id))]
    async fn get_events(&self, execution_id: &ExecutionId) -> BackendResult<Vec<Event>> {
        let id_str = execution_id_str(execution_id);
        let rows = sqlx::query(
            "SELECT * FROM events WHERE execution_id = ? AND tenant_id = ? ORDER BY sequence ASC",
        )
        .bind(&id_str)
        .bind(&self.tenant_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(map_db_err)?;

        rows.iter().map(row_to_event).collect()
    }

    #[instrument(skip(self), fields(tenant = %self.tenant_id, execution_id = %execution_id))]
    async fn get_events_since(
        &self,
        execution_id: &ExecutionId,
        since_sequence: EventSequence,
    ) -> BackendResult<Vec<Event>> {
        let id_str = execution_id_str(execution_id);
        let rows = sqlx::query(
            "SELECT * FROM events WHERE execution_id = ? AND tenant_id = ? AND sequence > ? ORDER BY sequence ASC",
        )
        .bind(&id_str)
        .bind(&self.tenant_id.0)
        .bind(since_sequence)
        .fetch_all(&self.pool)
        .await
        .map_err(map_db_err)?;

        rows.iter().map(row_to_event).collect()
    }

    #[instrument(skip(self), fields(tenant = %self.tenant_id, execution_id = %execution_id))]
    async fn latest_sequence(&self, execution_id: &ExecutionId) -> BackendResult<EventSequence> {
        let id_str = execution_id_str(execution_id);
        let row = sqlx::query(
            "SELECT COALESCE(MAX(sequence), 0) as seq FROM events WHERE execution_id = ? AND tenant_id = ?",
        )
        .bind(&id_str)
        .bind(&self.tenant_id.0)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;

        Ok(row.try_get::<i64, _>("seq").map_err(map_db_err)?)
    }

    // ── Snapshots ─────────────────────────────────────────────────────────

    #[instrument(skip(self, snapshot), fields(tenant = %self.tenant_id, execution_id = %snapshot.execution_id))]
    async fn write_snapshot(&self, snapshot: Snapshot) -> BackendResult<()> {
        let id = snapshot.id.to_string();
        let execution_id = execution_id_str(&snapshot.execution_id);
        let state_json = serde_json::to_string(&snapshot.state)?;
        let created_at = snapshot.created_at.to_rfc3339();
        let status_str = status_to_str(&snapshot.status);
        let completed_json = serde_json::to_string(&snapshot.completed_nodes)?;
        let active_json = serde_json::to_string(&snapshot.active_nodes)?;

        sqlx::query(
            r#"INSERT OR REPLACE INTO snapshots
               (id, execution_id, at_sequence, state_json, created_at, tenant_id, status, completed_nodes_json, active_nodes_json, last_sequence)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&id)
        .bind(&execution_id)
        .bind(snapshot.at_sequence)
        .bind(&state_json)
        .bind(&created_at)
        .bind(&self.tenant_id.0)
        .bind(status_str)
        .bind(&completed_json)
        .bind(&active_json)
        .bind(snapshot.last_sequence)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

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
        let exec_id_str = execution_id_str(&execution.execution_id);
        let exec_status = status_to_str(&execution.status);
        let initial_input_json = serde_json::to_string(&execution.initial_input)?;
        let current_state_json = serde_json::to_string(&execution.current_state)?;
        let exec_started_at = execution.started_at.to_rfc3339();
        let exec_updated_at = execution.updated_at.to_rfc3339();
        let exec_completed_at = execution.completed_at.map(|dt| dt.to_rfc3339());
        let parent_id = execution.parent_execution_id.as_ref().map(execution_id_str);
        let segment_number = execution.segment_number as i64;

        let snap_id = seed_snapshot.id.to_string();
        let snap_state_json = serde_json::to_string(&seed_snapshot.state)?;
        let snap_created_at = seed_snapshot.created_at.to_rfc3339();
        let snap_status_str = status_to_str(&seed_snapshot.status);
        let snap_completed_json = serde_json::to_string(&seed_snapshot.completed_nodes)?;
        let snap_active_json = serde_json::to_string(&seed_snapshot.active_nodes)?;

        let ev1_id = Uuid::new_v4().to_string();
        let ev1_kind_json = serde_json::to_string(&started_event)?;
        let ev1_created_at = Utc::now().to_rfc3339();

        let ev2_id = Uuid::new_v4().to_string();
        let ev2_kind_json = serde_json::to_string(&scheduled_event)?;
        let ev2_created_at = Utc::now().to_rfc3339();

        let wi_id = work_item.id.to_string();
        let wi_exec_id = execution_id_str(&work_item.execution_id);
        let wi_payload_json = serde_json::to_string(&work_item.payload)?;
        let wi_created_at = work_item.created_at.to_rfc3339();

        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_db_err)?;

        // 1. Execution row (tenant-scoped).
        sqlx::query(
            r#"INSERT INTO workflow_executions
               (execution_id, workflow_id, workflow_version, status, initial_input, current_state,
                started_at, updated_at, completed_at, tenant_id, parent_execution_id, segment_number)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&exec_id_str)
        .bind(&execution.workflow_id)
        .bind(&execution.workflow_version)
        .bind(exec_status)
        .bind(&initial_input_json)
        .bind(&current_state_json)
        .bind(&exec_started_at)
        .bind(&exec_updated_at)
        .bind(exec_completed_at.as_deref())
        .bind(&self.tenant_id.0)
        .bind(parent_id.as_deref())
        .bind(segment_number)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?;

        // 2. Seed snapshot (tenant-scoped).
        sqlx::query(
            r#"INSERT OR REPLACE INTO snapshots
               (id, execution_id, at_sequence, state_json, created_at, tenant_id, status,
                completed_nodes_json, active_nodes_json, last_sequence)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&snap_id)
        .bind(&exec_id_str)
        .bind(seed_snapshot.at_sequence)
        .bind(&snap_state_json)
        .bind(&snap_created_at)
        .bind(&self.tenant_id.0)
        .bind(snap_status_str)
        .bind(&snap_completed_json)
        .bind(&snap_active_json)
        .bind(seed_snapshot.last_sequence)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?;

        // 3. WorkflowStarted at seq 1 (replicate append_event's SELECT MAX+1).
        let seq1_row = sqlx::query(
            "SELECT COALESCE(MAX(sequence), 0) + 1 AS seq FROM events WHERE execution_id = ?",
        )
        .bind(&exec_id_str)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_db_err)?;
        let seq1: i64 = seq1_row.try_get::<i64, _>("seq").map_err(map_db_err)?;

        sqlx::query(
            r#"INSERT INTO events (id, execution_id, sequence, kind_json, created_at, tenant_id)
               VALUES (?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&ev1_id)
        .bind(&exec_id_str)
        .bind(seq1)
        .bind(&ev1_kind_json)
        .bind(&ev1_created_at)
        .bind(&self.tenant_id.0)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?;

        // 4. NodeScheduled at seq 2.
        let seq2_row = sqlx::query(
            "SELECT COALESCE(MAX(sequence), 0) + 1 AS seq FROM events WHERE execution_id = ?",
        )
        .bind(&exec_id_str)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_db_err)?;
        let seq2: i64 = seq2_row.try_get::<i64, _>("seq").map_err(map_db_err)?;

        sqlx::query(
            r#"INSERT INTO events (id, execution_id, sequence, kind_json, created_at, tenant_id)
               VALUES (?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&ev2_id)
        .bind(&exec_id_str)
        .bind(seq2)
        .bind(&ev2_kind_json)
        .bind(&ev2_created_at)
        .bind(&self.tenant_id.0)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?;

        // 5. Start-node work item (tenant-scoped).
        sqlx::query(
            r#"INSERT INTO work_items
               (id, execution_id, node_id, queue_type, payload_json, attempt, max_attempts,
                status, created_at, tenant_id)
               VALUES (?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?)"#,
        )
        .bind(&wi_id)
        .bind(&wi_exec_id)
        .bind(&work_item.node_id)
        .bind(&work_item.queue_type)
        .bind(&wi_payload_json)
        .bind(work_item.attempt as i64)
        .bind(work_item.max_attempts as i64)
        .bind(&wi_created_at)
        .bind(&self.tenant_id.0)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?;

        tx.commit().await.map_err(map_db_err)?;
        Ok(())
    }

    #[instrument(skip(self), fields(tenant = %self.tenant_id, execution_id = %execution_id))]
    async fn latest_snapshot(&self, execution_id: &ExecutionId) -> BackendResult<Option<Snapshot>> {
        let id_str = execution_id_str(execution_id);
        let row = sqlx::query(
            "SELECT * FROM snapshots WHERE execution_id = ? AND tenant_id = ? ORDER BY at_sequence DESC LIMIT 1",
        )
        .bind(&id_str)
        .bind(&self.tenant_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db_err)?;

        let Some(row) = row else { return Ok(None) };

        let id = Uuid::parse_str(row.try_get::<&str, _>("id").map_err(map_db_err)?)
            .map_err(|e| StateBackendError::Database(e.to_string()))?;
        let execution_id =
            parse_execution_id(row.try_get::<&str, _>("execution_id").map_err(map_db_err)?)?;
        let at_sequence: i64 = row.try_get("at_sequence").map_err(map_db_err)?;
        let state: serde_json::Value =
            serde_json::from_str(row.try_get::<&str, _>("state_json").map_err(map_db_err)?)
                .map_err(StateBackendError::Serialization)?;
        let created_at = parse_datetime(row.try_get::<&str, _>("created_at").map_err(map_db_err)?)?;
        let status = str_to_status(row.try_get::<&str, _>("status").unwrap_or("running"))
            .unwrap_or(WorkflowStatus::Running);
        let completed_nodes: std::collections::HashMap<String, serde_json::Value> = {
            match row.try_get::<Option<&str>, _>("completed_nodes_json") {
                Err(_) => std::collections::HashMap::new(), // absent column (pre-0005 rows)
                Ok(None) => std::collections::HashMap::new(),
                Ok(Some(s)) => serde_json::from_str(s).map_err(StateBackendError::Serialization)?,
            }
        };
        let active_nodes: std::collections::HashSet<String> = {
            match row.try_get::<Option<&str>, _>("active_nodes_json") {
                Err(_) => std::collections::HashSet::new(), // absent column (pre-0005 rows)
                Ok(None) => std::collections::HashSet::new(),
                Ok(Some(s)) => serde_json::from_str(s).map_err(StateBackendError::Serialization)?,
            }
        };
        let last_sequence: i64 = row
            .try_get::<i64, _>("last_sequence")
            .unwrap_or(at_sequence);

        Ok(Some(Snapshot {
            id,
            execution_id,
            at_sequence,
            state,
            status,
            completed_nodes,
            active_nodes,
            last_sequence,
            created_at,
        }))
    }

    // ── Idempotency cache ─────────────────────────────────────────────────

    async fn get_tool_effect(&self, key: &str) -> BackendResult<Option<serde_json::Value>> {
        let row = sqlx::query(
            "SELECT result_json FROM tool_effects WHERE idempotency_key = ? AND tenant_id = ?",
        )
        .bind(key)
        .bind(&self.tenant_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db_err)?;

        let Some(row) = row else { return Ok(None) };

        let result_json: serde_json::Value =
            serde_json::from_str(row.try_get::<&str, _>("result_json").map_err(map_db_err)?)
                .map_err(StateBackendError::Serialization)?;
        Ok(Some(result_json))
    }

    // ── Work item queue ───────────────────────────────────────────────────

    #[instrument(skip(self, item), fields(tenant = %self.tenant_id, execution_id = %item.execution_id))]
    async fn enqueue_work_item(&self, mut item: WorkItem) -> BackendResult<WorkItemId> {
        item.tenant_id = self.tenant_id.0.clone();
        let id = item.id.to_string();
        let execution_id = execution_id_str(&item.execution_id);
        let payload_json = serde_json::to_string(&item.payload)?;
        let created_at = item.created_at.to_rfc3339();

        sqlx::query(
            r#"INSERT INTO work_items
               (id, execution_id, node_id, queue_type, payload_json, attempt, max_attempts, status, created_at, tenant_id)
               VALUES (?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?)"#,
        )
        .bind(&id)
        .bind(&execution_id)
        .bind(&item.node_id)
        .bind(&item.queue_type)
        .bind(&payload_json)
        .bind(item.attempt as i64)
        .bind(item.max_attempts as i64)
        .bind(&created_at)
        .bind(&self.tenant_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

        Ok(item.id)
    }

    #[instrument(skip(self), fields(tenant = %self.tenant_id, worker_id = worker_id))]
    async fn claim_work_item(
        &self,
        worker_id: &str,
        queue_types: &[&str],
    ) -> BackendResult<Option<WorkItem>> {
        if queue_types.is_empty() {
            return Ok(None);
        }

        let now = Utc::now().to_rfc3339();
        // Expire stale leases for this tenant; bump lease_epoch so a re-claim
        // mints a strictly greater fence than any zombie worker's stale token.
        sqlx::query(
            "UPDATE work_items SET status = 'pending', worker_id = NULL, lease_expires_at = NULL, lease_epoch = lease_epoch + 1 \
             WHERE status = 'claimed' AND lease_expires_at < ? AND tenant_id = ?",
        )
        .bind(&now)
        .bind(&self.tenant_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

        // BEGIN IMMEDIATE: see SqliteBackend::append_event — a deferred
        // SELECT→UPDATE upgrade hits an instant SQLITE_BUSY under concurrency.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_db_err)?;

        let placeholders = queue_types
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let query_str = format!(
            "SELECT * FROM work_items WHERE status = 'pending' AND queue_type IN ({}) \
             AND tenant_id = ? AND (retry_after IS NULL OR retry_after <= ?) ORDER BY created_at ASC LIMIT 1",
            placeholders
        );
        let mut q = sqlx::query(&query_str);
        for qt in queue_types {
            q = q.bind(*qt);
        }
        q = q.bind(&self.tenant_id.0);
        q = q.bind(&now);
        let row = q.fetch_optional(&mut *tx).await.map_err(map_db_err)?;

        let Some(row) = row else {
            tx.rollback().await.map_err(map_db_err)?;
            return Ok(None);
        };

        let item = row_to_work_item(&row)?;
        let item_id = item.id.to_string();
        let lease_expires_at = (Utc::now() + chrono::Duration::seconds(30)).to_rfc3339();
        let claimed_at = Utc::now().to_rfc3339();

        let term = self.current_term(&mut tx).await?;
        let current_epoch: i64 = row.try_get::<i64, _>("lease_epoch").unwrap_or(0);
        let new_epoch = current_epoch + 1;
        let new_fence = term * 4_294_967_296_i64 + new_epoch;

        sqlx::query(
            "UPDATE work_items SET status = 'claimed', worker_id = ?, lease_expires_at = ?, claimed_at = ?, lease_epoch = ?, lease_fence = ? WHERE id = ?",
        )
        .bind(worker_id)
        .bind(&lease_expires_at)
        .bind(&claimed_at)
        .bind(new_epoch)
        .bind(new_fence)
        .bind(&item_id)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?;

        tx.commit().await.map_err(map_db_err)?;

        let mut claimed = item;
        claimed.worker_id = Some(worker_id.to_string());
        claimed.lease_fence = new_fence;
        claimed.lease_expires_at = Some(
            DateTime::parse_from_rfc3339(&lease_expires_at)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| StateBackendError::Database(e.to_string()))?,
        );
        Ok(Some(claimed))
    }

    #[instrument(skip(self), fields(tenant = %self.tenant_id, item_id = %item_id))]
    async fn renew_lease(
        &self,
        item_id: WorkItemId,
        worker_id: &str,
        lease_fence: i64,
    ) -> BackendResult<()> {
        let lease_expires_at = (Utc::now() + chrono::Duration::seconds(30)).to_rfc3339();
        let id_str = item_id.to_string();

        let rows_affected = sqlx::query(
            "UPDATE work_items SET lease_expires_at = ? WHERE id = ? AND worker_id = ? AND status = 'claimed' AND tenant_id = ? AND lease_fence = ?",
        )
        .bind(&lease_expires_at)
        .bind(&id_str)
        .bind(worker_id)
        .bind(&self.tenant_id.0)
        .bind(lease_fence)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?
        .rows_affected();

        if rows_affected == 0 {
            return Err(StateBackendError::FenceLost(id_str));
        }
        Ok(())
    }

    #[instrument(skip(self), fields(tenant = %self.tenant_id, item_id = %item_id))]
    async fn complete_work_item(&self, item_id: WorkItemId) -> BackendResult<()> {
        let id_str = item_id.to_string();
        let completed_at = Utc::now().to_rfc3339();

        let rows_affected = sqlx::query(
            "UPDATE work_items SET status = 'completed', completed_at = ?, lease_expires_at = NULL WHERE id = ? AND tenant_id = ?",
        )
        .bind(&completed_at)
        .bind(&id_str)
        .bind(&self.tenant_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?
        .rows_affected();

        if rows_affected == 0 {
            return Err(StateBackendError::NotFound(id_str));
        }
        Ok(())
    }

    #[instrument(skip(self, error), fields(tenant = %self.tenant_id, item_id = %item_id))]
    async fn fail_work_item(&self, item_id: WorkItemId, error: &str) -> BackendResult<()> {
        let id_str = item_id.to_string();
        let _ = error;

        let rows_affected = sqlx::query(
            "UPDATE work_items SET status = 'failed', lease_expires_at = NULL, worker_id = NULL WHERE id = ? AND tenant_id = ?",
        )
        .bind(&id_str)
        .bind(&self.tenant_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?
        .rows_affected();

        if rows_affected == 0 {
            return Err(StateBackendError::NotFound(id_str));
        }
        Ok(())
    }

    #[instrument(skip(self), fields(tenant = %self.tenant_id, item_id = %item_id, fence = lease_fence))]
    async fn park_work_item(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        retry_after: &str,
        next_attempt: u32,
    ) -> BackendResult<bool> {
        let id_str = item_id.to_string();
        // Same as SqliteBackend::park_work_item but scoped to tenant_id.
        let rows_affected = sqlx::query(
            "UPDATE work_items \
             SET status = 'pending', retry_after = ?, attempt = ?, worker_id = NULL, \
                 lease_expires_at = NULL, lease_epoch = lease_epoch + 1, lease_fence = 0 \
             WHERE id = ? AND lease_fence = ? AND tenant_id = ?",
        )
        .bind(retry_after)
        .bind(next_attempt as i64)
        .bind(&id_str)
        .bind(lease_fence)
        .bind(&self.tenant_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?
        .rows_affected();
        Ok(rows_affected > 0)
    }

    #[instrument(skip(self), fields(tenant = %self.tenant_id, execution_id = %execution_id, item_id = %work_item_id, fence = lease_fence))]
    async fn finalize_rollover_fenced(
        &self,
        execution_id: &ExecutionId,
        work_item_id: WorkItemId,
        lease_fence: i64,
    ) -> BackendResult<bool> {
        let exec_id_str = execution_id_str(execution_id);
        let item_id_str = work_item_id.to_string();
        let now = Utc::now().to_rfc3339();

        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_db_err)?;

        // 1. Fence-gated settle — tenant-scoped.
        let rows = sqlx::query(
            "UPDATE work_items SET status = 'completed', completed_at = ?, lease_expires_at = NULL \
             WHERE id = ? AND lease_fence = ? AND tenant_id = ?",
        )
        .bind(&now)
        .bind(&item_id_str)
        .bind(lease_fence)
        .bind(&self.tenant_id.0)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?
        .rows_affected();

        if rows == 0 {
            tx.rollback().await.map_err(map_db_err)?;
            return Ok(false);
        }

        // 2. Idempotent terminal update — tenant-scoped.
        sqlx::query(
            "UPDATE workflow_executions \
             SET status = 'limit_exceeded', updated_at = ?, completed_at = COALESCE(completed_at, ?) \
             WHERE execution_id = ? AND tenant_id = ? \
             AND status NOT IN ('completed', 'failed', 'cancelled', 'limit_exceeded')",
        )
        .bind(&now)
        .bind(&now)
        .bind(&exec_id_str)
        .bind(&self.tenant_id.0)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?;

        tx.commit().await.map_err(map_db_err)?;
        Ok(true)
    }

    #[instrument(skip(self, terminal_event), fields(tenant = %self.tenant_id, item_id = %item_id))]
    async fn commit_turn(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        terminal_event: Event,
        write_snapshot: bool,
    ) -> BackendResult<EventSequence> {
        let id_str = item_id.to_string();
        let execution_id = execution_id_str(&terminal_event.execution_id);
        let event_id = terminal_event.id.to_string();
        let kind_json = serde_json::to_string(&terminal_event.kind)?;
        let created_at = terminal_event.created_at.to_rfc3339();
        let now = Utc::now().to_rfc3339();

        // Validate terminal event kind BEFORE opening the transaction.
        // A miswired caller (non-terminal event) fails loud instead of
        // silently settling a non-terminal event as completed.
        let (status, set_completed_at) = match &terminal_event.kind {
            EventKind::NodeCompleted { .. } => ("completed", true),
            EventKind::NodeFailed { .. } => ("failed", false),
            _ => {
                return Err(StateBackendError::Database(
                    "commit_turn requires a terminal event (NodeCompleted/NodeFailed)".into(),
                ))
            }
        };

        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_db_err)?;

        // Fenced settle with tenant isolation. Zero rows => stale fence => fail closed.
        let rows = if set_completed_at {
            sqlx::query(
                "UPDATE work_items SET status = ?, completed_at = ?, lease_expires_at = NULL WHERE id = ? AND tenant_id = ? AND lease_fence = ?",
            )
            .bind(status)
            .bind(&now)
            .bind(&id_str)
            .bind(&self.tenant_id.0)
            .bind(lease_fence)
            .execute(&mut *tx)
            .await
            .map_err(map_db_err)?
            .rows_affected()
        } else {
            sqlx::query(
                "UPDATE work_items SET status = ?, completed_at = NULL, lease_expires_at = NULL, worker_id = NULL WHERE id = ? AND tenant_id = ? AND lease_fence = ?",
            )
            .bind(status)
            .bind(&id_str)
            .bind(&self.tenant_id.0)
            .bind(lease_fence)
            .execute(&mut *tx)
            .await
            .map_err(map_db_err)?
            .rows_affected()
        };

        if rows == 0 {
            tx.rollback().await.map_err(map_db_err)?;
            return Err(StateBackendError::FenceLost(id_str));
        }

        let seq_row = sqlx::query(
            "SELECT COALESCE(MAX(sequence), 0) + 1 AS seq FROM events WHERE execution_id = ?",
        )
        .bind(&execution_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_db_err)?;
        let sequence: i64 = seq_row.try_get::<i64, _>("seq").map_err(map_db_err)?;

        sqlx::query(
            r#"INSERT INTO events (id, execution_id, sequence, kind_json, created_at, tenant_id)
               VALUES (?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&event_id)
        .bind(&execution_id)
        .bind(sequence)
        .bind(&kind_json)
        .bind(&created_at)
        .bind(&self.tenant_id.0)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?;

        // 2b) If the terminal event is NodeCompleted with an idempotency key,
        //     record the result in tool_effects in the SAME transaction (tenant-scoped).
        //     INSERT OR IGNORE — a concurrent winner already recorded it.
        if let EventKind::NodeCompleted {
            node_id: ref nc_node_id,
            ref output,
            ref state_patch,
            duration_ms,
            ref gen_ai_system,
            ref gen_ai_model,
            input_tokens,
            output_tokens,
            ref finish_reason,
            idempotency_key: Some(ref idem_key),
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
            let result_json_str = serde_json::to_string(&result_json)?;
            sqlx::query(
                r#"INSERT OR IGNORE INTO tool_effects
                   (idempotency_key, execution_id, node_id, result_json, tenant_id, recorded_at)
                   VALUES (?, ?, ?, ?, ?, ?)"#,
            )
            .bind(idem_key)
            .bind(&execution_id)
            .bind(nc_node_id.as_str())
            .bind(&result_json_str)
            .bind(&self.tenant_id.0)
            .bind(&now)
            .execute(&mut *tx)
            .await
            .map_err(map_db_err)?;
        }

        // Optionally compute and write the snapshot IN-TX from committed state (tenant-scoped).
        // BEGIN IMMEDIATE serializes writers — no concurrent write-skew.
        if write_snapshot {
            // 3a. Read the latest snapshot for this execution IN-TX.
            let snap_row = sqlx::query(
                "SELECT * FROM snapshots WHERE execution_id = ? AND tenant_id = ? ORDER BY at_sequence DESC LIMIT 1",
            )
            .bind(&execution_id)
            .bind(&self.tenant_id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_db_err)?;

            let (base, base_sequence) = if let Some(row) = snap_row {
                let at_seq: i64 = row.try_get("at_sequence").map_err(map_db_err)?;
                let current_state: serde_json::Value =
                    serde_json::from_str(row.try_get::<&str, _>("state_json").map_err(map_db_err)?)
                        .map_err(StateBackendError::Serialization)?;
                let snap_status =
                    str_to_status(row.try_get::<&str, _>("status").unwrap_or("running"))
                        .unwrap_or(WorkflowStatus::Running);
                let completed_nodes: std::collections::HashMap<String, serde_json::Value> = {
                    match row.try_get::<Option<&str>, _>("completed_nodes_json") {
                        Err(_) => std::collections::HashMap::new(),
                        Ok(None) => std::collections::HashMap::new(),
                        Ok(Some(s)) => {
                            serde_json::from_str(s).map_err(StateBackendError::Serialization)?
                        }
                    }
                };
                let active_nodes: std::collections::HashSet<String> = {
                    match row.try_get::<Option<&str>, _>("active_nodes_json") {
                        Err(_) => std::collections::HashSet::new(),
                        Ok(None) => std::collections::HashSet::new(),
                        Ok(Some(s)) => {
                            serde_json::from_str(s).map_err(StateBackendError::Serialization)?
                        }
                    }
                };
                let last_seq: i64 = row.try_get::<i64, _>("last_sequence").unwrap_or(at_seq);
                (
                    crate::materializer::MaterializedState {
                        current_state,
                        status: snap_status,
                        completed_nodes,
                        active_nodes,
                        last_sequence: last_seq,
                    },
                    at_seq,
                )
            } else {
                // No snapshot: seed from the execution's initial_input IN-TX.
                let exec_row = sqlx::query(
                    "SELECT initial_input FROM workflow_executions WHERE execution_id = ? AND tenant_id = ?",
                )
                .bind(&execution_id)
                .bind(&self.tenant_id.0)
                .fetch_one(&mut *tx)
                .await
                .map_err(map_db_err)?;
                let initial_input: serde_json::Value = serde_json::from_str(
                    exec_row
                        .try_get::<&str, _>("initial_input")
                        .map_err(map_db_err)?,
                )
                .map_err(StateBackendError::Serialization)?;
                (
                    crate::materializer::MaterializedState {
                        current_state: initial_input,
                        status: WorkflowStatus::Pending,
                        completed_nodes: std::collections::HashMap::new(),
                        active_nodes: std::collections::HashSet::new(),
                        last_sequence: 0,
                    },
                    0,
                )
            };

            // 3b. Read events since base_sequence IN-TX (includes the just-inserted terminal event).
            let event_rows = sqlx::query(
                "SELECT * FROM events WHERE execution_id = ? AND tenant_id = ? AND sequence > ? ORDER BY sequence ASC",
            )
            .bind(&execution_id)
            .bind(&self.tenant_id.0)
            .bind(base_sequence)
            .fetch_all(&mut *tx)
            .await
            .map_err(map_db_err)?;

            let tail_events: Vec<crate::event::Event> = event_rows
                .iter()
                .map(row_to_event)
                .collect::<BackendResult<Vec<_>>>()?;

            // 3c. Fold events onto the base state.
            let mat = crate::materializer::apply_events_seeded(base, &tail_events);

            // 3d. Build and INSERT the snapshot.
            let snap = Snapshot::from_materialized(terminal_event.execution_id.clone(), &mat);
            let snap_id = snap.id.to_string();
            let snap_exec_id = execution_id_str(&snap.execution_id);
            let snap_state_json = serde_json::to_string(&snap.state)?;
            let snap_created_at = snap.created_at.to_rfc3339();
            let snap_status_str = status_to_str(&snap.status);
            let snap_completed_json = serde_json::to_string(&snap.completed_nodes)?;
            let snap_active_json = serde_json::to_string(&snap.active_nodes)?;

            sqlx::query(
                r#"INSERT OR REPLACE INTO snapshots
                   (id, execution_id, at_sequence, state_json, created_at, tenant_id, status, completed_nodes_json, active_nodes_json, last_sequence)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
            )
            .bind(&snap_id)
            .bind(&snap_exec_id)
            .bind(snap.at_sequence)
            .bind(&snap_state_json)
            .bind(&snap_created_at)
            .bind(&self.tenant_id.0)
            .bind(snap_status_str)
            .bind(&snap_completed_json)
            .bind(&snap_active_json)
            .bind(snap.last_sequence)
            .execute(&mut *tx)
            .await
            .map_err(map_db_err)?;
        }

        tx.commit().await.map_err(map_db_err)?;
        Ok(sequence)
    }

    #[instrument(skip(self), fields(tenant = %self.tenant_id))]
    async fn reclaim_expired_leases(&self) -> BackendResult<ReclaimResult> {
        let now = Utc::now().to_rfc3339();

        let rows = sqlx::query(
            "SELECT * FROM work_items WHERE status = 'claimed' AND lease_expires_at < ? AND tenant_id = ? ORDER BY created_at ASC",
        )
        .bind(&now)
        .bind(&self.tenant_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(map_db_err)?;

        let mut result = ReclaimResult::default();

        for row in &rows {
            let item = row_to_work_item(row)?;
            let new_attempt = item.attempt + 1;
            let id_str = item.id.to_string();

            if new_attempt >= item.max_attempts {
                let dead_lettered_at = Utc::now().to_rfc3339();
                sqlx::query(
                    r#"INSERT OR IGNORE INTO dead_letter_items
                       (id, execution_id, node_id, queue_type, payload_json, attempt, last_error, created_at, dead_lettered_at, tenant_id)
                       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
                )
                .bind(&id_str)
                .bind(execution_id_str(&item.execution_id))
                .bind(&item.node_id)
                .bind(&item.queue_type)
                .bind(serde_json::to_string(&item.payload)?)
                .bind(new_attempt as i64)
                .bind("lease expired: worker dead")
                .bind(item.created_at.to_rfc3339())
                .bind(dead_lettered_at)
                .bind(&self.tenant_id.0)
                .execute(&self.pool)
                .await
                .map_err(map_db_err)?;

                sqlx::query("UPDATE work_items SET status = 'dead_lettered', attempt = ?, lease_expires_at = NULL, worker_id = NULL WHERE id = ?")
                    .bind(new_attempt as i64)
                    .bind(&id_str)
                    .execute(&self.pool)
                    .await
                    .map_err(map_db_err)?;

                let mut exhausted_item = item;
                exhausted_item.attempt = new_attempt;
                result.exhausted.push(exhausted_item);
            } else {
                let backoff_secs = 1u64 << new_attempt.min(6);
                let retry_after =
                    (Utc::now() + chrono::Duration::seconds(backoff_secs as i64)).to_rfc3339();

                sqlx::query(
                    "UPDATE work_items SET status = 'pending', attempt = ?, worker_id = NULL, lease_expires_at = NULL, retry_after = ?, lease_epoch = lease_epoch + 1 WHERE id = ?",
                )
                .bind(new_attempt as i64)
                .bind(&retry_after)
                .bind(&id_str)
                .execute(&self.pool)
                .await
                .map_err(map_db_err)?;

                let mut retry_item = item;
                retry_item.attempt = new_attempt;
                result.retryable.push(retry_item);
            }
        }

        Ok(result)
    }

    #[instrument(skip(self, last_error), fields(tenant = %self.tenant_id, item_id = %item_id))]
    async fn move_to_dead_letter(
        &self,
        item_id: WorkItemId,
        last_error: &str,
    ) -> BackendResult<()> {
        let id_str = item_id.to_string();

        let row = sqlx::query("SELECT * FROM work_items WHERE id = ? AND tenant_id = ?")
            .bind(&id_str)
            .bind(&self.tenant_id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_db_err)?;

        let Some(row) = row else {
            return Err(StateBackendError::NotFound(id_str));
        };
        let item = row_to_work_item(&row)?;
        let dead_lettered_at = Utc::now().to_rfc3339();

        sqlx::query(
            r#"INSERT OR REPLACE INTO dead_letter_items
               (id, execution_id, node_id, queue_type, payload_json, attempt, last_error, created_at, dead_lettered_at, tenant_id)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&id_str)
        .bind(execution_id_str(&item.execution_id))
        .bind(&item.node_id)
        .bind(&item.queue_type)
        .bind(serde_json::to_string(&item.payload)?)
        .bind(item.attempt as i64)
        .bind(last_error)
        .bind(item.created_at.to_rfc3339())
        .bind(dead_lettered_at)
        .bind(&self.tenant_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

        sqlx::query("UPDATE work_items SET status = 'dead_lettered', lease_expires_at = NULL, worker_id = NULL WHERE id = ? AND tenant_id = ?")
            .bind(&id_str)
            .bind(&self.tenant_id.0)
            .execute(&self.pool)
            .await
            .map_err(map_db_err)?;

        Ok(())
    }

    // ── API tokens ────────────────────────────────────────────────────────

    async fn create_token(&self, name: &str, role: &str) -> BackendResult<(String, ApiToken)> {
        use rand::Rng;
        use sha2::{Digest, Sha256};

        let random_bytes: [u8; 32] = rand::thread_rng().gen();
        let token = format!(
            "jj_{}",
            random_bytes
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let token_hash = format!("{:x}", Sha256::digest(token.as_bytes()));
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();

        sqlx::query(
            r#"INSERT INTO api_tokens (id, token_hash, name, role, created_at, tenant_id)
               VALUES (?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&id)
        .bind(&token_hash)
        .bind(name)
        .bind(role)
        .bind(&now)
        .bind(&self.tenant_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

        let info = ApiToken {
            id,
            name: name.to_string(),
            role: role.to_string(),
            created_at: Utc::now(),
            expires_at: None,
            tenant_id: self.tenant_id.0.clone(),
        };
        Ok((token, info))
    }

    async fn validate_token(&self, token: &str) -> BackendResult<Option<ApiToken>> {
        use sha2::{Digest, Sha256};

        let token_hash = format!("{:x}", Sha256::digest(token.as_bytes()));
        let now = Utc::now().to_rfc3339();

        let row = sqlx::query(
            r#"SELECT id, name, role, created_at, expires_at, tenant_id
               FROM api_tokens
               WHERE token_hash = ?
                 AND revoked_at IS NULL
                 AND (expires_at IS NULL OR expires_at > ?)"#,
        )
        .bind(&token_hash)
        .bind(&now)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db_err)?;

        let Some(row) = row else { return Ok(None) };

        let id: String = row.get("id");
        sqlx::query("UPDATE api_tokens SET last_used_at = ? WHERE id = ?")
            .bind(&now)
            .bind(&id)
            .execute(&self.pool)
            .await
            .map_err(map_db_err)?;

        let expires_at: Option<String> = row.get("expires_at");
        let tenant_id: String = row
            .try_get("tenant_id")
            .unwrap_or_else(|_| DEFAULT_TENANT.to_string());

        let info = ApiToken {
            id,
            name: row.get("name"),
            role: row.get("role"),
            created_at: row
                .get::<String, _>("created_at")
                .parse::<chrono::DateTime<Utc>>()
                .unwrap_or_else(|_| Utc::now()),
            expires_at: expires_at.and_then(|s| s.parse().ok()),
            tenant_id,
        };
        Ok(Some(info))
    }

    // ── Tenant management ─────────────────────────────────────────────────

    async fn create_tenant(&self, tenant: Tenant) -> BackendResult<()> {
        let policy_json = tenant
            .policy
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let limits_json = tenant
            .limits
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(StateBackendError::Serialization)?;

        sqlx::query(
            r#"INSERT INTO tenants (id, name, status, policy_json, limits_json, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&tenant.id.0)
        .bind(&tenant.name)
        .bind(tenant.status.as_str())
        .bind(policy_json.as_deref())
        .bind(limits_json.as_deref())
        .bind(tenant.created_at.to_rfc3339())
        .bind(tenant.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

        Ok(())
    }

    async fn get_tenant(&self, id: &TenantId) -> BackendResult<Option<Tenant>> {
        let row = sqlx::query("SELECT * FROM tenants WHERE id = ?")
            .bind(&id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_db_err)?;

        let Some(row) = row else { return Ok(None) };
        row_to_tenant(&row).map(Some)
    }

    async fn list_tenants(&self) -> BackendResult<Vec<Tenant>> {
        let rows = sqlx::query("SELECT * FROM tenants ORDER BY created_at ASC")
            .fetch_all(&self.pool)
            .await
            .map_err(map_db_err)?;

        rows.iter().map(row_to_tenant).collect()
    }

    async fn update_tenant(&self, tenant: Tenant) -> BackendResult<()> {
        let policy_json = tenant
            .policy
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let limits_json = tenant
            .limits
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(StateBackendError::Serialization)?;

        let rows_affected = sqlx::query(
            r#"UPDATE tenants SET name = ?, status = ?, policy_json = ?, limits_json = ?, updated_at = ?
               WHERE id = ?"#,
        )
        .bind(&tenant.name)
        .bind(tenant.status.as_str())
        .bind(policy_json.as_deref())
        .bind(limits_json.as_deref())
        .bind(tenant.updated_at.to_rfc3339())
        .bind(&tenant.id.0)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?
        .rows_affected();

        if rows_affected == 0 {
            return Err(StateBackendError::NotFound(tenant.id.0));
        }
        Ok(())
    }
}

/// Parse a datetime that might be either RFC 3339 or SQLite's `datetime('now')` format.
fn parse_datetime_flexible(s: &str) -> BackendResult<DateTime<Utc>> {
    // Try RFC 3339 first (e.g. "2026-03-12T00:00:00+00:00")
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    // Fallback: SQLite datetime format "YYYY-MM-DD HH:MM:SS"
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Ok(naive.and_utc());
    }
    Err(StateBackendError::Database(format!(
        "invalid datetime: {s}"
    )))
}

fn row_to_tenant(row: &sqlx::sqlite::SqliteRow) -> BackendResult<Tenant> {
    let policy: Option<serde_json::Value> = row
        .try_get::<Option<&str>, _>("policy_json")
        .map_err(map_db_err)?
        .map(serde_json::from_str)
        .transpose()
        .map_err(StateBackendError::Serialization)?;

    let limits: Option<TenantLimits> = row
        .try_get::<Option<&str>, _>("limits_json")
        .map_err(map_db_err)?
        .map(serde_json::from_str)
        .transpose()
        .map_err(StateBackendError::Serialization)?;

    let created_at =
        parse_datetime_flexible(row.try_get::<&str, _>("created_at").map_err(map_db_err)?)?;
    let updated_at =
        parse_datetime_flexible(row.try_get::<&str, _>("updated_at").map_err(map_db_err)?)?;

    Ok(Tenant {
        id: TenantId(row.try_get::<String, _>("id").map_err(map_db_err)?),
        name: row.try_get::<String, _>("name").map_err(map_db_err)?,
        status: TenantStatus::parse(row.try_get::<&str, _>("status").map_err(map_db_err)?),
        policy,
        limits,
        created_at,
        updated_at,
    })
}
