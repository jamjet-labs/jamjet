//! `list_pending_approvals` on the Postgres store: every waiting approval
//! across runs, in one keyset-paged list. The suite has no case for it; these
//! are `state/tests/approvals_across_runs.rs`'s cases, on the unscoped store
//! and through a tenant's view.
//!
//! Who owns a row and whether its run is live come from the run's own row,
//! never from the projection's columns. The order is the time a row was
//! projected, then the run, then the node, bytewise as SQLite compares it,
//! and the list reads it from an index rather than sorting every pending row.

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    ApprovalCursor, ApprovalProjectionRow, PendingApprovalRow, PgBackend, PgSettings, StateBackend,
    Tenant, TenantId, TenantStatus,
};
use jamjet_testkit::pg::{TestDatabase, POOL_SIZE};
use serde_json::json;
use sqlx::{Connection, PgConnection};
use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

fn settings() -> PgSettings {
    PgSettings {
        pool_size: POOL_SIZE,
        ..PgSettings::default()
    }
}

async fn store(db: &TestDatabase) -> PgBackend {
    PgBackend::connect(db.url(), settings())
        .await
        .unwrap_or_else(|e| panic!("connect to the test database: {e}"))
}

async fn register(b: &PgBackend, id: &str) {
    let now = Utc::now();
    b.for_tenant(TenantId::from(id))
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

/// One store, as a case sees it: what the code under test reads through,
/// and the engine's unscoped handle, which the projector writes through.
struct Fixture {
    backend: PgBackend,
    engine: PgBackend,
    db: TestDatabase,
}

async fn unscoped() -> Option<Fixture> {
    let db = TestDatabase::fresh().await?;
    let b = store(&db).await;
    Some(Fixture {
        backend: b.clone(),
        engine: b,
        db,
    })
}

/// Seen through `acme`, whose runs these are; the rows are written unscoped.
async fn scoped() -> Option<Fixture> {
    let db = TestDatabase::fresh().await?;
    let b = store(&db).await;
    register(&b, "acme").await;
    Some(Fixture {
        backend: b.for_tenant(TenantId::from("acme")),
        engine: b,
        db,
    })
}

/// Each case once unscoped and once through a tenant's view.
macro_rules! on_both {
    ($($case:ident),* $(,)?) => {
        mod unscoped {
            $(#[tokio::test]
            async fn $case() {
                let Some(fx) = super::unscoped().await else {
                    return jamjet_testkit::skip(stringify!($case));
                };
                super::$case(&fx).await;
            })*
        }
        mod scoped {
            $(#[tokio::test]
            async fn $case() {
                let Some(fx) = super::scoped().await else {
                    return jamjet_testkit::skip(stringify!($case));
                };
                super::$case(&fx).await;
            })*
        }
    };
}

on_both!(
    pending_rows_of_live_runs_are_listed_oldest_first,
    a_cancelled_or_failed_runs_open_request_is_not_listed,
    paging_by_cursor_returns_each_row_once,
    nodes_sort_bytewise,
    a_batch_with_two_rows_of_one_node_keeps_the_later,
);

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

/// Three live runs, one of them paused, each with one waiting request,
/// projected in an order that is neither the runs' id order nor the nodes'
/// name order: the list follows the projection time. A decided row and a
/// live run with no request add nothing.
async fn pending_rows_of_live_runs_are_listed_oldest_first(fx: &Fixture) {
    let mut workflows = HashMap::new();
    let mut runs = Vec::new();
    for (wf, status) in [
        ("wf-one", WorkflowStatus::Running),
        ("wf-paused", WorkflowStatus::Paused),
        ("wf-two", WorkflowStatus::Running),
    ] {
        let id = start(&fx.backend, wf, status).await;
        workflows.insert(id.0.to_string(), wf);
        runs.push(id);
    }
    start(&fx.backend, "wf-idle", WorkflowStatus::Running).await;
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

/// A run that ended with its request open keeps a `pending` row. The list
/// leaves it out, whichever way the run ended.
async fn a_cancelled_or_failed_runs_open_request_is_not_listed(fx: &Fixture) {
    let live = start(&fx.backend, "wf", WorkflowStatus::Running).await;
    project(fx, &live, vec![pending_row(&live, "review", 3)]).await;
    let mut ended = Vec::new();
    for status in [
        WorkflowStatus::Cancelled,
        WorkflowStatus::Failed,
        WorkflowStatus::Completed,
        WorkflowStatus::LimitExceeded,
    ] {
        let exec = start(&fx.backend, "wf", WorkflowStatus::Running).await;
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
        let exec = start(&fx.backend, "wf", WorkflowStatus::Running).await;
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

/// One run's rows share one time, so they come in node order, compared
/// bytewise as SQLite compares text: capitals before lower case, `_` between
/// them. The column is given a collation that orders text as a dictionary
/// does first, so an order left to the column's collation would differ.
/// Paged one row at a time, the cursor walks them in that order too.
async fn nodes_sort_bytewise(fx: &Fixture) {
    sqlx::query("ALTER TABLE proj_approvals ALTER COLUMN node_id TYPE text COLLATE \"en-x-icu\"")
        .execute(&fx.db.pool().await)
        .await
        .unwrap();
    let exec = start(&fx.backend, "wf", WorkflowStatus::Running).await;
    let nodes = ["b", "_c", "B", "a", "A_"];
    let rows = nodes.iter().map(|n| pending_row(&exec, n, 5)).collect();
    project(fx, &exec, rows).await;
    let mut bytewise: Vec<&str> = nodes.to_vec();
    bytewise.sort_unstable();
    assert_eq!(bytewise, ["A_", "B", "_c", "a", "b"]);

    let listed: Vec<String> = fx
        .backend
        .list_pending_approvals(None, 50)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.node_id)
        .collect();
    assert_eq!(listed, bytewise, "the list's order");
    let mut walked = Vec::new();
    let mut after: Option<ApprovalCursor> = None;
    loop {
        let page = fx
            .backend
            .list_pending_approvals(after.as_ref(), 1)
            .await
            .unwrap();
        let Some(last) = page.last() else { break };
        after = Some(ApprovalCursor::from(last));
        walked.push(last.node_id.clone());
        assert!(walked.len() <= nodes.len(), "the cursor went round");
    }
    assert_eq!(walked, bytewise, "the cursor's order");
    let projected: Vec<String> = fx
        .engine
        .get_approval_projection(&exec)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.node_id)
        .collect();
    assert_eq!(projected, bytewise, "get_approval_projection's order");
}

/// A batch that holds two rows of one node writes the later one, as one
/// upsert after another would, beside the batch's other rows.
async fn a_batch_with_two_rows_of_one_node_keeps_the_later(fx: &Fixture) {
    let exec = start(&fx.backend, "wf", WorkflowStatus::Running).await;
    let mut decided = pending_row(&exec, "gate", 7);
    decided.status = "approved".into();
    decided.user_id = Some("alice".into());
    let rows = vec![
        pending_row(&exec, "gate", 5),
        pending_row(&exec, "other", 6),
        decided,
    ];
    fx.engine
        .apply_approval_projection_batch(rows, "approvals", &exec, 7)
        .await
        .unwrap();
    let projected = fx.backend.get_approval_projection(&exec).await.unwrap();
    let got: Vec<(&str, &str, i64)> = projected
        .iter()
        .map(|r| (r.node_id.as_str(), r.status.as_str(), r.last_sequence))
        .collect();
    assert_eq!(got, [("gate", "approved", 7), ("other", "pending", 6)]);
    assert_eq!(projected[0].user_id.as_deref(), Some("alice"));
    let checkpoint = fx
        .backend
        .get_projector_checkpoint("approvals", &exec)
        .await
        .unwrap();
    assert_eq!(checkpoint, 7);
}

// ── Tenants ───────────────────────────────────────────────────────────────────

/// Each tenant's rows are listed for it alone, and the unscoped store lists
/// every tenant's.
#[tokio::test]
async fn a_scoped_backend_lists_its_tenants_rows_only() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("a_scoped_backend_lists_its_tenants_rows_only");
    };
    let base = store(&db).await;
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
        assert_eq!(keys(&rows), vec![want.clone()]);
    }
    let unscoped = base.list_pending_approvals(None, 50).await.unwrap();
    let mut got = keys(&unscoped);
    got.sort();
    expected.sort();
    assert_eq!(got, expected, "the unscoped backend lists every tenant's");
}

