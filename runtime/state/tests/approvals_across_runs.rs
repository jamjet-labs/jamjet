//! `list_pending_approvals`: every waiting approval across runs, in one
//! keyset-paged list.
//!
//! Two columns of the projection are not trusted. Its `tenant_id` holds
//! `default` for every row, because the projector writes unscoped; and a run
//! that ended with a request open leaves that row `pending` for good, because
//! the projector only visits running runs. Who owns a row and whether its run
//! is live both come from the run's own row.
//!
//! The shared cases run on memory, SQLite and tenant-scoped SQLite. The
//! tenant cases need a scoped store, so they run on SQLite only.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::{ApprovalCursor, PendingApprovalRow, StateBackend};
use jamjet_state::tenant::{Tenant, TenantId, TenantStatus};
use jamjet_state::{ApprovalProjectionRow, InMemoryBackend, SqliteBackend};
use serde_json::json;

// ── Fixtures ──────────────────────────────────────────────────────────────────

/// One store, as a case sees it.
struct Fixture {
    /// What the code under test reads through, as a route does.
    backend: Arc<dyn StateBackend>,
    /// The engine's unscoped handle on the same store. The projector writes
    /// every row through it.
    engine: Arc<dyn StateBackend>,
}

async fn memory() -> Fixture {
    let b: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    Fixture {
        backend: b.clone(),
        engine: b,
    }
}

async fn sqlite() -> Fixture {
    let b: Arc<dyn StateBackend> = Arc::new(open().await);
    Fixture {
        backend: b.clone(),
        engine: b,
    }
}

/// Scoped to `acme`. Its runs are acme's; their rows are written unscoped,
/// as the projector writes them, so each row's own tenant column says
/// `default`.
async fn scoped_sqlite() -> Fixture {
    let base = open().await;
    register(&base, "acme").await;
    Fixture {
        backend: Arc::new(base.for_tenant(TenantId::from("acme"))),
        engine: Arc::new(base),
    }
}

async fn open() -> SqliteBackend {
    SqliteBackend::open("sqlite::memory:").await.unwrap()
}

/// Register a tenant, so the foreign keys on its rows hold.
async fn register(base: &SqliteBackend, id: &str) {
    let now = Utc::now();
    base.for_tenant(TenantId::default())
        .create_tenant(Tenant {
            id: TenantId::from(id),
            name: id.to_string(),
            status: TenantStatus::Active,
            policy: None,
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
}

/// Each case once per backend, under the case's own name.
macro_rules! on_every_backend {
    ($($case:ident),* $(,)?) => {
        mod memory {
            $(#[tokio::test]
            async fn $case() {
                super::$case(&super::memory().await).await;
            })*
        }
        mod sqlite {
            $(#[tokio::test]
            async fn $case() {
                super::$case(&super::sqlite().await).await;
            })*
        }
        mod scoped_sqlite {
            $(#[tokio::test]
            async fn $case() {
                super::$case(&super::scoped_sqlite().await).await;
            })*
        }
    };
}

on_every_backend!(
    pending_rows_of_live_runs_are_listed_oldest_first,
    a_cancelled_or_failed_runs_open_request_is_not_listed,
    paging_by_cursor_returns_each_row_once,
);

// ── Helpers ───────────────────────────────────────────────────────────────────

fn run(workflow_id: &str, status: WorkflowStatus) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        execution_id: ExecutionId::new(),
        workflow_id: workflow_id.to_string(),
        workflow_version: "1.0.0".to_string(),
        status,
        initial_input: json!({}),
        current_state: json!({}),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    }
}

async fn start(b: &dyn StateBackend, workflow_id: &str, status: WorkflowStatus) -> ExecutionId {
    let e = run(workflow_id, status);
    let id = e.execution_id.clone();
    b.create_execution(e).await.unwrap();
    id
}

