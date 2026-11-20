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

mod commit;
mod effects;
mod projection;
mod queue;
mod runs;
mod schema;
mod settings;
mod tokens;
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

    async fn get_tool_effect(&self, key: &str) -> BackendResult<Option<serde_json::Value>> {
        effects::get_tool_effect(self, key).await
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
        effects::reserve_tool_effect(self, key, execution_id, node_id, owner, lease_fence, ttl)
            .await
    }

    async fn release_tool_reservation(
        &self,
        key: &str,
        owner: &str,
        lease_fence: i64,
    ) -> BackendResult<()> {
        effects::release_tool_reservation(self, key, owner, lease_fence).await
    }

    async fn put_artifact(
        &self,
        bytes: &[u8],
        media_type: Option<&str>,
    ) -> BackendResult<ArtifactRef> {
        effects::put_artifact(self, bytes, media_type).await
    }

    async fn get_artifact(&self, hash: &str) -> BackendResult<Option<Vec<u8>>> {
        effects::get_artifact(self, hash).await
    }

    async fn artifact_size(&self, hash: &str) -> BackendResult<Option<u64>> {
        effects::artifact_size(self, hash).await
    }

    async fn enqueue_work_item(&self, item: WorkItem) -> BackendResult<WorkItemId> {
        queue::enqueue_work_item(self, item).await
    }

    async fn claim_work_item(
        &self,
        worker_id: &str,
        queue_types: &[&str],
    ) -> BackendResult<Option<WorkItem>> {
        let all = ClaimFilter::default();
        queue::claim_work_item_filtered(self, worker_id, queue_types, None, &all).await
    }

    async fn renew_lease(
        &self,
        item_id: WorkItemId,
        worker_id: &str,
        lease_fence: i64,
    ) -> BackendResult<()> {
        queue::renew_lease(self, item_id, worker_id, lease_fence).await
    }

    async fn claim_work_item_with_lease(
        &self,
        worker_id: &str,
        queue_types: &[&str],
        lease_secs: Option<u64>,
    ) -> BackendResult<Option<WorkItem>> {
        let all = ClaimFilter::default();
        queue::claim_work_item_filtered(self, worker_id, queue_types, lease_secs, &all).await
    }

    async fn claim_work_item_filtered(
        &self,
        worker_id: &str,
        queue_types: &[&str],
        lease_secs: Option<u64>,
        filter: &ClaimFilter,
    ) -> BackendResult<Option<WorkItem>> {
        queue::claim_work_item_filtered(self, worker_id, queue_types, lease_secs, filter).await
    }

    async fn leased_items(
        &self,
        queue_types: &[&str],
        filter: &ClaimFilter,
    ) -> BackendResult<Vec<LeasedItem>> {
        queue::leased_items(self, queue_types, filter).await
    }

    async fn mark_gate(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        mark: GateMark,
    ) -> BackendResult<bool> {
        queue::mark_gate(self, item_id, lease_fence, mark).await
    }

    async fn swap_gate(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        from: GateMark,
        to: GateMark,
    ) -> BackendResult<bool> {
        queue::swap_gate(self, item_id, lease_fence, from, to, None).await
    }

    async fn release_lease(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        from: GateMark,
        lease_secs: u64,
    ) -> BackendResult<bool> {
        let to = GateMark::Released;
        queue::swap_gate(self, item_id, lease_fence, from, to, Some(lease_secs)).await
    }

    async fn has_claimable(
        &self,
        queue_types: &[&str],
        filter: &ClaimFilter,
    ) -> BackendResult<bool> {
        queue::has_claimable(self, queue_types, filter).await
    }

    async fn heartbeat_lease(
        &self,
        item_id: WorkItemId,
        worker_id: &str,
        lease_fence: i64,
    ) -> BackendResult<LeaseRenewal> {
        queue::heartbeat_lease(self, item_id, worker_id, lease_fence).await
    }

    async fn count_node_completions(
        &self,
        execution_id: &ExecutionId,
        node_id: &str,
    ) -> BackendResult<u64> {
        runs::count_node_completions(self, execution_id, node_id).await
    }

    async fn get_work_item(&self, item_id: WorkItemId) -> BackendResult<Option<WorkItem>> {
        queue::get_work_item(self, item_id).await
    }

    async fn complete_work_item(&self, item_id: WorkItemId) -> BackendResult<()> {
        queue::complete_work_item(self, item_id).await
    }

    async fn complete_work_item_fenced(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
    ) -> BackendResult<bool> {
        queue::complete_work_item_fenced(self, item_id, lease_fence).await
    }

    async fn fail_work_item(&self, item_id: WorkItemId, _error: &str) -> BackendResult<()> {
        // The error is the caller's to record; this settles the item only.
        queue::fail_work_item(self, item_id).await
    }

    async fn fail_work_item_fenced(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        error: &str,
    ) -> BackendResult<Option<FailOutcome>> {
        queue::fail_work_item_fenced(self, item_id, lease_fence, error).await
    }

    async fn commit_turn(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        terminal_event: Event,
        write_snapshot: bool,
    ) -> BackendResult<EventSequence> {
        let commit = commit::commit_turn(
            self,
            item_id,
            lease_fence,
            terminal_event,
            write_snapshot,
            false,
        )
        .await?;
        match commit {
            TurnCommit::Committed(sequence) => Ok(sequence),
            // Not reached: the run's status is checked only when asked.
            TurnCommit::ExecutionTerminal(status) => Err(StateBackendError::Database(format!(
                "commit_turn refused for a {status:?} execution"
            ))),
        }
    }

    async fn commit_turn_unless_terminal(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        terminal_event: Event,
        write_snapshot: bool,
    ) -> BackendResult<TurnCommit> {
        commit::commit_turn(
            self,
            item_id,
            lease_fence,
            terminal_event,
            write_snapshot,
            true,
        )
        .await
    }

    async fn withdraw_work_item_fenced(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
    ) -> BackendResult<bool> {
        queue::withdraw_work_item_fenced(self, item_id, lease_fence).await
    }

    async fn park_work_item(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        retry_after: &str,
        next_attempt: u32,
    ) -> BackendResult<bool> {
        queue::park_work_item(self, item_id, lease_fence, retry_after, next_attempt).await
    }

    async fn park_for_execution(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        _attempt: u32,
    ) -> BackendResult<bool> {
        // The attempt stays as it is, as on SQLite.
        queue::park_for_execution(self, item_id, lease_fence).await
    }

    async fn finalize_rollover_fenced(
        &self,
        execution_id: &ExecutionId,
        work_item_id: WorkItemId,
        lease_fence: i64,
    ) -> BackendResult<bool> {
        queue::finalize_rollover_fenced(self, execution_id, work_item_id, lease_fence).await
    }

    async fn reclaim_expired_leases(&self) -> BackendResult<ReclaimResult> {
        queue::reclaim_expired_leases(self).await
    }

    async fn move_to_dead_letter(
        &self,
        item_id: WorkItemId,
        last_error: &str,
    ) -> BackendResult<()> {
        queue::move_to_dead_letter(self, item_id, last_error).await
    }

    async fn create_token(&self, name: &str, role: &str) -> BackendResult<(String, ApiToken)> {
        tokens::create_token(self, name, role).await
    }

    async fn validate_token(&self, token: &str) -> BackendResult<Option<ApiToken>> {
        tokens::validate_token(self, token).await
    }

    async fn apply_approval_projection(
        &self,
        row: ApprovalProjectionRow,
        projection_name: &str,
        new_checkpoint: i64,
    ) -> BackendResult<()> {
        projection::apply_approval_projection(self, row, projection_name, new_checkpoint).await
    }

    async fn apply_approval_projection_batch(
        &self,
        rows: Vec<ApprovalProjectionRow>,
        projection_name: &str,
        execution_id: &ExecutionId,
        new_checkpoint: i64,
    ) -> BackendResult<()> {
        projection::apply_approval_projection_batch(
            self,
            rows,
            projection_name,
            execution_id,
            new_checkpoint,
        )
        .await
    }

    async fn get_approval_projection(
        &self,
        execution_id: &ExecutionId,
    ) -> BackendResult<Vec<ApprovalProjectionRow>> {
        projection::get_approval_projection(self, execution_id).await
    }

    async fn get_projector_checkpoint(
        &self,
        projection_name: &str,
        execution_id: &ExecutionId,
    ) -> BackendResult<i64> {
        projection::get_projector_checkpoint(self, projection_name, execution_id).await
    }

    async fn set_projector_checkpoint(
        &self,
        projection_name: &str,
        execution_id: &ExecutionId,
        new_checkpoint: i64,
    ) -> BackendResult<()> {
        projection::set_projector_checkpoint(self, projection_name, execution_id, new_checkpoint)
            .await
    }

    async fn list_pending_approvals(
        &self,
        after: Option<&ApprovalCursor>,
        limit: u32,
    ) -> BackendResult<Vec<PendingApprovalRow>> {
        projection::list_pending_approvals(self, after, limit).await
    }

    async fn open_work_items(
        &self,
        execution_id: &ExecutionId,
    ) -> BackendResult<Vec<OpenWorkItem>> {
        queue::open_work_items(self, execution_id).await
    }

    async fn create_tenant(&self, tenant: Tenant) -> BackendResult<()> {
        tokens::create_tenant(self, tenant).await
    }

    async fn get_tenant(&self, id: &TenantId) -> BackendResult<Option<Tenant>> {
        tokens::get_tenant(self, id).await
    }

    async fn list_tenants(&self) -> BackendResult<Vec<Tenant>> {
        tokens::list_tenants(self).await
    }

    async fn update_tenant(&self, tenant: Tenant) -> BackendResult<()> {
        tokens::update_tenant(self, tenant).await
    }

    async fn commit_scheduler_pass(&self, pass: SchedulerPass) -> BackendResult<PassCommit> {
        runs::commit_scheduler_pass(self, pass).await
    }

    async fn reclaim_expired_leases_recorded(&self) -> BackendResult<ReclaimResult> {
        queue::reclaim_expired_leases_recorded(self).await
    }

    async fn fail_work_item_fenced_recorded(
        &self,
        item_id: WorkItemId,
        lease_fence: i64,
        error: &str,
    ) -> BackendResult<Option<FailOutcome>> {
        queue::fail_work_item_fenced_recorded(self, item_id, lease_fence, error).await
    }
}
