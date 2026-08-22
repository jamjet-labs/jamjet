//! Tenant isolation integration tests.
//!
//! Verifies that data is properly partitioned between tenants:
//! tenants cannot see each other's workflows, executions, or events.

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::{StateBackend, WorkflowDefinition};
use jamjet_state::event::{Event, EventKind};
use jamjet_state::tenant::{Tenant, TenantId, TenantStatus};
use jamjet_state::SqliteBackend;
use serde_json::json;

/// Open an in-memory SQLite backend with migrations.
async fn open_test_db() -> SqliteBackend {
    SqliteBackend::open("sqlite::memory:")
        .await
        .expect("failed to open in-memory SQLite")
}

/// Register a tenant in the database so FK constraints are satisfied.
async fn register_tenant(db: &SqliteBackend, id: &str, name: &str) {
    let scoped = db.for_tenant(TenantId::default());
    let now = Utc::now();
    scoped
        .create_tenant(Tenant {
            id: TenantId::from(id),
            name: name.to_string(),
            status: TenantStatus::Active,
            policy: None,
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("failed to register tenant");
}

fn sample_execution(workflow_id: &str) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        execution_id: ExecutionId::new(),
        workflow_id: workflow_id.to_string(),
        workflow_version: "1.0.0".to_string(),
        status: WorkflowStatus::Pending,
        initial_input: json!({"x": 1}),
        current_state: json!({}),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    }
}

// ── Tenant CRUD ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_and_list_tenants() {
    let db = open_test_db().await;
    let scoped = db.for_tenant(TenantId::default());

    let now = Utc::now();
    let tenant = Tenant {
        id: TenantId::from("acme"),
        name: "Acme Corp".to_string(),
        status: TenantStatus::Active,
        policy: None,
        limits: None,
        created_at: now,
        updated_at: now,
    };
    scoped.create_tenant(tenant).await.unwrap();

    let tenants = scoped.list_tenants().await.unwrap();
    // "default" (from migration) + "acme"
    assert_eq!(tenants.len(), 2);
    let names: Vec<&str> = tenants.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"Acme Corp"));
}