/// A request waiting on `node`, as the projector folds `ToolApprovalRequired`.
fn pending_row(exec: &ExecutionId, node: &str, sequence: i64) -> ApprovalProjectionRow {
    ApprovalProjectionRow {
        execution_id: exec.clone(),
        node_id: node.to_string(),
        status: "pending".into(),
        user_id: None,
        comment: None,
        last_sequence: sequence,
        tool_name: Some(format!("tool-{node}")),
        approver: Some("human".into()),
        context: Some(json!({ "action_digest": format!("digest-{node}") })),
    }
}

/// Write one run's rows as one projector batch: one write, one time.
async fn project(fx: &Fixture, exec: &ExecutionId, rows: Vec<ApprovalProjectionRow>) {
    let checkpoint = rows.iter().map(|r| r.last_sequence).max().unwrap_or(0);
    fx.engine
        .apply_approval_projection_batch(rows, "approvals", exec, checkpoint)
        .await
        .unwrap();
}

fn key(exec: &ExecutionId, node: &str) -> (String, String) {
    (exec.0.to_string(), node.to_string())
}

fn keys(rows: &[PendingApprovalRow]) -> Vec<(String, String)> {
    rows.iter()
        .map(|r| key(&r.execution_id, &r.node_id))
        .collect()
}

// ── Shared cases ──────────────────────────────────────────────────────────────

/// Three live runs, one of them paused, each with one waiting request. They
/// are projected in an order that is neither the runs' id order nor the
/// nodes' name order, and the list follows the projection time. A decided
/// row and a live run with no request add nothing.
async fn pending_rows_of_live_runs_are_listed_oldest_first(fx: &Fixture) {
    let mut workflows = HashMap::new();
    let mut runs = Vec::new();
    for (wf, status) in [
        ("wf-one", WorkflowStatus::Running),
        ("wf-paused", WorkflowStatus::Paused),
        ("wf-two", WorkflowStatus::Running),
    ] {
        let id = start(&*fx.backend, wf, status).await;
        workflows.insert(id.0.to_string(), wf);
        runs.push(id);
    }
    start(&*fx.backend, "wf-idle", WorkflowStatus::Running).await;
    runs.sort_by_key(|id| id.0.to_string());
    let (r1, r2, r3) = (runs[0].clone(), runs[1].clone(), runs[2].clone());

    let mut decided = pending_row(&r1, "n0", 1);
    decided.status = "approved".into();
    decided.tool_name = None;
    decided.approver = None;
    project(fx, &r1, vec![decided]).await;

    let order = [(r3, "n2"), (r1, "n3"), (r2, "n1")];
    for (i, (exec, node)) in order.iter().enumerate() {
        // Apart in time, so the order is the projection's and not a tie.
        tokio::time::sleep(Duration::from_millis(5)).await;
        project(fx, exec, vec![pending_row(exec, node, 10 + i as i64)]).await;
    }

    let rows = fx.backend.list_pending_approvals(None, 50).await.unwrap();
    assert_eq!(
        keys(&rows),
        order.iter().map(|(e, n)| key(e, n)).collect::<Vec<_>>(),
        "oldest projected first"
    );
    for pair in rows.windows(2) {
        assert!(pair[0].updated_at < pair[1].updated_at, "{pair:?}");
    }
    for (i, (row, (_, node))) in rows.iter().zip(order.iter()).enumerate() {
        assert_eq!(row.workflow_id, workflows[&row.execution_id.0.to_string()]);
        assert_eq!(row.workflow_version, "1.0.0");
        assert_eq!(
            row.tool_name.as_deref(),
            Some(format!("tool-{node}").as_str())
        );
        assert_eq!(row.approver.as_deref(), Some("human"));
        assert_eq!(
            row.context,
            Some(json!({ "action_digest": format!("digest-{node}") }))
        );
        assert_eq!(row.request_sequence, 10 + i as i64);
    }
}

