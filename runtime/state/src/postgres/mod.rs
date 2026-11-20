//! The Postgres state store: the [`StateBackend`] trait on a Postgres
//! database that several engine instances share.
//!
//! Where SQLite relies on one writer per file, this store locks rows: a
//! transaction that writes a run's log or status locks the run's row first,
//! a claim skips rows another claimer holds, and leases follow the
//! database's clock. Every transaction runs through `with_tx`, which
//! retries a deadlock or a lost connection and never a commit.
//!
//! A [`PgBackend`] does not migrate its database: [`migrate`] does, once per
//! deploy, and [`PgBackend::connect`] refuses a schema that is not the one
//! this build knows.
//!
//! A method that answers `not yet` is still to be written.

mod runs;
mod schema;
mod settings;
mod tx;

pub use schema::{migrate, schema_state, MigrateReport, SchemaState, MIGRATOR};
pub use settings::{
    PgSettings, ACQUIRE_TIMEOUT_ENV, IDLE_IN_TX_TIMEOUT_ENV, LOCK_TIMEOUT_ENV, MIN_POOL_SIZE,
    POOL_SIZE_ENV, STATEMENT_TIMEOUT_ENV,
};

use crate::artifact::ArtifactRef;
use crate::backend::{
    ApiToken, ApprovalCursor, ApprovalProjectionRow, BackendResult, ClaimFilter, ClaimedWorkItem,
    ConditionalAppend, ExecutionFinish, FailOutcome, GateMark, LeaseRenewal, LeasedItem,
    OpenWorkItem, PassCommit, PendingApprovalRow, ReclaimResult, ReserveOutcome, RunningPage,
    SchedulerPass, StateBackend, StateBackendError, TurnCommit, WorkItem, WorkItemId,
    WorkflowDefinition,
};
use crate::event::{Event, EventKind, EventSequence};
use crate::snapshot::Snapshot;
use crate::sqlite::RevokedToken;
use crate::tenant::{Tenant, TenantId};
use async_trait::async_trait;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::Row;
use std::str::FromStr;
use tx::with_tx;

/// The state store on Postgres. Unscoped, it sees every tenant, as the
/// engine's scheduler and workers do; [`Self::for_tenant`] gives the view a
/// request of one tenant uses. Clones share the pool.
#[derive(Clone)]
pub struct PgBackend {
    pool: PgPool,
    scope: Option<TenantId>,
}

impl PgBackend {
    /// Connect to the database at `url` and check that its schema is the one
    /// this build knows. Runs no migration: a database that is empty,
    /// behind, ahead, half migrated or changed is refused with what to do.
    pub async fn connect(url: &str, settings: PgSettings) -> Result<Self, String> {
        let backend = Self::connect_unchecked(url, settings).await?;
        let state = schema_state(&backend.pool)
            .await
            .map_err(|e| e.to_string())?;
        match state.refusal() {
            None => Ok(backend),
            Some(why) => {
                backend.pool.close().await;
                Err(why)
            }
        }
    }

    /// [`Self::connect`] without the schema check, for tests and for
    /// `migrate --check`, which report the state themselves.
    pub async fn connect_unchecked(url: &str, settings: PgSettings) -> Result<Self, String> {
        // The URL holds the password, so no message here repeats it.
        let options = PgConnectOptions::from_str(url)
            .map_err(|e| format!("not a Postgres URL: {e}"))?
            .application_name("jamjet");
        let session = settings.clone();
        let pool = PgPoolOptions::new()
            .max_connections(settings.pool_size)
            .acquire_timeout(settings.acquire_timeout)
            .after_connect(move |conn, _| {
                let session = session.clone();
                Box::pin(async move { set_session(conn, &session).await })
            })
            .connect_with(options)
            .await
            .map_err(|e| format!("cannot connect to Postgres: {e}"))?;
        Ok(Self { pool, scope: None })
    }

    /// The view of `tenant`, on the same pool: what it reads and writes is
    /// that tenant's.
    pub fn for_tenant(&self, tenant: TenantId) -> PgBackend {
        Self {
            pool: self.pool.clone(),
            scope: Some(tenant),
        }
    }

    /// The pool, shared with every clone and tenant view.
    pub fn pool(&self) -> PgPool {
        self.pool.clone()
    }

    /// The tenant of this view, `None` for the unscoped store.
    fn scope(&self) -> Option<&str> {
        self.scope.as_ref().map(|t| t.as_str())
    }

