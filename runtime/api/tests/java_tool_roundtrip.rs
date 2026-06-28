//! Durable `java_tool` claim/complete round-trip against the HTTP work-item API.
//!
//! This proves the contract the Phase B Java tool-worker will implement: a
//! workflow whose tool node is a `JavaFn` schedules a `java_tool` work item that
//! an EXTERNAL worker claims over `POST /work-items/claim` (the engine's own pool
//! registers `java_tool` with zero internal workers — see `default_pool`), and
//! completing it over `POST /work-items/{id}/complete` advances the execution
//! durably to a terminal state with the tool result committed to state.
//!
//! It mirrors the python_tool worker contract exactly — `execution_e2e.rs`
//! (scheduler + worker pool over a shared SQLite backend) for the durable engine
//! loop and `work_item_genai.rs` (in-process axum router) for the HTTP calls.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{AuditEnricher, NoopAuditBackend};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::{StateBackend, WorkItem, WorkflowDefinition};
use jamjet_state::event::EventKind;
use jamjet_state::{Event, SqliteBackend, DEFAULT_TENANT};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

fn make_state(backend: Arc<dyn StateBackend>) -> AppState {
    let backend_for_fn = backend.clone();
    let audit: Arc<dyn jamjet_audit::AuditBackend> = Arc::new(NoopAuditBackend);
    let enricher = Arc::new(AuditEnricher::new(Arc::clone(&audit)));
    AppState {
        backend: backend.clone(),
        backend_for_fn: Arc::new(move |_tenant_id: &jamjet_state::TenantId| backend_for_fn.clone()),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        audit,
        enricher,
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
    }
}