/// A run that ended with its request open keeps a `pending` row in the
/// projection (the projector no longer visits it). The list leaves it out,
/// whichever way the run ended.
async fn a_cancelled_or_failed_runs_open_request_is_not_listed(fx: &Fixture) {
    let live = start(&*fx.backend, "wf", WorkflowStatus::Running).await;
    project(fx, &live, vec![pending_row(&live, "review", 3)]).await;
    let mut ended = Vec::new();
    for status in [
        WorkflowStatus::Cancelled,
        WorkflowStatus::Failed,
        WorkflowStatus::Completed,
        WorkflowStatus::LimitExceeded,
    ] {
        let exec = start(&*fx.backend, "wf", WorkflowStatus::Running).await;
        project(fx, &exec, vec![pending_row(&exec, "review", 3)]).await;
        fx.backend
            .update_execution_status(&exec, status)
            .await
            .unwrap();
        ended.push(exec);
    }
    for exec in &ended {
        let projected = fx.engine.get_approval_projection(exec).await.unwrap();
        assert_eq!(projected[0].status, "pending", "the row stays pending");
    }

    let rows = fx.backend.list_pending_approvals(None, 50).await.unwrap();
    assert_eq!(keys(&rows), vec![key(&live, "review")]);
}

/// 250 rows read in pages of 100. Each run's rows are one projector batch and
/// share one time, and the page ends fall inside such a tie, so the cursor
/// must carry the run and the node, not the time alone.
async fn paging_by_cursor_returns_each_row_once(fx: &Fixture) {
    let mut all = BTreeSet::new();
    // 83 runs of three rows, then one of one: 250, with ties of three that
    // straddle rows 100 and 200.
    for n in 0..84 {
        let exec = start(&*fx.backend, "wf", WorkflowStatus::Running).await;
        let nodes: &[&str] = if n < 83 { &["a", "b", "c"] } else { &["a"] };
        let rows = nodes
            .iter()
            .map(|node| pending_row(&exec, node, 5))
            .collect();
        project(fx, &exec, rows).await;
        for node in nodes {
            all.insert(key(&exec, node));
        }
    }
    assert_eq!(all.len(), 250);

    let mut pages: Vec<Vec<PendingApprovalRow>> = Vec::new();
    let mut after: Option<ApprovalCursor> = None;
    loop {
        let page = fx
            .backend
            .list_pending_approvals(after.as_ref(), 100)
            .await
            .unwrap();
        if page.is_empty() {
            break;
        }
        after = Some(ApprovalCursor::from(page.last().unwrap()));
        pages.push(page);
        assert!(pages.len() <= 3, "more pages than rows");
    }
    assert_eq!(
        pages.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![100, 100, 50]
    );
    for boundary in [0, 1] {
        assert_eq!(
            pages[boundary].last().unwrap().updated_at,
            pages[boundary + 1][0].updated_at,
            "page {boundary} ends inside a tie"
        );
    }

    let walked: Vec<(String, String)> = pages.iter().flat_map(|p| keys(p)).collect();
    let distinct: BTreeSet<_> = walked.iter().cloned().collect();
    assert_eq!(walked.len(), 250, "no row twice");
    assert_eq!(distinct, all, "no row missed");
    let whole = fx.backend.list_pending_approvals(None, 1000).await.unwrap();
    assert_eq!(
        walked,
        keys(&whole),
        "the pages are the whole list in order"
    );
}

// ── Tenants (scoped SQLite) ───────────────────────────────────────────────────

/// Each tenant's rows, written under that tenant, are listed for it alone.
#[tokio::test]
async fn a_scoped_backend_lists_its_tenants_rows_only() {
    let base = open().await;
    register(&base, "acme").await;
    register(&base, "globex").await;
    let acme = base.for_tenant(TenantId::from("acme"));
    let globex = base.for_tenant(TenantId::from("globex"));
    let default = base.for_tenant(TenantId::default());

    let mut expected = Vec::new();
    for scope in [&acme, &globex, &default] {
        let exec = start(scope, "wf", WorkflowStatus::Running).await;
        scope
            .apply_approval_projection(pending_row(&exec, "review", 3), "approvals", 3)
            .await
            .unwrap();
        expected.push(key(&exec, "review"));
    }

    for (scope, want) in [&acme, &globex, &default].into_iter().zip(&expected) {
        let rows = scope.list_pending_approvals(None, 50).await.unwrap();
        assert_eq!(keys(&rows), vec![want.clone()], "{}", scope.tenant_id());
    }
    let unscoped = base.list_pending_approvals(None, 50).await.unwrap();
    let mut got = keys(&unscoped);
    got.sort();
    expected.sort();
    assert_eq!(got, expected, "the unscoped backend lists every tenant's");
}

