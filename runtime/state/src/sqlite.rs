use crate::backend::{
    ApiToken, BackendResult, ReclaimResult, StateBackend, StateBackendError, WorkItem, WorkItemId,
    WorkflowDefinition,
};
use crate::event::{Event, EventKind, EventSequence};
use crate::snapshot::Snapshot;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use sqlx::{sqlite::SqliteConnectOptions, Row, SqlitePool};
use std::str::FromStr;
use tracing::instrument;
use uuid::Uuid;

/// SQLite-backed state store for local development.
///
/// Run migrations with [`SqliteBackend::migrate`] before first use.
pub struct SqliteBackend {
    pool: SqlitePool,
}

impl SqliteBackend {
    /// Connect to the SQLite database at `database_url` and return a backend.
    /// `database_url` is a SQLx-compatible URL, e.g. `sqlite:///path/to/db.sqlite3`.
    /// The database file is created automatically if it does not exist.
    pub async fn connect(database_url: &str) -> Result<Self, sqlx::Error> {
        let opts = SqliteConnectOptions::from_str(database_url)?.create_if_missing(true);
        let pool = SqlitePool::connect_with(opts).await?;
        Ok(Self { pool })
    }

    /// Run embedded migrations against the connected database.
    pub async fn migrate(&self) -> Result<(), sqlx::migrate::MigrateError> {
        sqlx::migrate!("./migrations").run(&self.pool).await
    }

    /// Convenience: connect and immediately run migrations.
    pub async fn open(
        database_url: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let backend = Self::connect(database_url).await?;
        backend.migrate().await?;
        Ok(backend)
    }

    /// Create a tenant-scoped view of this backend.
    ///
    /// All operations on the returned backend are filtered by `tenant_id`,
    /// ensuring complete data isolation between tenants.
    pub fn for_tenant(
        &self,
        tenant_id: crate::tenant::TenantId,
    ) -> crate::tenant_scoped::TenantScopedSqliteBackend {
        crate::tenant_scoped::TenantScopedSqliteBackend::new(self.pool.clone(), tenant_id)
    }

    /// Get a clone of the underlying connection pool.
    pub fn pool(&self) -> SqlitePool {
        self.pool.clone()
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub(crate) fn map_db_err(e: sqlx::Error) -> StateBackendError {
    StateBackendError::Database(e.to_string())
}

fn execution_id_str(id: &ExecutionId) -> String {
    id.0.to_string()
}

pub(crate) fn parse_execution_id(s: &str) -> BackendResult<ExecutionId> {
    let uuid = Uuid::parse_str(s)
        .map_err(|e| StateBackendError::Database(format!("invalid execution_id: {e}")))?;
    Ok(ExecutionId(uuid))
}

pub(crate) fn parse_datetime(s: &str) -> BackendResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| StateBackendError::Database(format!("invalid datetime: {e}")))
}

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
        .unwrap_or_else(|_| crate::tenant::DEFAULT_TENANT.to_string());

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
impl StateBackend for SqliteBackend {
    // ── Workflow definitions ──────────────────────────────────────────────

    #[instrument(skip(self, def), fields(workflow_id = %def.workflow_id, version = %def.version))]
    async fn store_workflow(&self, def: WorkflowDefinition) -> BackendResult<()> {
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
        .bind(&def.tenant_id)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

        Ok(())
    }

    #[instrument(skip(self), fields(workflow_id = workflow_id, version = version))]
    async fn get_workflow(
        &self,
        workflow_id: &str,
        version: &str,
    ) -> BackendResult<Option<WorkflowDefinition>> {
        let row =
            sqlx::query("SELECT * FROM workflow_definitions WHERE workflow_id = ? AND version = ?")
                .bind(workflow_id)
                .bind(version)
                .fetch_optional(&self.pool)
                .await
                .map_err(map_db_err)?;

        let Some(row) = row else { return Ok(None) };

        let ir: serde_json::Value =
            serde_json::from_str(row.try_get::<&str, _>("ir_json").map_err(map_db_err)?)
                .map_err(StateBackendError::Serialization)?;
        let created_at = parse_datetime(row.try_get::<&str, _>("created_at").map_err(map_db_err)?)?;

        let tenant_id: String = row
            .try_get("tenant_id")
            .unwrap_or_else(|_| crate::tenant::DEFAULT_TENANT.to_string());

        Ok(Some(WorkflowDefinition {
            workflow_id: row
                .try_get::<String, _>("workflow_id")
                .map_err(map_db_err)?,
            version: row.try_get::<String, _>("version").map_err(map_db_err)?,
            ir,
            created_at,
            tenant_id,
        }))
    }