    /// Revoke the API token with this id, returning what was revoked (its
    /// tenant, role and name, for the audit record). `Ok(None)` when there is
    /// no such token, in this view's tenant if it has one, or it was already
    /// revoked.
    pub async fn revoke_token(&self, id: &str) -> BackendResult<Option<RevokedToken>> {
        tx::no_nul("token id", id)?;
        let tenant = self.scope.as_ref().map(|t| t.as_str());
        with_tx(&self.pool, "revoke_token", |tx| {
            Box::pin(async move {
                let row = match tenant {
                    None => {
                        sqlx::query(
                            "UPDATE api_tokens SET revoked_at = statement_timestamp() \
                             WHERE id = $1 AND revoked_at IS NULL \
                             RETURNING tenant_id, role, name",
                        )
                        .bind(id)
                        .fetch_optional(&mut **tx)
                        .await?
                    }
                    Some(tenant) => {
                        sqlx::query(
                            "UPDATE api_tokens SET revoked_at = statement_timestamp() \
                             WHERE id = $1 AND tenant_id = $2 AND revoked_at IS NULL \
                             RETURNING tenant_id, role, name",
                        )
                        .bind(id)
                        .bind(tenant)
                        .fetch_optional(&mut **tx)
                        .await?
                    }
                };
                Ok(row.map(|r| RevokedToken {
                    tenant_id: r.get("tenant_id"),
                    role: r.get("role"),
                    name: r.get("name"),
                }))
            })
        })
        .await
    }

    /// Raise the store's failover term to at least `term`, as a promotion
    /// does, and return the term after. It never goes down.
    pub async fn set_store_term_at_least(&self, term: i64) -> BackendResult<i64> {
        if term < 0 {
            return Err(StateBackendError::Database(format!(
                "a store term is not negative, got {term}"
            )));
        }
        with_tx(&self.pool, "set_store_term_at_least", |tx| {
            Box::pin(async move {
                let after: i64 = sqlx::query_scalar(
                    "UPDATE store_identity SET term = GREATEST(term, $1) WHERE id = 1 RETURNING term",
                )
                .bind(term)
                .fetch_one(&mut **tx)
                .await?;
                Ok(after)
            })
        })
        .await
    }
}

/// The session every pooled connection starts with: UTC, standard strings,
/// READ COMMITTED, and the timeouts from `settings`. The values are bound,
/// through `set_config`, rather than written into `SET` statements.
async fn set_session(
    conn: &mut sqlx::PgConnection,
    settings: &PgSettings,
) -> Result<(), sqlx::Error> {
    let ms = |d: std::time::Duration| d.as_millis().to_string();
    sqlx::query(
        "SELECT set_config('TimeZone', 'UTC', false), \
                set_config('standard_conforming_strings', 'on', false), \
                set_config('default_transaction_isolation', 'read committed', false), \
                set_config('statement_timeout', $1, false), \
                set_config('lock_timeout', $2, false), \
                set_config('idle_in_transaction_session_timeout', $3, false)",
    )
    .bind(ms(settings.statement_timeout))
    .bind(ms(settings.lock_timeout))
    .bind(ms(settings.idle_in_tx_timeout))
    .execute(conn)
    .await?;
    Ok(())
}

/// The answer of a method this store does not have yet.
fn not_yet<T>(method: &'static str) -> BackendResult<T> {
    Err(StateBackendError::Database(format!("not yet: {method}")))
}

#[async_trait]
impl StateBackend for PgBackend {
    async fn store_workflow(&self, def: WorkflowDefinition) -> BackendResult<()> {
        runs::store_workflow(self, def).await
    }

    async fn get_workflow(
        &self,
        workflow_id: &str,
        version: &str,
    ) -> BackendResult<Option<WorkflowDefinition>> {
        runs::get_workflow(self, workflow_id, version).await
    }

    async fn create_execution(&self, execution: WorkflowExecution) -> BackendResult<()> {
        runs::create_execution(self, execution).await
    }

    async fn get_execution(&self, id: &ExecutionId) -> BackendResult<Option<WorkflowExecution>> {
        runs::get_execution(self, id).await
    }

    async fn execution_tenant(&self, id: &ExecutionId) -> BackendResult<Option<TenantId>> {
        runs::execution_tenant(self, id).await
    }