/// An acme run's row, written by the unscoped projector, carries acme, the
/// run's tenant. Moved to `default` by hand, it is still listed for acme,
/// whose run it is, and not for the default tenant: the list judges by the
/// run's row.
#[tokio::test]
async fn the_list_judges_a_row_by_its_run() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("the_list_judges_a_row_by_its_run");
    };
    let base = store(&db).await;
    register(&base, "acme").await;
    let acme = base.for_tenant(TenantId::from("acme"));
    let default = base.for_tenant(TenantId::default());
    let pool = db.pool().await;

    let exec = start(&acme, "wf", WorkflowStatus::Running).await;
    base.apply_approval_projection(pending_row(&exec, "review", 3), "approvals", 3)
        .await
        .unwrap();
    let stored: String =
        sqlx::query_scalar("SELECT tenant_id FROM proj_approvals WHERE execution_id = $1")
            .bind(exec.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, "acme", "the row carries its run's tenant");

    sqlx::query("UPDATE proj_approvals SET tenant_id = 'default' WHERE execution_id = $1")
        .bind(exec.0)
        .execute(&pool)
        .await
        .unwrap();
    let rows = acme.list_pending_approvals(None, 50).await.unwrap();
    assert_eq!(keys(&rows), vec![key(&exec, "review")]);
    let rows = default.list_pending_approvals(None, 50).await.unwrap();
    assert!(rows.is_empty(), "listed for the default tenant: {rows:?}");
}

