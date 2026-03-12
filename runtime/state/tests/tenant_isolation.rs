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