    async fn update_execution_status(
        &self,
        id: &ExecutionId,
        status: WorkflowStatus,
    ) -> BackendResult<()> {
        runs::update_execution_status(self, id, status).await
    }

    async fn finish_execution(
        &self,
        event: Event,
        status: WorkflowStatus,
    ) -> BackendResult<ExecutionFinish> {
        runs::finish_execution(self, event, status).await
    }

    async fn append_event_if(
        &self,
        event: Event,
        condition: &crate::backend::EventCondition,
    ) -> BackendResult<ConditionalAppend> {
        runs::append_event_if(self, event, condition).await
    }

    async fn update_execution_current_state(
        &self,
        id: &ExecutionId,
        current_state: &serde_json::Value,
    ) -> BackendResult<()> {
        runs::update_execution_current_state(self, id, current_state).await
    }

    async fn patch_append_array(
        &self,
        execution_id: &ExecutionId,
        key: &str,
        value: serde_json::Value,
    ) -> BackendResult<()> {
        runs::patch_append_array(self, execution_id, key, value).await
    }

    async fn list_executions(
        &self,
        status: Option<WorkflowStatus>,
        limit: u32,
        offset: u32,
    ) -> BackendResult<Vec<WorkflowExecution>> {
        runs::list_executions(self, status, limit, offset).await
    }