/// The projector writes `default` into every row's tenant column, so an acme
/// run's row says `default`. It is listed for acme, whose run it is, and not
/// for the default tenant.
#[tokio::test]
async fn the_projections_default_tenant_column_is_not_trusted() {
    let base = open().await;
    register(&base, "acme").await;
    let acme = base.for_tenant(TenantId::from("acme"));
    let default = base.for_tenant(TenantId::default());

    let exec = start(&acme, "wf", WorkflowStatus::Running).await;
    base.apply_approval_projection(pending_row(&exec, "review", 3), "approvals", 3)
        .await
        .unwrap();
    let stored: String =
        sqlx::query_scalar("SELECT tenant_id FROM proj_approvals WHERE execution_id = ?")
            .bind(exec.0.to_string())
            .fetch_one(&base.pool())
            .await
            .unwrap();
    assert_eq!(stored, "default", "the row's own column says default");

    let rows = acme.list_pending_approvals(None, 50).await.unwrap();
    assert_eq!(keys(&rows), vec![key(&exec, "review")]);
    let rows = default.list_pending_approvals(None, 50).await.unwrap();
    assert!(rows.is_empty(), "listed for the default tenant: {rows:?}");
}

// ── The index (SQLite) ────────────────────────────────────────────────────────

/// Each page of the list comes from `idx_proj_approvals_pending` in order,
/// with and without a tenant and a cursor, not from sorting every pending
/// row. The query is the one `list_pending_approvals` sends.
#[tokio::test]
async fn the_pending_list_reads_its_index_in_order() {
    let base = open().await;
    let pool = base.pool();
    for (tenant, cursor) in [(false, false), (true, false), (false, true), (true, true)] {
        let sql = format!(
            "EXPLAIN QUERY PLAN \
             SELECT p.execution_id, e.workflow_id, e.workflow_version, p.node_id, p.tool_name, \
                    p.approver, p.context_json, p.last_sequence, p.updated_at \
             FROM proj_approvals p JOIN workflow_executions e ON e.execution_id = p.execution_id \
             WHERE p.status = 'pending' AND e.status IN ('running', 'paused'){}{} \
             ORDER BY p.updated_at, p.execution_id, p.node_id \
             LIMIT ?",
            if tenant { " AND e.tenant_id = ?" } else { "" },
            if cursor {
                " AND (p.updated_at, p.execution_id, p.node_id) > (?, ?, ?)"
            } else {
                ""
            },
        );
        let mut q = sqlx::query_as::<_, (i64, i64, i64, String)>(sqlx::AssertSqlSafe(sql));
        if tenant {
            q = q.bind("acme");
        }
        if cursor {
            q = q.bind("").bind("").bind("");
        }
        let steps: Vec<String> = q
            .bind(100_i64)
            .fetch_all(&pool)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.3)
            .collect();
        assert!(
            !steps.iter().any(|s| s.contains("TEMP B-TREE")),
            "tenant {tenant}, cursor {cursor}: sorts in a temporary B-tree: {steps:?}"
        );
        assert!(
            steps
                .iter()
                .any(|s| s.contains("idx_proj_approvals_pending")),
            "tenant {tenant}, cursor {cursor}: does not use the index: {steps:?}"
        );
    }
}

// ── A backend that keeps the defaults ─────────────────────────────────────────

