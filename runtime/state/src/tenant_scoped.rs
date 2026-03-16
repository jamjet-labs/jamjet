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

        sqlx::query(
            r#"INSERT INTO workflow_executions
               (execution_id, workflow_id, workflow_version, status, initial_input, current_state,
                started_at, updated_at, completed_at, tenant_id)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
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

        sqlx::query(
            r#"INSERT INTO events (id, execution_id, sequence, kind_json, created_at, tenant_id)
               VALUES (?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&id)
        .bind(&execution_id)
        .bind(event.sequence)
        .bind(&kind_json)
        .bind(&created_at)
        .bind(&self.tenant_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

        Ok(event.sequence)
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

        sqlx::query(
            r#"INSERT OR REPLACE INTO snapshots (id, execution_id, at_sequence, state_json, created_at, tenant_id)
               VALUES (?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&id)
        .bind(&execution_id)
        .bind(snapshot.at_sequence)
        .bind(&state_json)
        .bind(&created_at)
        .bind(&self.tenant_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

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

        Ok(Some(Snapshot {
            id,
            execution_id,
            at_sequence,
            state,
            created_at,
        }))
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
        // Expire stale leases for this tenant.
        sqlx::query(
            "UPDATE work_items SET status = 'pending', worker_id = NULL, lease_expires_at = NULL \
             WHERE status = 'claimed' AND lease_expires_at < ? AND tenant_id = ?",
        )
        .bind(&now)
        .bind(&self.tenant_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

        let mut tx = self.pool.begin().await.map_err(map_db_err)?;

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

        sqlx::query(
            "UPDATE work_items SET status = 'claimed', worker_id = ?, lease_expires_at = ?, claimed_at = ? WHERE id = ?",
        )
        .bind(worker_id)
        .bind(&lease_expires_at)
        .bind(&claimed_at)
        .bind(&item_id)
        .execute(&mut *tx)
        .await
        .map_err(map_db_err)?;

        tx.commit().await.map_err(map_db_err)?;

        let mut claimed = item;
        claimed.worker_id = Some(worker_id.to_string());
        claimed.lease_expires_at = Some(
            DateTime::parse_from_rfc3339(&lease_expires_at)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| StateBackendError::Database(e.to_string()))?,
        );
        Ok(Some(claimed))
    }

    #[instrument(skip(self), fields(tenant = %self.tenant_id, item_id = %item_id))]
    async fn renew_lease(&self, item_id: WorkItemId, worker_id: &str) -> BackendResult<()> {
        let lease_expires_at = (Utc::now() + chrono::Duration::seconds(30)).to_rfc3339();
        let id_str = item_id.to_string();

        let rows_affected = sqlx::query(
            "UPDATE work_items SET lease_expires_at = ? WHERE id = ? AND worker_id = ? AND status = 'claimed' AND tenant_id = ?",
        )
        .bind(&lease_expires_at)
        .bind(&id_str)
        .bind(worker_id)
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
                    "UPDATE work_items SET status = 'pending', attempt = ?, worker_id = NULL, lease_expires_at = NULL, retry_after = ? WHERE id = ?",
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