async fn body_json(body: Body) -> Value {
    let bytes = body.collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// A three-node workflow: `gate_in -> java_tool_node -> gate_out -> end`.
///
/// `java_tool_node` is a `JavaFn`, so the scheduler enriches its payload (class /
/// method / input) and routes it to the `java_tool` queue. The two condition
/// gates are ordinary nodes the engine's general workers drain — only the JavaFn
/// node lands on `java_tool`, which an external worker (this test) must claim.
fn java_tool_workflow_ir() -> Value {
    let condition = |id: &str| {
        serde_json::json!({
            "id": id,
            "kind": { "type": "condition", "branches": [] },
            "retry_policy": null,
            "node_timeout_secs": null,
            "description": null,
            "labels": {}
        })
    };
    serde_json::json!({
        "workflow_id": "java-tool-e2e",
        "version": "0.1.0",
        "name": null,
        "description": null,
        "state_schema": "",
        "start_node": "gate_in",
        "nodes": {
            "gate_in": condition("gate_in"),
            "java_tool_node": {
                "id": "java_tool_node",
                "kind": {
                    "type": "java_fn",
                    "class_name": "com.example.tools.WeatherTool",
                    "method": "getWeather",
                    "output_schema": ""
                },
                "retry_policy": "no_retry",
                "node_timeout_secs": null,
                "description": null,
                "labels": {}
            },
            "gate_out": condition("gate_out")
        },
        "edges": [
            { "from": "gate_in", "to": "java_tool_node", "condition": null },
            { "from": "java_tool_node", "to": "gate_out", "condition": null },
            { "from": "gate_out", "to": "end", "condition": null }
        ],
        "retry_policies": {},
        "timeouts": {},
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {}
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn java_tool_claim_complete_round_trip_advances_durably() {
    // A temp-file SQLite DB shared across the scheduler, worker pool, and the
    // HTTP router so the round-trip is genuinely durable (committed to disk).
    let db_path = std::env::temp_dir().join(format!("jjtest-java-{}.db", Uuid::new_v4()));
    let url = format!("sqlite://{}", db_path.display());
    let backend: Arc<dyn StateBackend> =
        Arc::new(SqliteBackend::open(&url).await.expect("open sqlite backend"));

    backend
        .store_workflow(WorkflowDefinition {
            workflow_id: "java-tool-e2e".into(),
            version: "0.1.0".into(),
            ir: java_tool_workflow_ir(),
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
            workflow_id: "java-tool-e2e".into(),
            workflow_version: "0.1.0".into(),
            status: WorkflowStatus::Running,
            initial_input: serde_json::json!({}),
            current_state: serde_json::json!({}),
            started_at: now,
            updated_at: now,
            completed_at: None,
            session_type: None,
            parent_execution_id: None,
            segment_number: 0,
        })
        .await
        .expect("create_execution");
    backend
        .append_event(Event::new(
            execution_id.clone(),
            1,
            EventKind::WorkflowStarted {
                workflow_id: "java-tool-e2e".into(),
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
                node_id: "gate_in".into(),
                queue_type: "general".into(),
            },
        ))
        .await
        .expect("append NodeScheduled");
    backend
        .enqueue_work_item(WorkItem {
            id: Uuid::new_v4(),
            execution_id: execution_id.clone(),
            node_id: "gate_in".into(),
            queue_type: "general".into(),
            payload: serde_json::json!({
                "workflow_id": "java-tool-e2e",
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

    // Spawn the scheduler + the default worker pool. The pool registers
    // `java_tool` with ZERO internal workers, so the JavaFn node's work item is
    // claimable only by an external worker — which this test plays the part of.
    let scheduler = jamjet_scheduler::Scheduler::new(backend.clone())
        .with_poll_interval(Duration::from_millis(25));
    let sched_handle = tokio::spawn(async move { scheduler.run().await });
    let worker_handles = jamjet_worker::default_pool(backend.clone()).spawn();

    let state = make_state(backend.clone());

    // ── Act as the Java tool-worker: poll `POST /work-items/claim` until the
    // `gate_in` condition completes and the scheduler enqueues the enriched
    // `java_tool` work item. Bounded so a regression can never hang the suite.
    let claimed = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let body = serde_json::json!({
                "worker_id": "java-tool-worker-0",
                "queue_types": ["java_tool"],
            });
            let resp = build_router_with_opts(state.clone(), true)
                .oneshot(
                    Request::post("/work-items/claim")
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "claim must not error");
            let json = body_json(resp.into_body()).await;
            if json["claimed"] == true {
                return json["work_item"].clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("a java_tool work item must become claimable");

    // The claimed item is the JavaFn node on the java_tool queue, carrying the
    // scheduler's enrichment the Phase B Java worker reflects/dispatches against.
    assert_eq!(claimed["queue_type"], "java_tool");
    assert_eq!(claimed["node_id"], "java_tool_node");
    assert_eq!(
        claimed["payload"]["class"], "com.example.tools.WeatherTool",
        "claimed payload must carry the class name"
    );
    assert_eq!(
        claimed["payload"]["method"], "getWeather",
        "claimed payload must carry the method name"
    );
    assert!(
        claimed["payload"]["input"].is_object(),
        "claimed payload.input must be the accumulated workflow state"
    );
    let work_item_id = claimed["id"].as_str().expect("work item id").to_string();

    // ── Complete the work item over HTTP with the tool result (the same path the
    // python_tool worker uses), advancing the execution.
    let complete_body = serde_json::json!({
        "execution_id": execution_id.to_string(),
        "node_id": "java_tool_node",
        "output": { "temp_c": 18 },
        "state_patch": { "java_result": "sunny" },
        "duration_ms": 7
    });
    let resp = build_router_with_opts(state.clone(), true)
        .oneshot(
            Request::post(format!("/work-items/{work_item_id}/complete"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&complete_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp.into_body()).await["completed"], true);

    // ── The execution must now advance durably to a terminal state: the JavaFn
    // node was drained by the external worker, so the scheduler routes
    // `java_tool_node -> gate_out -> end` and detects completion.
    let final_status = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let exec = backend
                .get_execution(&execution_id)
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
    })
    .await;

    sched_handle.abort();
    for h in worker_handles {
        h.abort();
    }

    assert_eq!(
        final_status.ok(),
        Some(WorkflowStatus::Completed),
        "completing the java_tool work item must drive the execution to completion"
    );

    // The tool result landed in committed (durable) state.
    let exec = backend
        .get_execution(&execution_id)
        .await
        .expect("get_execution")
        .expect("execution exists");
    assert_eq!(
        exec.current_state["java_result"], "sunny",
        "the java_tool result must be committed to execution state"
    );

    // The durable event log carries the JavaFn node's completion and the
    // subsequent node was scheduled (the next node became runnable).
    let events = backend.get_events(&execution_id).await.expect("get_events");
    assert!(
        events.iter().any(|e| matches!(
            &e.kind,
            EventKind::NodeCompleted { node_id, .. } if node_id == "java_tool_node"
        )),
        "a NodeCompleted event must be committed for the java_tool node"
    );
    assert!(
        events.iter().any(|e| matches!(
            &e.kind,
            EventKind::NodeScheduled { node_id, .. } if node_id == "gate_out"
        )),
        "the next node must be scheduled after the java_tool node completes"
    );

    let _ = std::fs::remove_file(&db_path);
}