/// The real in-memory backend behind every required method, and the trait's
/// defaults for the two this lane adds: an implementor outside the repo that
/// has not written them.
struct OnTheDefaults(InMemoryBackend);

/// With a waiting request and an open item in the store, both methods still
/// refuse rather than answer an empty list.
#[tokio::test]
async fn a_backend_without_the_methods_refuses() {
    let b = OnTheDefaults(InMemoryBackend::new());
    let exec = start(&b, "wf", WorkflowStatus::Running).await;
    b.apply_approval_projection(pending_row(&exec, "review", 3), "approvals", 3)
        .await
        .unwrap();
    b.enqueue_work_item(jamjet_state::WorkItem {
        id: uuid::Uuid::new_v4(),
        execution_id: exec.clone(),
        node_id: "step".into(),
        queue_type: "python_fn".into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: "default".into(),
    })
    .await
    .unwrap();
    assert_eq!(
        b.0.list_pending_approvals(None, 50).await.unwrap().len(),
        1,
        "the store has a row to list"
    );
    assert_eq!(b.0.open_work_items(&exec).await.unwrap().len(), 1);

    let refused = b.list_pending_approvals(None, 50).await;
    assert!(
        matches!(refused, Err(jamjet_state::StateBackendError::Database(_))),
        "{refused:?}"
    );
    let refused = b.open_work_items(&exec).await;
    assert!(
        matches!(refused, Err(jamjet_state::StateBackendError::Database(_))),
        "{refused:?}"
    );
}

mod on_the_defaults {
    use super::OnTheDefaults;
    use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
    use jamjet_state::backend::{
        ApiToken, BackendResult, EventCondition, FailOutcome, ReclaimResult, ReserveOutcome,
        StateBackend, WorkItem, WorkItemId, WorkflowDefinition,
    };
    use jamjet_state::event::{Event, EventKind, EventSequence};
    use jamjet_state::{
        ApprovalProjectionRow, ArtifactRef, ConditionalAppend, ExecutionFinish, Snapshot, TenantId,
        TurnCommit,
    };