#[tokio::test]
async fn get_and_update_tenant() {
    let db = open_test_db().await;
    let scoped = db.for_tenant(TenantId::default());

    let now = Utc::now();
    scoped
        .create_tenant(Tenant {
            id: TenantId::from("beta"),
            name: "Beta Inc".to_string(),
            status: TenantStatus::Active,
            policy: None,
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();

    let tenant = scoped
        .get_tenant(&TenantId::from("beta"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tenant.name, "Beta Inc");

    scoped
        .update_tenant(Tenant {
            id: TenantId::from("beta"),
            name: "Beta Corp".to_string(),
            status: TenantStatus::Suspended,
            policy: Some(json!({"blocked_tools": ["rm_rf"]})),
            limits: None,
            created_at: now,
            updated_at: Utc::now(),
        })
        .await
        .unwrap();

    let updated = scoped
        .get_tenant(&TenantId::from("beta"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.name, "Beta Corp");
    assert_eq!(updated.status, TenantStatus::Suspended);
    assert!(updated.policy.is_some());
}

// ── Workflow isolation ──────────────────────────────────────────────────────

#[tokio::test]
async fn workflow_definitions_are_tenant_isolated() {
    let db = open_test_db().await;
    register_tenant(&db, "alpha", "Alpha Corp").await;
    register_tenant(&db, "bravo", "Bravo Corp").await;
    let tenant_a = db.for_tenant(TenantId::from("alpha"));
    let tenant_b = db.for_tenant(TenantId::from("bravo"));

    // Tenant A stores a workflow
    let def_a = WorkflowDefinition {
        workflow_id: "shared-name".to_string(),
        version: "1.0.0".to_string(),
        ir: json!({"workflow_id": "shared-name", "owner": "alpha"}),
        created_at: Utc::now(),
        tenant_id: "alpha".to_string(),
    };
    tenant_a.store_workflow(def_a).await.unwrap();

    // Tenant B stores a workflow with the same id
    let def_b = WorkflowDefinition {
        workflow_id: "shared-name".to_string(),
        version: "1.0.0".to_string(),
        ir: json!({"workflow_id": "shared-name", "owner": "bravo"}),
        created_at: Utc::now(),
        tenant_id: "bravo".to_string(),
    };
    tenant_b.store_workflow(def_b).await.unwrap();

    // Tenant A only sees its own workflow
    let fetched_a = tenant_a
        .get_workflow("shared-name", "1.0.0")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched_a.ir["owner"], "alpha");

    // Tenant B only sees its own workflow
    let fetched_b = tenant_b
        .get_workflow("shared-name", "1.0.0")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched_b.ir["owner"], "bravo");
}

// ── Execution isolation ─────────────────────────────────────────────────────

#[tokio::test]
async fn executions_are_tenant_isolated() {
    let db = open_test_db().await;
    register_tenant(&db, "alpha", "Alpha Corp").await;
    register_tenant(&db, "bravo", "Bravo Corp").await;
    let tenant_a = db.for_tenant(TenantId::from("alpha"));
    let tenant_b = db.for_tenant(TenantId::from("bravo"));

    // Each tenant creates an execution
    let exec_a = sample_execution("wf-alpha");
    let id_a = exec_a.execution_id.clone();
    tenant_a.create_execution(exec_a).await.unwrap();

    let exec_b = sample_execution("wf-bravo");
    let id_b = exec_b.execution_id.clone();
    tenant_b.create_execution(exec_b).await.unwrap();

    // Tenant A can only see its own execution
    let list_a = tenant_a.list_executions(None, 10, 0).await.unwrap();
    assert_eq!(list_a.len(), 1);
    assert_eq!(list_a[0].workflow_id, "wf-alpha");

    // Tenant B can only see its own execution
    let list_b = tenant_b.list_executions(None, 10, 0).await.unwrap();
    assert_eq!(list_b.len(), 1);
    assert_eq!(list_b[0].workflow_id, "wf-bravo");

    // Cross-tenant access returns None
    assert!(tenant_a.get_execution(&id_b).await.unwrap().is_none());
    assert!(tenant_b.get_execution(&id_a).await.unwrap().is_none());
}

// ── Event isolation ─────────────────────────────────────────────────────────

#[tokio::test]
async fn events_are_tenant_isolated() {
    let db = open_test_db().await;
    register_tenant(&db, "alpha", "Alpha Corp").await;
    register_tenant(&db, "bravo", "Bravo Corp").await;
    let tenant_a = db.for_tenant(TenantId::from("alpha"));
    let tenant_b = db.for_tenant(TenantId::from("bravo"));

    let exec_a = sample_execution("wf-a");
    let id_a = exec_a.execution_id.clone();
    tenant_a.create_execution(exec_a).await.unwrap();

    // Append event for tenant A
    let event = Event::new(
        id_a.clone(),
        1,
        EventKind::WorkflowStarted {
            workflow_id: "wf-a".to_string(),
            workflow_version: "1.0.0".to_string(),
            initial_input: json!({}),
        },
    );
    tenant_a.append_event(event).await.unwrap();

    // Tenant A can see its events
    let events_a = tenant_a.get_events(&id_a).await.unwrap();
    assert_eq!(events_a.len(), 1);

    // Tenant B cannot see tenant A's events
    let events_b = tenant_b.get_events(&id_a).await.unwrap();
    assert_eq!(events_b.len(), 0);

    // Latest sequence respects tenant scope
    let seq_a = tenant_a.latest_sequence(&id_a).await.unwrap();
    assert_eq!(seq_a, 1);

    let seq_b = tenant_b.latest_sequence(&id_a).await.unwrap();
    assert_eq!(seq_b, 0);
}

// ── Work item isolation ─────────────────────────────────────────────────────

#[tokio::test]
async fn work_items_are_tenant_isolated() {
    let db = open_test_db().await;
    register_tenant(&db, "alpha", "Alpha Corp").await;
    register_tenant(&db, "bravo", "Bravo Corp").await;
    let tenant_a = db.for_tenant(TenantId::from("alpha"));
    let tenant_b = db.for_tenant(TenantId::from("bravo"));

    let exec_a = sample_execution("wf-a");
    let id_a = exec_a.execution_id.clone();
    tenant_a.create_execution(exec_a).await.unwrap();

    // Enqueue work for tenant A
    let item = jamjet_state::WorkItem {
        id: uuid::Uuid::new_v4(),
        execution_id: id_a,
        node_id: "node-1".to_string(),
        queue_type: "general".to_string(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: "alpha".to_string(),
    };
    tenant_a.enqueue_work_item(item).await.unwrap();

    // Tenant A can claim the work
    let claimed = tenant_a
        .claim_work_item("worker-1", &["general"])
        .await
        .unwrap();
    assert!(claimed.is_some());

    // Re-enqueue for tenant A
    let item2 = jamjet_state::WorkItem {
        id: uuid::Uuid::new_v4(),
        execution_id: ExecutionId::new(),
        node_id: "node-2".to_string(),
        queue_type: "general".to_string(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: "alpha".to_string(),
    };
    // Create execution first for the foreign key
    let exec_a2 = sample_execution("wf-a2");
    let id_a2 = exec_a2.execution_id.clone();
    tenant_a.create_execution(exec_a2).await.unwrap();
    let mut item2 = item2;
    item2.execution_id = id_a2;
    tenant_a.enqueue_work_item(item2).await.unwrap();

    // Tenant B cannot see tenant A's work items
    let claimed_b = tenant_b
        .claim_work_item("worker-2", &["general"])
        .await
        .unwrap();
    assert!(claimed_b.is_none());
}

// ── Default tenant backward compatibility ───────────────────────────────────

#[tokio::test]
async fn default_tenant_backward_compatible() {
    let db = open_test_db().await;

    // Using the unscoped SqliteBackend (original code path)
    let def = WorkflowDefinition {
        workflow_id: "legacy-wf".to_string(),
        version: "1.0.0".to_string(),
        ir: json!({"workflow_id": "legacy-wf"}),
        created_at: Utc::now(),
        tenant_id: "default".to_string(),
    };
    db.store_workflow(def).await.unwrap();

    // A scoped backend with "default" tenant can see it
    let default_scoped = db.for_tenant(TenantId::default());
    let fetched = default_scoped
        .get_workflow("legacy-wf", "1.0.0")
        .await
        .unwrap();
    assert!(fetched.is_some());

    // A scoped backend with a different tenant cannot
    let other = db.for_tenant(TenantId::from("other"));
    let not_found = other.get_workflow("legacy-wf", "1.0.0").await.unwrap();
    assert!(not_found.is_none());
}

// ── Token tenant attribution ────────────────────────────────────────────────

#[tokio::test]
async fn tokens_carry_tenant_id() {
    let db = open_test_db().await;
    register_tenant(&db, "acme", "Acme Corp").await;
    let scoped = db.for_tenant(TenantId::from("acme"));

    let (plaintext, info) = scoped.create_token("dev-token", "developer").await.unwrap();
    assert_eq!(info.tenant_id, "acme");

    // Validate returns the correct tenant
    let validated = scoped.validate_token(&plaintext).await.unwrap().unwrap();
    assert_eq!(validated.tenant_id, "acme");
}

// ── Fenced failure, tenant-scoped ────────────────────────────────────────────

/// The tenant-scoped dead-letter row must store the execution id in the SAME
/// format every other row uses: the bare UUID from `execution_id_str`, not
/// `ExecutionId`'s `Display`, which renders `exec_<simple>`.
///
/// Getting this wrong is silent. The insert succeeds, the item is correctly
/// dead-lettered, and every assertion about status passes — but the row joins to
/// nothing, so an operator listing an execution's dead-lettered work finds an
/// empty result and concludes nothing failed. Caught in review on #118, which is
/// why the assertion is on the STORED STRING rather than on a round-trip that
/// would format both sides the same way and agree with itself.
#[tokio::test]
async fn scoped_dead_letter_stores_the_bare_execution_uuid() {
    let db = open_test_db().await;
    register_tenant(&db, "alpha", "Alpha").await;
    let tenant = db.for_tenant(TenantId::from("alpha"));

    let exec = sample_execution("wf-dl");
    let execution_id = exec.execution_id.clone();
    tenant.create_execution(exec).await.unwrap();

    let item_id = uuid::Uuid::new_v4();
    tenant
        .enqueue_work_item(jamjet_state::WorkItem {
            id: item_id,
            execution_id: execution_id.clone(),
            node_id: "node-dl".to_string(),
            queue_type: "general".to_string(),
            payload: json!({}),
            // One attempt short of the cap, so failing it exhausts the budget
            // and takes the dead-letter branch.
            attempt: 2,
            max_attempts: 3,
            created_at: Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            lease_fence: 0,
            tenant_id: "alpha".to_string(),
        })
        .await
        .unwrap();

    let claimed = tenant
        .claim_work_item("worker-dl", &["general"])
        .await
        .unwrap()
        .expect("the item must be claimable");

    let outcome = tenant
        .fail_work_item_fenced(item_id, claimed.lease_fence, "tool exploded")
        .await
        .unwrap()
        .expect("a matching fence must settle the item");
    assert!(
        matches!(
            outcome,
            jamjet_state::backend::FailOutcome::Exhausted { .. }
        ),
        "attempt 3 of 3 is spent, so this must dead-letter"
    );

    let stored: String =
        sqlx::query_scalar("SELECT execution_id FROM dead_letter_items WHERE id = ?")
            .bind(item_id.to_string())
            .fetch_one(&db.pool())
            .await
            .expect("the dead-letter row must exist");

    assert_eq!(
        stored,
        execution_id.0.to_string(),
        "the dead-letter row must store the bare UUID, like every other \
         execution_id column; Display would write exec_<simple> and join to nothing"
    );
    assert_ne!(
        stored,
        execution_id.to_string(),
        "Display is the wrong format here — this assertion is the regression guard"
    );
}

/// A tenant must not be able to fail another tenant's work item, even holding a
/// correct id and fence.
#[tokio::test]
async fn scoped_fail_cannot_reach_another_tenants_item() {
    let db = open_test_db().await;
    register_tenant(&db, "alpha", "Alpha").await;
    register_tenant(&db, "beta", "Beta").await;
    let tenant_a = db.for_tenant(TenantId::from("alpha"));
    let tenant_b = db.for_tenant(TenantId::from("beta"));

    let exec = sample_execution("wf-x");
    let execution_id = exec.execution_id.clone();
    tenant_a.create_execution(exec).await.unwrap();

    let item_id = uuid::Uuid::new_v4();
    tenant_a
        .enqueue_work_item(jamjet_state::WorkItem {
            id: item_id,
            execution_id,
            node_id: "node-x".to_string(),
            queue_type: "general".to_string(),
            payload: json!({}),
            attempt: 0,
            max_attempts: 3,
            created_at: Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            lease_fence: 0,
            tenant_id: "alpha".to_string(),
        })
        .await
        .unwrap();

    let claimed = tenant_a
        .claim_work_item("worker-a", &["general"])
        .await
        .unwrap()
        .expect("claimable");

    // Beta knows the id and the real fence and still cannot touch it.
    let cross = tenant_b
        .fail_work_item_fenced(item_id, claimed.lease_fence, "not yours")
        .await
        .unwrap();
    assert!(
        cross.is_none(),
        "a scoped backend must never settle another tenant's item"
    );

    // Alpha still can, which proves the refusal was tenancy and not a bad fence.
    assert!(tenant_a
        .fail_work_item_fenced(item_id, claimed.lease_fence, "mine")
        .await
        .unwrap()
        .is_some());
}

// ── The tenant-scoped backend must match the untenanted one ──────────────────

/// The tenant-scoped `reclaim_expired_leases` must not resurrect a settled item.
///
/// This is the same duplicate-execution bug fixed for the untenanted backend,
/// which was fixed in ONE of the two copies: the scoped reclaim kept
/// `WHERE id = ?` with no `status` guard and no `tenant_id` on the write. A
/// worker committing inside the sweep's update loop had its COMPLETED item
/// flipped back to pending, re-claimed, and re-run.
///
/// Needs volume for the same reason as its untenanted twin: a settle done
/// BEFORE the call also changes `status`, which the sweep's own SELECT filters
/// out, so nothing races. Verified to fail against the unguarded UPDATE.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scoped_reclaim_cannot_resurrect_a_settled_item() {
    const ITEMS: usize = 400;

    let db = std::sync::Arc::new(open_test_db().await);
    register_tenant(&db, "alpha", "Alpha").await;
    let tenant = std::sync::Arc::new(db.for_tenant(TenantId::from("alpha")));

    let exec = sample_execution("wf-race");
    let execution_id = exec.execution_id.clone();
    tenant.create_execution(exec).await.unwrap();

    for _ in 0..ITEMS {
        tenant
            .enqueue_work_item(jamjet_state::WorkItem {
                id: uuid::Uuid::new_v4(),
                execution_id: execution_id.clone(),
                node_id: "n1".to_string(),
                queue_type: "general".to_string(),
                payload: json!({}),
                attempt: 0,
                max_attempts: 3,
                created_at: Utc::now(),
                lease_expires_at: None,
                worker_id: None,
                lease_fence: 0,
                tenant_id: "alpha".to_string(),
            })
            .await
            .unwrap();
    }

    let mut claimed = Vec::with_capacity(ITEMS);
    for _ in 0..ITEMS {
        let c = tenant
            .claim_work_item("worker-slow", &["general"])
            .await
            .unwrap()
            .expect("claim");
        claimed.push((c.id, c.lease_fence));
    }
    // Expire every lease so the sweep captures all of them.
    sqlx::query("UPDATE work_items SET lease_expires_at = '2020-01-01T00:00:00+00:00'")
        .execute(&db.pool())
        .await
        .unwrap();

    // Settle the LAST items the sweep will reach, while it works through the rest.
    let victims: Vec<(uuid::Uuid, i64)> = claimed.iter().rev().take(40).copied().collect();
    let settler = {
        let tenant = tenant.clone();
        tokio::spawn(async move {
            let mut settled = Vec::new();
            for (id, fence) in victims {
                if tenant.complete_work_item_fenced(id, fence).await.unwrap() {
                    settled.push(id);
                }
            }
            settled
        })
    };

    let reclaimed = tenant.reclaim_expired_leases().await.unwrap();
    let settled = settler.await.unwrap();

    let reported: std::collections::HashSet<uuid::Uuid> = reclaimed
        .retryable
        .iter()
        .chain(reclaimed.exhausted.iter())
        .map(|i| i.id)
        .collect();
    for id in &settled {
        assert!(
            !reported.contains(id),
            "a COMPLETED item was reported as reclaimed ({id}) — the caller will \
             emit NodeFailed for a node that succeeded"
        );
        let status: String = sqlx::query_scalar("SELECT status FROM work_items WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(&db.pool())
            .await
            .unwrap();
        assert_eq!(
            status, "completed",
            "a COMPLETED item was resurrected to {status:?} ({id}) — it will re-run \
             and fire its side effect twice"
        );
    }
    assert!(
        !settled.is_empty(),
        "the race never opened; the test proved nothing"
    );
}

/// The scoped claim path must consume attempts too, or a node that kills its
/// worker loops forever — the same infinite-retry bug fixed for the untenanted
/// backend, in the copy that was missed.
#[tokio::test]
async fn scoped_claim_side_expiry_consumes_attempts() {
    let db = open_test_db().await;
    register_tenant(&db, "alpha", "Alpha").await;
    let tenant = db.for_tenant(TenantId::from("alpha"));

    let exec = sample_execution("wf-attempts");
    let execution_id = exec.execution_id.clone();
    tenant.create_execution(exec).await.unwrap();

    let item_id = uuid::Uuid::new_v4();
    tenant
        .enqueue_work_item(jamjet_state::WorkItem {
            id: item_id,
            execution_id,
            node_id: "n1".to_string(),
            queue_type: "general".to_string(),
            payload: json!({}),
            attempt: 0,
            max_attempts: 3,
            created_at: Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            lease_fence: 0,
            tenant_id: "alpha".to_string(),
        })
        .await
        .unwrap();

    let mut seen = Vec::new();
    for _ in 0..3 {
        let Some(c) = tenant.claim_work_item("dies", &["general"]).await.unwrap() else {
            break;
        };
        seen.push(c.attempt);
        sqlx::query(
            "UPDATE work_items SET lease_expires_at = '2020-01-01T00:00:00+00:00' WHERE id = ?",
        )
        .bind(item_id.to_string())
        .execute(&db.pool())
        .await
        .unwrap();
    }
    assert_eq!(
        seen,
        vec![0, 1, 2],
        "each claim-side expiry must consume exactly one attempt"
    );
    assert!(
        tenant
            .claim_work_item("dies", &["general"])
            .await
            .unwrap()
            .is_none(),
        "an item that has spent its attempts must not be handed out again"
    );
}

/// One tenant's reservation must never be handed to another.
///
/// The reservation table originally had a GLOBAL `idempotency_key` primary key.
/// A scoped upsert then conflicted on a row it could not see, affected zero
/// rows, and its tenant-filtered lookup found nothing — which the code read as
/// "free" and returned `Acquired`. Tenant B was told it owned a key tenant A
/// held, and would have run the effect.
///
/// Both halves are asserted: B is refused, and A can still take its own key in
/// the first place.
#[tokio::test]
async fn a_reservation_is_scoped_to_its_tenant() {
    let db = open_test_db().await;
    register_tenant(&db, "alpha", "Alpha").await;
    register_tenant(&db, "beta", "Beta").await;
    let a = db.for_tenant(TenantId::from("alpha"));
    let b = db.for_tenant(TenantId::from("beta"));

    let eid_a = ExecutionId::new();
    let eid_b = ExecutionId::new();
    let ttl = std::time::Duration::from_secs(300);

    assert_eq!(
        a.reserve_tool_effect("shared-key", &eid_a, "n1", "worker-A", 1, ttl)
            .await
            .unwrap(),
        jamjet_state::backend::ReserveOutcome::Acquired
    );

    // Beta asks for the same key string. It is a DIFFERENT reservation, so it
    // must be grantable — the key is scoped, not globally exclusive.
    assert_eq!(
        b.reserve_tool_effect("shared-key", &eid_b, "n1", "worker-B", 1, ttl)
            .await
            .unwrap(),
        jamjet_state::backend::ReserveOutcome::Acquired,
        "a key is scoped per tenant; beta's reservation is not alpha's"
    );

    // ...and within a tenant it still excludes a second worker.
    assert!(
        matches!(
            b.reserve_tool_effect("shared-key", &eid_b, "n1", "worker-C", 2, ttl)
                .await
                .unwrap(),
            jamjet_state::backend::ReserveOutcome::Held { .. }
        ),
        "beta's own key must still be exclusive within beta"
    );
}