// ── The index ─────────────────────────────────────────────────────────────────

/// Each page of the list comes from `proj_approvals_pending` in order, with
/// and without a tenant and a cursor, not from sorting every pending row.
/// The plan is read with sequential scans turned off, since the tables are
/// small, and must hold no sort.
#[tokio::test]
async fn the_pending_list_reads_its_index_in_order() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("the_pending_list_reads_its_index_in_order");
    };
    let mut conn = PgConnection::connect(db.url()).await.unwrap();
    sqlx::query("SET enable_seqscan = off")
        .execute(&mut conn)
        .await
        .unwrap();
    for (tenant, cursor) in [(false, false), (true, false), (false, true), (true, true)] {
        let sql = format!(
            "EXPLAIN (FORMAT TEXT) \
             SELECT p.execution_id, e.workflow_id, e.workflow_version, p.node_id, p.tool_name, \
                    p.approver, p.context_json, p.last_sequence, p.updated_at \
             FROM proj_approvals p JOIN workflow_executions e ON e.execution_id = p.execution_id \
             WHERE p.status = 'pending' AND e.status IN ('running', 'paused'){}{} \
             ORDER BY p.updated_at, p.execution_id, p.node_id COLLATE \"C\" LIMIT 100",
            if tenant {
                " AND e.tenant_id = 'acme'"
            } else {
                ""
            },
            if cursor {
                " AND (p.updated_at, p.execution_id, p.node_id COLLATE \"C\") \
                 > (statement_timestamp(), gen_random_uuid(), 'n')"
            } else {
                ""
            },
        );
        let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .fetch_all(&mut conn)
            .await
            .unwrap();
        let plan = plan.join("\n");
        assert!(
            plan.contains("proj_approvals_pending"),
            "tenant {tenant}, cursor {cursor}: does not use the index:\n{plan}"
        );
        assert!(
            !plan.contains("Sort"),
            "tenant {tenant}, cursor {cursor}: sorts:\n{plan}"
        );
    }
}