    #[async_trait::async_trait]
    impl StateBackend for OnTheDefaults {
        async fn store_workflow(&self, def: WorkflowDefinition) -> BackendResult<()> {
            self.0.store_workflow(def).await
        }
        async fn get_workflow(
            &self,
            workflow_id: &str,
            version: &str,
        ) -> BackendResult<Option<WorkflowDefinition>> {
            self.0.get_workflow(workflow_id, version).await
        }
        async fn create_execution(&self, execution: WorkflowExecution) -> BackendResult<()> {
            self.0.create_execution(execution).await
        }
        async fn get_execution(
            &self,
            id: &ExecutionId,
        ) -> BackendResult<Option<WorkflowExecution>> {
            self.0.get_execution(id).await
        }
        async fn execution_tenant(&self, id: &ExecutionId) -> BackendResult<Option<TenantId>> {
            self.0.execution_tenant(id).await
        }
        async fn update_execution_status(
            &self,
            id: &ExecutionId,
            status: WorkflowStatus,
        ) -> BackendResult<()> {
            self.0.update_execution_status(id, status).await
        }
        async fn finish_execution(
            &self,
            event: Event,
            status: WorkflowStatus,
        ) -> BackendResult<ExecutionFinish> {
            self.0.finish_execution(event, status).await
        }
        async fn append_event_if(
            &self,
            event: Event,
            condition: &EventCondition,
        ) -> BackendResult<ConditionalAppend> {
            self.0.append_event_if(event, condition).await
        }
        async fn update_execution_current_state(
            &self,
            id: &ExecutionId,
            current_state: &serde_json::Value,
        ) -> BackendResult<()> {
            self.0
                .update_execution_current_state(id, current_state)
                .await
        }
        async fn patch_append_array(
            &self,
            execution_id: &ExecutionId,
            key: &str,
            value: serde_json::Value,
        ) -> BackendResult<()> {
            self.0.patch_append_array(execution_id, key, value).await
        }
        async fn list_executions(
            &self,
            status: Option<WorkflowStatus>,
            limit: u32,
            offset: u32,
        ) -> BackendResult<Vec<WorkflowExecution>> {
            self.0.list_executions(status, limit, offset).await
        }
        async fn append_event(&self, event: Event) -> BackendResult<EventSequence> {
            self.0.append_event(event).await
        }
        async fn get_events(&self, execution_id: &ExecutionId) -> BackendResult<Vec<Event>> {
            self.0.get_events(execution_id).await
        }
        async fn get_events_since(
            &self,
            execution_id: &ExecutionId,
            since_sequence: EventSequence,
        ) -> BackendResult<Vec<Event>> {
            self.0.get_events_since(execution_id, since_sequence).await
        }
        async fn latest_sequence(
            &self,
            execution_id: &ExecutionId,
        ) -> BackendResult<EventSequence> {
            self.0.latest_sequence(execution_id).await
        }
        async fn write_snapshot(&self, snapshot: Snapshot) -> BackendResult<()> {
            self.0.write_snapshot(snapshot).await
        }
        async fn latest_snapshot(
            &self,
            execution_id: &ExecutionId,
        ) -> BackendResult<Option<Snapshot>> {
            self.0.latest_snapshot(execution_id).await
        }
        async fn create_segment_atomic(
            &self,
            execution: WorkflowExecution,
            seed_snapshot: Snapshot,
            started_event: EventKind,
            scheduled_event: EventKind,
            work_item: WorkItem,
        ) -> BackendResult<()> {
            self.0
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
        ) -> BackendResult<()> {
            self.0
                .start_execution_atomic(execution, started_event, scheduled_event, work_item)
                .await
        }
        async fn get_tool_effect(&self, key: &str) -> BackendResult<Option<serde_json::Value>> {
            self.0.get_tool_effect(key).await
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
            self.0
                .reserve_tool_effect(key, execution_id, node_id, owner, lease_fence, ttl)
                .await
        }
        async fn release_tool_reservation(
            &self,
            key: &str,
            owner: &str,
            lease_fence: i64,
        ) -> BackendResult<()> {
            self.0
                .release_tool_reservation(key, owner, lease_fence)
                .await
        }
        async fn put_artifact(
            &self,
            bytes: &[u8],
            media_type: Option<&str>,
        ) -> BackendResult<ArtifactRef> {
            self.0.put_artifact(bytes, media_type).await
        }
        async fn get_artifact(&self, hash: &str) -> BackendResult<Option<Vec<u8>>> {
            self.0.get_artifact(hash).await
        }
        async fn enqueue_work_item(&self, item: WorkItem) -> BackendResult<WorkItemId> {
            self.0.enqueue_work_item(item).await
        }
        async fn claim_work_item(
            &self,
            worker_id: &str,
            queue_types: &[&str],
        ) -> BackendResult<Option<WorkItem>> {
            self.0.claim_work_item(worker_id, queue_types).await
        }
        async fn renew_lease(
            &self,
            item_id: WorkItemId,
            worker_id: &str,
            lease_fence: i64,
        ) -> BackendResult<()> {
            self.0.renew_lease(item_id, worker_id, lease_fence).await
        }
        async fn claim_work_item_with_lease(
            &self,
            worker_id: &str,
            queue_types: &[&str],
            lease_secs: Option<u64>,
        ) -> BackendResult<Option<WorkItem>> {
            self.0
                .claim_work_item_with_lease(worker_id, queue_types, lease_secs)
                .await
        }
        async fn get_work_item(&self, item_id: WorkItemId) -> BackendResult<Option<WorkItem>> {
            self.0.get_work_item(item_id).await
        }
        async fn complete_work_item(&self, item_id: WorkItemId) -> BackendResult<bool> {
            self.0.complete_work_item(item_id).await
        }
        async fn complete_work_item_fenced(
            &self,
            item_id: WorkItemId,
            lease_fence: i64,
        ) -> BackendResult<bool> {
            self.0.complete_work_item_fenced(item_id, lease_fence).await
        }
        async fn fail_work_item(&self, item_id: WorkItemId, error: &str) -> BackendResult<()> {
            self.0.fail_work_item(item_id, error).await
        }
        async fn fail_work_item_fenced(
            &self,
            item_id: WorkItemId,
            lease_fence: i64,
            error: &str,
        ) -> BackendResult<Option<FailOutcome>> {
            self.0
                .fail_work_item_fenced(item_id, lease_fence, error)
                .await
        }
        async fn commit_turn(
            &self,
            item_id: WorkItemId,
            lease_fence: i64,
            terminal_event: Event,
            write_snapshot: bool,
        ) -> BackendResult<EventSequence> {
            self.0
                .commit_turn(item_id, lease_fence, terminal_event, write_snapshot)
                .await
        }
        async fn commit_turn_unless_terminal(
            &self,
            item_id: WorkItemId,
            lease_fence: i64,
            terminal_event: Event,
            write_snapshot: bool,
        ) -> BackendResult<TurnCommit> {
            self.0
                .commit_turn_unless_terminal(item_id, lease_fence, terminal_event, write_snapshot)
                .await
        }
        async fn withdraw_work_item_fenced(
            &self,
            item_id: WorkItemId,
            lease_fence: i64,
        ) -> BackendResult<bool> {
            self.0.withdraw_work_item_fenced(item_id, lease_fence).await
        }
        async fn park_work_item(
            &self,
            item_id: WorkItemId,
            lease_fence: i64,
            retry_after: &str,
            next_attempt: u32,
        ) -> BackendResult<bool> {
            self.0
                .park_work_item(item_id, lease_fence, retry_after, next_attempt)
                .await
        }
        async fn finalize_rollover_fenced(
            &self,
            execution_id: &ExecutionId,
            work_item_id: WorkItemId,
            lease_fence: i64,
        ) -> BackendResult<bool> {
            self.0
                .finalize_rollover_fenced(execution_id, work_item_id, lease_fence)
                .await
        }
        async fn reclaim_expired_leases(&self) -> BackendResult<ReclaimResult> {
            self.0.reclaim_expired_leases().await
        }
        async fn move_to_dead_letter(
            &self,
            item_id: WorkItemId,
            last_error: &str,
        ) -> BackendResult<()> {
            self.0.move_to_dead_letter(item_id, last_error).await
        }
        async fn create_token(&self, name: &str, role: &str) -> BackendResult<(String, ApiToken)> {
            self.0.create_token(name, role).await
        }
        async fn validate_token(&self, token: &str) -> BackendResult<Option<ApiToken>> {
            self.0.validate_token(token).await
        }
        async fn apply_approval_projection(
            &self,
            row: ApprovalProjectionRow,
            projection_name: &str,
            new_checkpoint: i64,
        ) -> BackendResult<()> {
            self.0
                .apply_approval_projection(row, projection_name, new_checkpoint)
                .await
        }
        async fn apply_approval_projection_batch(
            &self,
            rows: Vec<ApprovalProjectionRow>,
            projection_name: &str,
            execution_id: &ExecutionId,
            new_checkpoint: i64,
        ) -> BackendResult<()> {
            self.0
                .apply_approval_projection_batch(
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
            self.0.get_approval_projection(execution_id).await
        }
        async fn get_projector_checkpoint(
            &self,
            projection_name: &str,
            execution_id: &ExecutionId,
        ) -> BackendResult<i64> {
            self.0
                .get_projector_checkpoint(projection_name, execution_id)
                .await
        }
        async fn set_projector_checkpoint(
            &self,
            projection_name: &str,
            execution_id: &ExecutionId,
            new_checkpoint: i64,
        ) -> BackendResult<()> {
            self.0
                .set_projector_checkpoint(projection_name, execution_id, new_checkpoint)
                .await
        }
    }
}