    async fn list_running_executions_after(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> BackendResult<RunningPage> {
        runs::list_running_executions_after(self, after, limit).await
    }

    async fn append_event(&self, event: Event) -> BackendResult<EventSequence> {
        runs::append_event(self, event).await
    }

    async fn get_events(&self, execution_id: &ExecutionId) -> BackendResult<Vec<Event>> {
        runs::get_events(self, execution_id).await
    }

    async fn get_events_since(
        &self,
        execution_id: &ExecutionId,
        since_sequence: EventSequence,
    ) -> BackendResult<Vec<Event>> {
        runs::get_events_since(self, execution_id, since_sequence).await
    }

    async fn get_events_page(
        &self,
        execution_id: &ExecutionId,
        after: EventSequence,
        limit: u32,
        max_bytes: usize,
    ) -> BackendResult<Vec<Event>> {
        runs::get_events_page(self, execution_id, after, limit, max_bytes).await
    }

    async fn latest_sequence(&self, execution_id: &ExecutionId) -> BackendResult<EventSequence> {
        runs::latest_sequence(self, execution_id).await
    }

    async fn list_claimed_work_items(&self, limit: u32) -> BackendResult<Vec<ClaimedWorkItem>> {
        runs::list_claimed_work_items(self, limit).await
    }

    async fn write_snapshot(&self, snapshot: Snapshot) -> BackendResult<()> {
        runs::write_snapshot(self, snapshot).await
    }

    async fn latest_snapshot(&self, execution_id: &ExecutionId) -> BackendResult<Option<Snapshot>> {
        runs::latest_snapshot(self, execution_id).await
    }

    async fn create_segment_atomic(
        &self,
        execution: WorkflowExecution,
        seed_snapshot: Snapshot,
        started_event: EventKind,
        scheduled_event: EventKind,
        work_item: WorkItem,
    ) -> BackendResult<()> {
        runs::create_run(
            self,
            execution,
            Some(seed_snapshot),
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
    ) -> BackendResult<()> {
        runs::create_run(
            self,
            execution,
            None,
            started_event,
            scheduled_event,
            work_item,
        )
        .await
    }

    async fn get_tool_effect(&self, _key: &str) -> BackendResult<Option<serde_json::Value>> {
        not_yet("get_tool_effect")
    }

    async fn reserve_tool_effect(
        &self,
        _key: &str,
        _execution_id: &ExecutionId,
        _node_id: &str,
        _owner: &str,
        _lease_fence: i64,
        _ttl: std::time::Duration,
    ) -> BackendResult<ReserveOutcome> {
        not_yet("reserve_tool_effect")
    }

    async fn release_tool_reservation(
        &self,
        _key: &str,
        _owner: &str,
        _lease_fence: i64,
    ) -> BackendResult<()> {
        not_yet("release_tool_reservation")
    }

    async fn put_artifact(
        &self,
        _bytes: &[u8],
        _media_type: Option<&str>,
    ) -> BackendResult<ArtifactRef> {
        not_yet("put_artifact")
    }

    async fn get_artifact(&self, _hash: &str) -> BackendResult<Option<Vec<u8>>> {
        not_yet("get_artifact")
    }

    async fn artifact_size(&self, _hash: &str) -> BackendResult<Option<u64>> {
        not_yet("artifact_size")
    }

    async fn enqueue_work_item(&self, _item: WorkItem) -> BackendResult<WorkItemId> {
        not_yet("enqueue_work_item")
    }

    async fn claim_work_item(
        &self,
        _worker_id: &str,
        _queue_types: &[&str],
    ) -> BackendResult<Option<WorkItem>> {
        not_yet("claim_work_item")
    }

    async fn renew_lease(
        &self,
        _item_id: WorkItemId,
        _worker_id: &str,
        _lease_fence: i64,
    ) -> BackendResult<()> {
        not_yet("renew_lease")
    }

    async fn claim_work_item_with_lease(
        &self,
        _worker_id: &str,
        _queue_types: &[&str],
        _lease_secs: Option<u64>,
    ) -> BackendResult<Option<WorkItem>> {
        not_yet("claim_work_item_with_lease")
    }

    async fn claim_work_item_filtered(
        &self,
        _worker_id: &str,
        _queue_types: &[&str],
        _lease_secs: Option<u64>,
        _filter: &ClaimFilter,
    ) -> BackendResult<Option<WorkItem>> {
        not_yet("claim_work_item_filtered")
    }

    async fn leased_items(
        &self,
        _queue_types: &[&str],
        _filter: &ClaimFilter,
    ) -> BackendResult<Vec<LeasedItem>> {
        not_yet("leased_items")
    }

    async fn mark_gate(
        &self,
        _item_id: WorkItemId,
        _lease_fence: i64,
        _mark: GateMark,
    ) -> BackendResult<bool> {
        not_yet("mark_gate")
    }

    async fn swap_gate(
        &self,
        _item_id: WorkItemId,
        _lease_fence: i64,
        _from: GateMark,
        _to: GateMark,
    ) -> BackendResult<bool> {
        not_yet("swap_gate")
    }

    async fn release_lease(
        &self,
        _item_id: WorkItemId,
        _lease_fence: i64,
        _from: GateMark,
        _lease_secs: u64,
    ) -> BackendResult<bool> {
        not_yet("release_lease")
    }

    async fn has_claimable(
        &self,
        _queue_types: &[&str],
        _filter: &ClaimFilter,
    ) -> BackendResult<bool> {
        not_yet("has_claimable")
    }

    async fn heartbeat_lease(
        &self,
        _item_id: WorkItemId,
        _worker_id: &str,
        _lease_fence: i64,
    ) -> BackendResult<LeaseRenewal> {
        not_yet("heartbeat_lease")
    }

    async fn count_node_completions(
        &self,
        execution_id: &ExecutionId,
        node_id: &str,
    ) -> BackendResult<u64> {
        runs::count_node_completions(self, execution_id, node_id).await
    }

    async fn get_work_item(&self, _item_id: WorkItemId) -> BackendResult<Option<WorkItem>> {
        not_yet("get_work_item")
    }

    async fn complete_work_item(&self, _item_id: WorkItemId) -> BackendResult<()> {
        not_yet("complete_work_item")
    }

    async fn complete_work_item_fenced(
        &self,
        _item_id: WorkItemId,
        _lease_fence: i64,
    ) -> BackendResult<bool> {
        not_yet("complete_work_item_fenced")
    }

    async fn fail_work_item(&self, _item_id: WorkItemId, _error: &str) -> BackendResult<()> {
        not_yet("fail_work_item")
    }

    async fn fail_work_item_fenced(
        &self,
        _item_id: WorkItemId,
        _lease_fence: i64,
        _error: &str,
    ) -> BackendResult<Option<FailOutcome>> {
        not_yet("fail_work_item_fenced")
    }

    async fn commit_turn(
        &self,
        _item_id: WorkItemId,
        _lease_fence: i64,
        _terminal_event: Event,
        _write_snapshot: bool,
    ) -> BackendResult<EventSequence> {
        not_yet("commit_turn")
    }

    async fn commit_turn_unless_terminal(
        &self,
        _item_id: WorkItemId,
        _lease_fence: i64,
        _terminal_event: Event,
        _write_snapshot: bool,
    ) -> BackendResult<TurnCommit> {
        not_yet("commit_turn_unless_terminal")
    }

    async fn withdraw_work_item_fenced(
        &self,
        _item_id: WorkItemId,
        _lease_fence: i64,
    ) -> BackendResult<bool> {
        not_yet("withdraw_work_item_fenced")
    }

    async fn park_work_item(
        &self,
        _item_id: WorkItemId,
        _lease_fence: i64,
        _retry_after: &str,
        _next_attempt: u32,
    ) -> BackendResult<bool> {
        not_yet("park_work_item")
    }

    async fn park_for_execution(
        &self,
        _item_id: WorkItemId,
        _lease_fence: i64,
        _attempt: u32,
    ) -> BackendResult<bool> {
        not_yet("park_for_execution")
    }

    async fn finalize_rollover_fenced(
        &self,
        _execution_id: &ExecutionId,
        _work_item_id: WorkItemId,
        _lease_fence: i64,
    ) -> BackendResult<bool> {
        not_yet("finalize_rollover_fenced")
    }

    async fn reclaim_expired_leases(&self) -> BackendResult<ReclaimResult> {
        not_yet("reclaim_expired_leases")
    }

    async fn move_to_dead_letter(
        &self,
        _item_id: WorkItemId,
        _last_error: &str,
    ) -> BackendResult<()> {
        not_yet("move_to_dead_letter")
    }

    async fn create_token(&self, _name: &str, _role: &str) -> BackendResult<(String, ApiToken)> {
        not_yet("create_token")
    }

    async fn validate_token(&self, _token: &str) -> BackendResult<Option<ApiToken>> {
        not_yet("validate_token")
    }

    async fn apply_approval_projection(
        &self,
        _row: ApprovalProjectionRow,
        _projection_name: &str,
        _new_checkpoint: i64,
    ) -> BackendResult<()> {
        not_yet("apply_approval_projection")
    }

    async fn apply_approval_projection_batch(
        &self,
        _rows: Vec<ApprovalProjectionRow>,
        _projection_name: &str,
        _execution_id: &ExecutionId,
        _new_checkpoint: i64,
    ) -> BackendResult<()> {
        not_yet("apply_approval_projection_batch")
    }

    async fn get_approval_projection(
        &self,
        _execution_id: &ExecutionId,
    ) -> BackendResult<Vec<ApprovalProjectionRow>> {
        not_yet("get_approval_projection")
    }

    async fn get_projector_checkpoint(
        &self,
        _projection_name: &str,
        _execution_id: &ExecutionId,
    ) -> BackendResult<i64> {
        not_yet("get_projector_checkpoint")
    }

    async fn set_projector_checkpoint(
        &self,
        _projection_name: &str,
        _execution_id: &ExecutionId,
        _new_checkpoint: i64,
    ) -> BackendResult<()> {
        not_yet("set_projector_checkpoint")
    }

    async fn list_pending_approvals(
        &self,
        _after: Option<&ApprovalCursor>,
        _limit: u32,
    ) -> BackendResult<Vec<PendingApprovalRow>> {
        not_yet("list_pending_approvals")
    }

    async fn open_work_items(
        &self,
        _execution_id: &ExecutionId,
    ) -> BackendResult<Vec<OpenWorkItem>> {
        not_yet("open_work_items")
    }

    async fn create_tenant(&self, _tenant: Tenant) -> BackendResult<()> {
        not_yet("create_tenant")
    }

    async fn get_tenant(&self, _id: &TenantId) -> BackendResult<Option<Tenant>> {
        not_yet("get_tenant")
    }

    async fn list_tenants(&self) -> BackendResult<Vec<Tenant>> {
        not_yet("list_tenants")
    }

    async fn update_tenant(&self, _tenant: Tenant) -> BackendResult<()> {
        not_yet("update_tenant")
    }

    async fn commit_scheduler_pass(&self, pass: SchedulerPass) -> BackendResult<PassCommit> {
        runs::commit_scheduler_pass(self, pass).await
    }

    async fn reclaim_expired_leases_recorded(&self) -> BackendResult<ReclaimResult> {
        not_yet("reclaim_expired_leases_recorded")
    }

    async fn fail_work_item_fenced_recorded(
        &self,
        _item_id: WorkItemId,
        _lease_fence: i64,
        _error: &str,
    ) -> BackendResult<Option<FailOutcome>> {
        not_yet("fail_work_item_fenced_recorded")
    }
}