    // ── Executions ────────────────────────────────────────────────────────

    #[instrument(skip(self, execution), fields(execution_id = %execution.execution_id))]
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
                started_at, updated_at, completed_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
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
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

        Ok(())
    }

    #[instrument(skip(self), fields(execution_id = %id))]
    async fn get_execution(&self, id: &ExecutionId) -> BackendResult<Option<WorkflowExecution>> {
        let id_str = execution_id_str(id);
        let row = sqlx::query("SELECT * FROM workflow_executions WHERE execution_id = ?")
            .bind(&id_str)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_db_err)?;

        row.map(|r| row_to_execution(&r)).transpose()
    }

    #[instrument(skip(self), fields(execution_id = %id, status = ?status))]
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
            "UPDATE workflow_executions SET status = ?, updated_at = ?, completed_at = COALESCE(?, completed_at) WHERE execution_id = ?",
        )
        .bind(status_str)
        .bind(&now)
        .bind(completed_at.as_deref())
        .bind(&id_str)
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
            "UPDATE workflow_executions SET current_state = ?, updated_at = ? WHERE execution_id = ?",
        )
        .bind(&state_str)
        .bind(&now)
        .bind(&id_str)
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

    #[instrument(skip(self))]
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
                    "SELECT * FROM workflow_executions WHERE status = ? ORDER BY updated_at DESC LIMIT ? OFFSET ?",
                )
                .bind(status_str)
                .bind(limit as i64)
                .bind(offset as i64)
                .fetch_all(&self.pool)
                .await
                .map_err(map_db_err)?
            }
            None => sqlx::query(
                "SELECT * FROM workflow_executions ORDER BY updated_at DESC LIMIT ? OFFSET ?",
            )
            .bind(limit as i64)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(map_db_err)?,
        };

        rows.iter().map(row_to_execution).collect()
    }

    // ── Event log ─────────────────────────────────────────────────────────

    #[instrument(skip(self, event), fields(execution_id = %event.execution_id, seq = event.sequence))]
    async fn append_event(&self, event: Event) -> BackendResult<EventSequence> {
        let id = event.id.to_string();
        let execution_id = execution_id_str(&event.execution_id);
        let kind_json = serde_json::to_string(&event.kind)?;
        let created_at = event.created_at.to_rfc3339();

        sqlx::query(
            r#"INSERT INTO events (id, execution_id, sequence, kind_json, created_at)
               VALUES (?, ?, ?, ?, ?)"#,
        )
        .bind(&id)
        .bind(&execution_id)
        .bind(event.sequence)
        .bind(&kind_json)
        .bind(&created_at)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

        Ok(event.sequence)
    }

    #[instrument(skip(self), fields(execution_id = %execution_id))]
    async fn get_events(&self, execution_id: &ExecutionId) -> BackendResult<Vec<Event>> {
        let id_str = execution_id_str(execution_id);
        let rows = sqlx::query("SELECT * FROM events WHERE execution_id = ? ORDER BY sequence ASC")
            .bind(&id_str)
            .fetch_all(&self.pool)
            .await
            .map_err(map_db_err)?;

        rows.iter().map(row_to_event).collect()
    }

    #[instrument(skip(self), fields(execution_id = %execution_id, since = since_sequence))]
    async fn get_events_since(
        &self,
        execution_id: &ExecutionId,
        since_sequence: EventSequence,
    ) -> BackendResult<Vec<Event>> {
        let id_str = execution_id_str(execution_id);
        let rows = sqlx::query(
            "SELECT * FROM events WHERE execution_id = ? AND sequence > ? ORDER BY sequence ASC",
        )
        .bind(&id_str)
        .bind(since_sequence)
        .fetch_all(&self.pool)
        .await
        .map_err(map_db_err)?;

        rows.iter().map(row_to_event).collect()
    }

    #[instrument(skip(self), fields(execution_id = %execution_id))]
    async fn latest_sequence(&self, execution_id: &ExecutionId) -> BackendResult<EventSequence> {
        let id_str = execution_id_str(execution_id);
        let row = sqlx::query(
            "SELECT COALESCE(MAX(sequence), 0) as seq FROM events WHERE execution_id = ?",
        )
        .bind(&id_str)
        .fetch_one(&self.pool)
        .await
        .map_err(map_db_err)?;

        Ok(row.try_get::<i64, _>("seq").map_err(map_db_err)?)
    }

    // ── Snapshots ─────────────────────────────────────────────────────────

    #[instrument(skip(self, snapshot), fields(execution_id = %snapshot.execution_id, at_seq = snapshot.at_sequence))]
    async fn write_snapshot(&self, snapshot: Snapshot) -> BackendResult<()> {
        let id = snapshot.id.to_string();
        let execution_id = execution_id_str(&snapshot.execution_id);
        let state_json = serde_json::to_string(&snapshot.state)?;
        let created_at = snapshot.created_at.to_rfc3339();

        sqlx::query(
            r#"INSERT OR REPLACE INTO snapshots (id, execution_id, at_sequence, state_json, created_at)
               VALUES (?, ?, ?, ?, ?)"#,
        )
        .bind(&id)
        .bind(&execution_id)
        .bind(snapshot.at_sequence)
        .bind(&state_json)
        .bind(&created_at)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

        Ok(())
    }

    #[instrument(skip(self), fields(execution_id = %execution_id))]
    async fn latest_snapshot(&self, execution_id: &ExecutionId) -> BackendResult<Option<Snapshot>> {
        let id_str = execution_id_str(execution_id);
        let row = sqlx::query(
            "SELECT * FROM snapshots WHERE execution_id = ? ORDER BY at_sequence DESC LIMIT 1",
        )
        .bind(&id_str)
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

    #[instrument(skip(self, item), fields(execution_id = %item.execution_id, node_id = %item.node_id))]
    async fn enqueue_work_item(&self, item: WorkItem) -> BackendResult<WorkItemId> {
        let id = item.id.to_string();
        let execution_id = execution_id_str(&item.execution_id);
        let payload_json = serde_json::to_string(&item.payload)?;
        let created_at = item.created_at.to_rfc3339();

        sqlx::query(
            r#"INSERT INTO work_items
               (id, execution_id, node_id, queue_type, payload_json, attempt, max_attempts, status, created_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, 'pending', ?)"#,
        )
        .bind(&id)
        .bind(&execution_id)
        .bind(&item.node_id)
        .bind(&item.queue_type)
        .bind(&payload_json)
        .bind(item.attempt as i64)
        .bind(item.max_attempts as i64)
        .bind(&created_at)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

        Ok(item.id)
    }

    #[instrument(skip(self), fields(worker_id = worker_id))]
    async fn claim_work_item(
        &self,
        worker_id: &str,
        queue_types: &[&str],
    ) -> BackendResult<Option<WorkItem>> {
        if queue_types.is_empty() {
            return Ok(None);
        }

        // Expire stale leases first
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE work_items SET status = 'pending', worker_id = NULL, lease_expires_at = NULL \
             WHERE status = 'claimed' AND lease_expires_at < ?",
        )
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

        // SQLite doesn't support UPDATE ... RETURNING with a subquery easily, so
        // we use a transaction: SELECT FOR UPDATE equivalent via exclusive transaction.
        let mut tx = self.pool.begin().await.map_err(map_db_err)?;

        // Build placeholders for queue_types IN clause
        let placeholders = queue_types
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let query_str = format!(
            "SELECT * FROM work_items WHERE status = 'pending' AND queue_type IN ({}) \
             AND (retry_after IS NULL OR retry_after <= ?) ORDER BY created_at ASC LIMIT 1",
            placeholders
        );
        let mut q = sqlx::query(&query_str);
        for qt in queue_types {
            q = q.bind(*qt);
        }
        q = q.bind(&now); // for retry_after <= now check
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

        // Return item with updated fields
        let mut claimed = item;
        claimed.worker_id = Some(worker_id.to_string());
        claimed.lease_expires_at = Some(
            DateTime::parse_from_rfc3339(&lease_expires_at)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| StateBackendError::Database(e.to_string()))?,
        );
        Ok(Some(claimed))
    }

    #[instrument(skip(self), fields(item_id = %item_id, worker_id = worker_id))]
    async fn renew_lease(&self, item_id: WorkItemId, worker_id: &str) -> BackendResult<()> {
        let lease_expires_at = (Utc::now() + chrono::Duration::seconds(30)).to_rfc3339();
        let id_str = item_id.to_string();

        let rows_affected = sqlx::query(
            "UPDATE work_items SET lease_expires_at = ? WHERE id = ? AND worker_id = ? AND status = 'claimed'",
        )
        .bind(&lease_expires_at)
        .bind(&id_str)
        .bind(worker_id)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?
        .rows_affected();

        if rows_affected == 0 {
            return Err(StateBackendError::NotFound(id_str));
        }
        Ok(())
    }

    #[instrument(skip(self), fields(item_id = %item_id))]
    async fn complete_work_item(&self, item_id: WorkItemId) -> BackendResult<()> {
        let id_str = item_id.to_string();
        let completed_at = Utc::now().to_rfc3339();

        let rows_affected = sqlx::query(
            "UPDATE work_items SET status = 'completed', completed_at = ?, lease_expires_at = NULL WHERE id = ?",
        )
        .bind(&completed_at)
        .bind(&id_str)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?
        .rows_affected();

        if rows_affected == 0 {
            return Err(StateBackendError::NotFound(id_str));
        }
        Ok(())
    }

    #[instrument(skip(self, error), fields(item_id = %item_id))]
    async fn fail_work_item(&self, item_id: WorkItemId, error: &str) -> BackendResult<()> {
        let id_str = item_id.to_string();
        let _ = error; // logged by caller; stored in event log not here

        let rows_affected = sqlx::query(
            "UPDATE work_items SET status = 'failed', lease_expires_at = NULL, worker_id = NULL WHERE id = ?",
        )
        .bind(&id_str)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?
        .rows_affected();

        if rows_affected == 0 {
            return Err(StateBackendError::NotFound(id_str));
        }
        Ok(())
    }

    #[instrument(skip(self))]
    async fn reclaim_expired_leases(&self) -> BackendResult<ReclaimResult> {
        let now = Utc::now().to_rfc3339();

        // Find all claimed items whose lease has expired.
        let rows = sqlx::query(
            "SELECT * FROM work_items WHERE status = 'claimed' AND lease_expires_at < ? ORDER BY created_at ASC",
        )
        .bind(&now)
        .fetch_all(&self.pool)
        .await
        .map_err(map_db_err)?;

        let mut result = ReclaimResult::default();

        for row in &rows {
            let item = row_to_work_item(row)?;
            let new_attempt = item.attempt + 1;
            let id_str = item.id.to_string();

            if new_attempt >= item.max_attempts {
                // Exhausted — move to dead-letter (caller emits the event)
                let dead_lettered_at = Utc::now().to_rfc3339();
                sqlx::query(
                    r#"INSERT OR IGNORE INTO dead_letter_items
                       (id, execution_id, node_id, queue_type, payload_json, attempt, last_error, created_at, dead_lettered_at)
                       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
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
                // Retryable — reset to pending with incremented attempt.
                // Apply exponential backoff: 2^attempt seconds.
                let backoff_secs = 1u64 << new_attempt.min(6); // max 64s
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

    #[instrument(skip(self, last_error), fields(item_id = %item_id))]
    async fn move_to_dead_letter(
        &self,
        item_id: WorkItemId,
        last_error: &str,
    ) -> BackendResult<()> {
        let id_str = item_id.to_string();

        // Load the item first to copy fields.
        let row = sqlx::query("SELECT * FROM work_items WHERE id = ?")
            .bind(&id_str)
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
               (id, execution_id, node_id, queue_type, payload_json, attempt, last_error, created_at, dead_lettered_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
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
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

        sqlx::query("UPDATE work_items SET status = 'dead_lettered', lease_expires_at = NULL, worker_id = NULL WHERE id = ?")
            .bind(&id_str)
            .execute(&self.pool)
            .await
            .map_err(map_db_err)?;

        Ok(())
    }

    async fn create_token(&self, name: &str, role: &str) -> BackendResult<(String, ApiToken)> {
        use rand::Rng;
        use sha2::{Digest, Sha256};

        // Generate a random 32-byte token, hex-encoded with a prefix.
        let random_bytes: [u8; 32] = rand::thread_rng().gen();
        let token = format!(
            "jj_{}",
            random_bytes
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let token_hash = format!("{:x}", Sha256::digest(token.as_bytes()));

        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();

        sqlx::query(
            r#"INSERT INTO api_tokens (id, token_hash, name, role, created_at)
               VALUES (?, ?, ?, ?, ?)"#,
        )
        .bind(&id)
        .bind(&token_hash)
        .bind(name)
        .bind(role)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(map_db_err)?;

        let info = ApiToken {
            id,
            name: name.to_string(),
            role: role.to_string(),
            created_at: Utc::now(),
            expires_at: None,
            tenant_id: crate::tenant::DEFAULT_TENANT.to_string(),
        };
        Ok((token, info))
    }

    async fn validate_token(&self, token: &str) -> BackendResult<Option<ApiToken>> {
        use sha2::{Digest, Sha256};

        let token_hash = format!("{:x}", Sha256::digest(token.as_bytes()));
        let now = Utc::now().to_rfc3339();

        let row = sqlx::query(
            r#"SELECT id, name, role, created_at, expires_at
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

        // Update last_used_at.
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
            .unwrap_or_else(|_| crate::tenant::DEFAULT_TENANT.to_string());
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use jamjet_core::workflow::{WorkflowExecution, WorkflowStatus};
    use serde_json::json;

    async fn open_test_db() -> SqliteBackend {
        let backend = SqliteBackend::open("sqlite::memory:")
            .await
            .expect("failed to open in-memory SQLite");
        backend
    }

    fn sample_execution() -> WorkflowExecution {
        let now = Utc::now();
        WorkflowExecution {
            execution_id: ExecutionId::new(),
            workflow_id: "test-wf".to_string(),
            workflow_version: "1.0.0".to_string(),
            status: WorkflowStatus::Pending,
            initial_input: json!({"x": 1}),
            current_state: json!({}),
            started_at: now,
            updated_at: now,
            completed_at: None,
            session_type: None,
        }
    }

    #[tokio::test]
    async fn test_create_and_get_execution() {
        let db = open_test_db().await;
        let exec = sample_execution();
        let id = exec.execution_id.clone();
        db.create_execution(exec).await.unwrap();
        let fetched = db.get_execution(&id).await.unwrap().unwrap();
        assert_eq!(fetched.workflow_id, "test-wf");
        assert_eq!(fetched.status, WorkflowStatus::Pending);
    }

    #[tokio::test]
    async fn test_update_status() {
        let db = open_test_db().await;
        let exec = sample_execution();
        let id = exec.execution_id.clone();
        db.create_execution(exec).await.unwrap();
        db.update_execution_status(&id, WorkflowStatus::Running)
            .await
            .unwrap();
        let fetched = db.get_execution(&id).await.unwrap().unwrap();
        assert_eq!(fetched.status, WorkflowStatus::Running);
    }

    #[tokio::test]
    async fn test_list_executions() {
        let db = open_test_db().await;
        db.create_execution(sample_execution()).await.unwrap();
        db.create_execution(sample_execution()).await.unwrap();
        let all = db.list_executions(None, 10, 0).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn test_event_log() {
        use crate::event::{Event, EventKind};
        let db = open_test_db().await;
        let exec = sample_execution();
        let exec_id = exec.execution_id.clone();
        db.create_execution(exec).await.unwrap();

        let event = Event::new(
            exec_id.clone(),
            1,
            EventKind::WorkflowStarted {
                workflow_id: "test-wf".to_string(),
                workflow_version: "1.0.0".to_string(),
                initial_input: json!({"x": 1}),
            },
        );
        db.append_event(event).await.unwrap();

        let events = db.get_events(&exec_id).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence, 1);

        let seq = db.latest_sequence(&exec_id).await.unwrap();
        assert_eq!(seq, 1);
    }

    #[tokio::test]
    async fn test_snapshot() {
        use crate::snapshot::Snapshot;
        let db = open_test_db().await;
        let exec = sample_execution();
        let exec_id = exec.execution_id.clone();
        db.create_execution(exec).await.unwrap();

        let snap = Snapshot::new(exec_id.clone(), 5, json!({"nodes_completed": ["a", "b"]}));
        db.write_snapshot(snap).await.unwrap();

        let loaded = db.latest_snapshot(&exec_id).await.unwrap().unwrap();
        assert_eq!(loaded.at_sequence, 5);
    }

    #[tokio::test]
    async fn test_workflow_definition() {
        use crate::backend::WorkflowDefinition;
        let db = open_test_db().await;

        let def = WorkflowDefinition {
            workflow_id: "my-wf".to_string(),
            version: "1.0.0".to_string(),
            ir: json!({"workflow_id": "my-wf", "version": "1.0.0", "nodes": {}}),
            created_at: Utc::now(),
            tenant_id: crate::tenant::DEFAULT_TENANT.to_string(),
        };
        db.store_workflow(def).await.unwrap();

        let loaded = db.get_workflow("my-wf", "1.0.0").await.unwrap().unwrap();
        assert_eq!(loaded.workflow_id, "my-wf");
        assert_eq!(loaded.version, "1.0.0");

        // Non-existent version returns None
        let missing = db.get_workflow("my-wf", "2.0.0").await.unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn test_work_item_queue() {
        let db = open_test_db().await;
        let exec = sample_execution();
        let exec_id = exec.execution_id.clone();
        db.create_execution(exec).await.unwrap();

        let item = WorkItem {
            id: Uuid::new_v4(),
            execution_id: exec_id.clone(),
            node_id: "node-1".to_string(),
            queue_type: "default".to_string(),
            payload: json!({}),
            attempt: 0,
            max_attempts: 3,
            created_at: Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            tenant_id: crate::tenant::DEFAULT_TENANT.to_string(),
        };
        let item_id = item.id;
        db.enqueue_work_item(item).await.unwrap();

        let claimed = db
            .claim_work_item("worker-1", &["default"])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.node_id, "node-1");
        assert_eq!(claimed.worker_id.as_deref(), Some("worker-1"));

        db.complete_work_item(item_id).await.unwrap();

        // No more items
        let none = db.claim_work_item("worker-1", &["default"]).await.unwrap();
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn test_patch_append_array() {
        let db = open_test_db().await;
        let exec = sample_execution();
        let id = exec.execution_id.clone();
        db.create_execution(exec).await.unwrap();

        db.patch_append_array(
            &id,
            "agent_tool_events",
            json!({"type": "progress", "chunk": 0}),
        )
        .await
        .unwrap();
        db.patch_append_array(
            &id,
            "agent_tool_events",
            json!({"type": "progress", "chunk": 1}),
        )
        .await
        .unwrap();

        let fetched = db.get_execution(&id).await.unwrap().unwrap();
        let events = fetched.current_state["agent_tool_events"]
            .as_array()
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["chunk"], 0);
        assert_eq!(events[1]["chunk"], 1);
    }
}
