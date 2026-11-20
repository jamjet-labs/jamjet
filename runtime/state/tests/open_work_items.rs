//! `open_work_items`: a run's pending and claimed items, the ones still to
//! run or running.
//!
//! On a scoped backend the run's own row decides whose items they are. The
//! engine enqueues every item after the first through its unscoped handle,
//! so an item's own tenant column says `default` whoever's run it belongs to.
//!
//! The shared case runs on memory, SQLite and tenant-scoped SQLite. The
//! tenant cases need a scoped store, so they run on SQLite only.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::StateBackend;
use jamjet_state::tenant::{Tenant, TenantId, TenantStatus};
use jamjet_state::{InMemoryBackend, SqliteBackend, WorkItem};
use serde_json::json;
use uuid::Uuid;

// ── Fixtures ──────────────────────────────────────────────────────────────────

/// One store, as a case sees it.
struct Fixture {
    /// What the code under test reads through, as a route does.
    backend: Arc<dyn StateBackend>,
    /// The engine's unscoped handle on the same store: the scheduler enqueues
    /// and the worker claims and settles through it.
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

/// Scoped to `acme`. Its runs are acme's; their items are enqueued unscoped,
/// as the scheduler enqueues them.
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

mod memory {
    #[tokio::test]
    async fn open_work_items_lists_pending_and_claimed_only() {
        super::open_work_items_lists_pending_and_claimed_only(&super::memory().await).await;
    }
}

mod sqlite {
    #[tokio::test]
    async fn open_work_items_lists_pending_and_claimed_only() {
        super::open_work_items_lists_pending_and_claimed_only(&super::sqlite().await).await;
    }
}

mod scoped_sqlite {
    #[tokio::test]
    async fn open_work_items_lists_pending_and_claimed_only() {
        super::open_work_items_lists_pending_and_claimed_only(&super::scoped_sqlite().await).await;
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn start(b: &dyn StateBackend) -> ExecutionId {
    let now = Utc::now();
    let id = ExecutionId::new();
    b.create_execution(WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf".into(),
        workflow_version: "1.0.0".into(),
        status: WorkflowStatus::Running,
        initial_input: json!({}),
        current_state: json!({}),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    })
    .await
    .unwrap();
    id
}

/// An item of `exec` on its own queue, so a claim can name it. `n` orders
/// the items by creation.
fn item(exec: &ExecutionId, node: &str, queue: &str, t0: DateTime<Utc>, n: i64) -> WorkItem {
    WorkItem {
        id: Uuid::new_v4(),
        execution_id: exec.clone(),
        node_id: node.into(),
        queue_type: queue.into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: t0 + chrono::Duration::seconds(n),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: "default".into(),
    }
}

async fn item_tenant(base: &SqliteBackend, id: Uuid) -> String {
    sqlx::query_scalar("SELECT tenant_id FROM work_items WHERE id = ?")
        .bind(id.to_string())
        .fetch_one(&base.pool())
        .await
        .unwrap()
}

// ── Shared case ───────────────────────────────────────────────────────────────

/// One item waiting, one claimed, one completed, one failed, one
/// dead-lettered, and another run's item waiting. Only the first two are
/// the run's open items, in the order they were created.
async fn open_work_items_lists_pending_and_claimed_only(fx: &Fixture) {
    let exec = start(&*fx.backend).await;
    let other = start(&*fx.backend).await;
    let t0 = Utc::now();
    let waiting = item(&exec, "waiting", "python_fn", t0, 0);
    let held = item(&exec, "held", "held_q", t0, 1);
    let done = item(&exec, "done", "done_q", t0, 2);
    let failed = item(&exec, "failed", "failed_q", t0, 3);
    let dead = item(&exec, "dead", "dead_q", t0, 4);
    let elsewhere = item(&other, "waiting", "python_fn", t0, 5);
    for i in [&waiting, &held, &done, &failed, &dead, &elsewhere] {
        fx.engine.enqueue_work_item(i.clone()).await.unwrap();
    }

    let claimed = fx
        .engine
        .claim_work_item_with_lease("worker-1", &["held_q"], Some(60))
        .await
        .unwrap()
        .expect("the held item is claimable");
    assert_eq!(claimed.id, held.id);
    let settled = fx
        .engine
        .claim_work_item_with_lease("worker-1", &["done_q"], Some(60))
        .await
        .unwrap()
        .expect("the done item is claimable");
    assert!(fx
        .engine
        .complete_work_item_fenced(settled.id, settled.lease_fence)
        .await
        .unwrap());
    fx.engine.fail_work_item(failed.id, "boom").await.unwrap();
    fx.engine
        .move_to_dead_letter(dead.id, "attempts spent")
        .await
        .unwrap();

    let open = fx.backend.open_work_items(&exec).await.unwrap();
    let got: Vec<_> = open
        .iter()
        .map(|o| (o.id, o.node_id.as_str(), o.queue_type.as_str(), o.claimed))
        .collect();
    assert_eq!(
        got,
        vec![
            (waiting.id, "waiting", "python_fn", false),
            (held.id, "held", "held_q", true),
        ]
    );
    assert_eq!(open[0].lease_expires_at, None);
    assert!(
        open[1].lease_expires_at.is_some_and(|t| t > Utc::now()),
        "the claim's lease: {:?}",
        open[1].lease_expires_at
    );
}

// ── Tenants (scoped SQLite) ───────────────────────────────────────────────────

/// An acme run's items, one enqueued under acme and one unscoped as the
/// scheduler does: another tenant sees none of them, the default tenant
/// included, whose name the unscoped item carries.
#[tokio::test]
async fn open_work_items_of_another_tenants_run_is_empty() {
    let base = open().await;
    register(&base, "acme").await;
    register(&base, "globex").await;
    let acme = base.for_tenant(TenantId::from("acme"));
    let globex = base.for_tenant(TenantId::from("globex"));
    let default = base.for_tenant(TenantId::default());

    let exec = start(&acme).await;
    let t0 = Utc::now();
    let first = item(&exec, "first", "python_fn", t0, 0);
    let later = item(&exec, "later", "python_fn", t0, 1);
    acme.enqueue_work_item(first.clone()).await.unwrap();
    base.enqueue_work_item(later.clone()).await.unwrap();
    assert_eq!(item_tenant(&base, later.id).await, "default");

    assert_eq!(acme.open_work_items(&exec).await.unwrap().len(), 2);
    for scope in [&globex, &default] {
        let open = scope.open_work_items(&exec).await.unwrap();
        assert!(open.is_empty(), "{} sees {open:?}", scope.tenant_id());
    }
}

/// Items the scheduler enqueued unscoped carry `default` in their own
/// tenant column. They are still the acme run's, and acme lists them.
#[tokio::test]
async fn a_scoped_backend_lists_its_runs_items_stored_as_default() {
    let base = open().await;
    register(&base, "acme").await;
    let acme = base.for_tenant(TenantId::from("acme"));

    let exec = start(&acme).await;
    let t0 = Utc::now();
    let first = item(&exec, "first", "python_fn", t0, 0);
    let second = item(&exec, "second", "python_fn", t0, 1);
    for i in [&first, &second] {
        base.enqueue_work_item(i.clone()).await.unwrap();
        assert_eq!(item_tenant(&base, i.id).await, "default");
    }

    let open = acme.open_work_items(&exec).await.unwrap();
    assert_eq!(
        open.iter().map(|o| o.id).collect::<Vec<_>>(),
        vec![first.id, second.id]
    );
}
