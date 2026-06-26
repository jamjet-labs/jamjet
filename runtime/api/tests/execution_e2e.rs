//! End-to-end execution regression test.
//!
//! Submitting a workflow must drive it to a terminal state. This guards the
//! scheduler + worker-pool wiring and the scheduler's completion detection,
//! which previously left every execution stuck in `running` forever (the worker
//! pool was never spawned and no code emitted `WorkflowCompleted`).

use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::{StateBackend, WorkItem, WorkflowDefinition};
use jamjet_state::event::EventKind;
use jamjet_state::{Event, SqliteBackend, DEFAULT_TENANT};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_runs_to_completion() {
    // A temp-file SQLite DB shared across the scheduler, worker pool, and this
    // test. (`:memory:` would give each pool connection its own database, so it
    // cannot be shared.)
    let db_path = std::env::temp_dir().join(format!("jjtest-{}.db", Uuid::new_v4()));
    let url = format!("sqlite://{}", db_path.display());
    let backend: Arc<dyn StateBackend> = Arc::new(
        SqliteBackend::open(&url)
            .await
            .expect("open sqlite backend"),
    );

    // A trivial single-node workflow: a condition node that terminates (edge to
    // the "end" sentinel). The worker runs it via the no-op stub, after which the
    // scheduler must detect completion.
    let ir = serde_json::json!({
        "workflow_id": "e2e-completion",
        "version": "0.1.0",
        "name": null,
        "description": null,
        "state_schema": "",
        "start_node": "route",
        "nodes": {
            "route": {
                "id": "route",
                "kind": { "type": "condition", "branches": [] },
                "retry_policy": null,
                "node_timeout_secs": null,
                "description": null,
                "labels": {}
            }
        },
        "edges": [ { "from": "route", "to": "end", "condition": null } ],
        "retry_policies": {},
        "timeouts": {},
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {}
    });

    backend
        .store_workflow(WorkflowDefinition {
            workflow_id: "e2e-completion".into(),
            version: "0.1.0".into(),
            ir,
            created_at: chrono::Utc::now(),
            tenant_id: DEFAULT_TENANT.into(),
        })
        .await
        .expect("store_workflow");

    // Create the execution and enqueue the start node (mimics POST /executions).
    let execution_id = ExecutionId::new();
    let now = chrono::Utc::now();
    backend
        .create_execution(WorkflowExecution {
            execution_id: execution_id.clone(),
            workflow_id: "e2e-completion".into(),
            workflow_version: "0.1.0".into(),
            status: WorkflowStatus::Running,
            initial_input: serde_json::json!({}),
            current_state: serde_json::json!({}),
            started_at: now,
            updated_at: now,
            completed_at: None,
            session_type: None,
        })
        .await
        .expect("create_execution");

    backend
        .append_event(Event::new(
            execution_id.clone(),
            1,
            EventKind::WorkflowStarted {
                workflow_id: "e2e-completion".into(),
                workflow_version: "0.1.0".into(),
                initial_input: serde_json::json!({}),
            },
        ))
        .await
        .expect("append WorkflowStarted");
    backend
        .append_event(Event::new(
            execution_id.clone(),
            2,
            EventKind::NodeScheduled {
                node_id: "route".into(),
                queue_type: "general".into(),
            },
        ))
        .await
        .expect("append NodeScheduled");
    backend
        .enqueue_work_item(WorkItem {
            id: Uuid::new_v4(),
            execution_id: execution_id.clone(),
            node_id: "route".into(),
            queue_type: "general".into(),
            payload: serde_json::json!({
                "workflow_id": "e2e-completion",
                "workflow_version": "0.1.0",
            }),
            attempt: 0,
            max_attempts: 3,
            created_at: now,
            lease_expires_at: None,
            worker_id: None,
            lease_fence: 0,
            tenant_id: DEFAULT_TENANT.into(),
        })
        .await
        .expect("enqueue_work_item");

    // Spawn the scheduler + worker pool against the shared backend.
    let scheduler = jamjet_scheduler::Scheduler::new(backend.clone())
        .with_poll_interval(Duration::from_millis(25));
    let sched_handle = tokio::spawn(async move { scheduler.run().await });
    let worker_handles = jamjet_worker::default_pool(backend.clone()).spawn();

    // Wait for a terminal state, bounded by a hard timeout so the test can never
    // hang the suite even if completion detection regresses.
    let poll_backend = backend.clone();
    let exec_id = execution_id.clone();
    let wait = async move {
        loop {
            let exec = poll_backend
                .get_execution(&exec_id)
                .await
                .expect("get_execution")
                .expect("execution exists");
            if matches!(
                exec.status,
                WorkflowStatus::Completed | WorkflowStatus::Failed
            ) {
                return exec.status;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(20), wait).await;

    // Stop the background loops and clean up the temp DB.
    sched_handle.abort();
    for h in worker_handles {
        h.abort();
    }
    // Diagnostics if we did not reach a terminal state.
    if result.is_err() {
        let events = backend.get_events(&execution_id).await.unwrap_or_default();
        let kinds: Vec<String> = events.iter().map(|e| format!("{:?}", e.kind)).collect();
        eprintln!("E2E TIMEOUT — {} events: {:#?}", events.len(), kinds);
        if let Ok(Some(e)) = backend.get_execution(&execution_id).await {
            eprintln!("E2E final stored status: {:?}", e.status);
        }
    }

    let _ = std::fs::remove_file(&db_path);

    let status = result.ok();
    assert_eq!(
        status,
        Some(WorkflowStatus::Completed),
        "workflow should run to completion (got {status:?})"
    );
}
