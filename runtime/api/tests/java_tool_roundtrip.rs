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
    let backend: Arc<dyn StateBackend> = Arc::new(
        SqliteBackend::open(&url)
            .await
            .expect("open sqlite backend"),
    );

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
    // The claim response carries the lease fence (F1) the external worker echoes
    // on complete so the engine can prove the lease is still held.
    let lease_fence = claimed["lease_fence"]
        .as_i64()
        .expect("claim response must carry lease_fence");
    assert!(
        lease_fence > 0,
        "a claimed item must have a non-zero lease fence"
    );

    // ── Complete the work item over HTTP with the tool result (the same path the
    // python_tool worker uses), advancing the execution — echoing the lease fence
    // so the completion is fence-validated end-to-end.
    let complete_body = serde_json::json!({
        "execution_id": execution_id.to_string(),
        "node_id": "java_tool_node",
        "output": { "temp_c": 18 },
        "state_patch": { "java_result": "sunny" },
        "duration_ms": 7,
        "lease_fence": lease_fence
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

// ── Lease-fence validation on the external HTTP complete path ─────────────────
//
// The internal worker tier gets exactly-once-COMMIT from the lease fence
// (`worker.rs` `commit_turn`); these tests prove the EXTERNAL HTTP complete path
// (`POST /work-items/{id}/complete`) is fenced too, so a reclaimed / replayed
// worker cannot settle a stale lease and duplicate a `NodeCompleted` event.
//
// They drive the complete handler directly (no scheduler / worker pool) so the
// fence logic is isolated and deterministic.

fn temp_sqlite_url() -> (std::path::PathBuf, String) {
    let db_path = std::env::temp_dir().join(format!("jjtest-fence-{}.db", Uuid::new_v4()));
    let url = format!("sqlite://{}", db_path.display());
    (db_path, url)
}

/// Create a Running execution and enqueue a single claimable `java_tool` work
/// item for it, mirroring what the scheduler would enqueue for a JavaFn node.
async fn seed_claimable_java_item(backend: &Arc<dyn StateBackend>) -> ExecutionId {
    let execution_id = ExecutionId::new();
    let now = chrono::Utc::now();
    backend
        .create_execution(WorkflowExecution {
            execution_id: execution_id.clone(),
            workflow_id: "java-tool-fence".into(),
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
                workflow_id: "java-tool-fence".into(),
                workflow_version: "0.1.0".into(),
                initial_input: serde_json::json!({}),
            },
        ))
        .await
        .expect("append WorkflowStarted");
    backend
        .enqueue_work_item(WorkItem {
            id: Uuid::new_v4(),
            execution_id: execution_id.clone(),
            node_id: "java_tool_node".into(),
            queue_type: "java_tool".into(),
            payload: serde_json::json!({
                "class": "com.example.tools.WeatherTool",
                "method": "getWeather",
                "input": {},
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
    execution_id
}

/// Claim the single `java_tool` item over HTTP; returns the `work_item` JSON
/// (which, after F1, carries `lease_fence`).
async fn http_claim_java(state: &AppState) -> Value {
    let body = serde_json::json!({
        "worker_id": "java-fence-worker",
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
    assert_eq!(json["claimed"], true, "an item must be claimable");
    json["work_item"].clone()
}

/// Complete a work item over HTTP; returns the (status, body) pair.
async fn http_complete(state: &AppState, item_id: &str, body: Value) -> (StatusCode, Value) {
    let resp = build_router_with_opts(state.clone(), true)
        .oneshot(
            Request::post(format!("/work-items/{item_id}/complete"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp.into_body()).await)
}

/// Count `NodeCompleted` events for `node_id` in the durable event log.
async fn count_node_completed(
    backend: &Arc<dyn StateBackend>,
    eid: &ExecutionId,
    node_id: &str,
) -> usize {
    backend
        .get_events(eid)
        .await
        .expect("get_events")
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::NodeCompleted { node_id: n, .. } if n == node_id))
        .count()
}

/// Happy path: claim returns the fence (F1), and completing WITH the matching
/// fence settles the item, emits `NodeCompleted`, and lands the result in state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_with_matching_fence_succeeds() {
    let (db_path, url) = temp_sqlite_url();
    let backend: Arc<dyn StateBackend> =
        Arc::new(SqliteBackend::open(&url).await.expect("open sqlite"));
    let execution_id = seed_claimable_java_item(&backend).await;
    let state = make_state(backend.clone());

    let claimed = http_claim_java(&state).await;
    // F1: the claim response must carry the lease fence so the worker can echo it.
    let fence = claimed["lease_fence"]
        .as_i64()
        .expect("claim response must carry lease_fence");
    assert!(fence > 0, "a claimed item must have a non-zero lease fence");
    let item_id = claimed["id"].as_str().unwrap().to_string();

    let (status, body) = http_complete(
        &state,
        &item_id,
        serde_json::json!({
            "execution_id": execution_id.to_string(),
            "node_id": "java_tool_node",
            "output": { "temp_c": 18 },
            "state_patch": { "java_result": "sunny" },
            "duration_ms": 7,
            "lease_fence": fence
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "matching fence must be accepted");
    assert_eq!(body["completed"], true);
    assert_eq!(
        count_node_completed(&backend, &execution_id, "java_tool_node").await,
        1,
        "exactly one NodeCompleted must be committed"
    );
    let exec = backend.get_execution(&execution_id).await.unwrap().unwrap();
    assert_eq!(
        exec.current_state["java_result"], "sunny",
        "the result must land in durable state"
    );

    let _ = std::fs::remove_file(&db_path);
}

/// Adversarial dual: an item is claimed (fence F1), then RECLAIMED (a fresh claim
/// mints F2 > F1). A complete carrying the OLD fence F1 — or a forged fence — is
/// REJECTED with 409, appends NO `NodeCompleted`, and does NOT double-settle. The
/// current holder (F2) can still settle exactly once afterward.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_with_stale_or_forged_fence_is_rejected() {
    let (db_path, url) = temp_sqlite_url();
    let backend: Arc<dyn StateBackend> =
        Arc::new(SqliteBackend::open(&url).await.expect("open sqlite"));
    let execution_id = seed_claimable_java_item(&backend).await;
    let state = make_state(backend.clone());

    // First claim → fence F1.
    let first = http_claim_java(&state).await;
    let item_id = first["id"].as_str().unwrap().to_string();
    let fence_1 = first["lease_fence"].as_i64().expect("F1 fence");
    let wi_id = Uuid::parse_str(&item_id).unwrap();

    // Simulate a reclaim: park the leased item back to pending (fence-guarded by
    // F1, made immediately visible) then claim again → a strictly-greater F2.
    let past = (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
    let parked = backend
        .park_work_item(wi_id, fence_1, &past, 1)
        .await
        .expect("park_work_item");
    assert!(parked, "parking with the held fence must succeed");
    let second = http_claim_java(&state).await;
    assert_eq!(
        second["id"].as_str().unwrap(),
        item_id,
        "the reclaim must be the same item"
    );
    let fence_2 = second["lease_fence"].as_i64().expect("F2 fence");
    assert!(
        fence_2 > fence_1,
        "a reclaim must mint a strictly-greater fence (F2={fence_2} > F1={fence_1})"
    );

    // Zombie worker completes with the STALE fence F1 → must be rejected.
    let (status, body) = http_complete(
        &state,
        &item_id,
        serde_json::json!({
            "execution_id": execution_id.to_string(),
            "node_id": "java_tool_node",
            "output": { "stale": true },
            "state_patch": { "java_result": "STALE" },
            "duration_ms": 1,
            "lease_fence": fence_1
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a stale fence must be rejected with 409"
    );
    assert_eq!(
        body["completed"], false,
        "the stale completion must report completed=false"
    );
    assert_eq!(
        count_node_completed(&backend, &execution_id, "java_tool_node").await,
        0,
        "a stale completion must NOT append a NodeCompleted event"
    );

    // Forged / absent-from-the-table fence value → also rejected.
    let (forged_status, forged_body) = http_complete(
        &state,
        &item_id,
        serde_json::json!({
            "execution_id": execution_id.to_string(),
            "node_id": "java_tool_node",
            "output": { "forged": true },
            "state_patch": { "java_result": "FORGED" },
            "duration_ms": 1,
            "lease_fence": 999_999_999_i64
        }),
    )
    .await;
    assert_eq!(
        forged_status,
        StatusCode::CONFLICT,
        "a forged fence must be rejected"
    );
    assert_eq!(forged_body["completed"], false);
    assert_eq!(
        count_node_completed(&backend, &execution_id, "java_tool_node").await,
        0,
        "a forged completion must NOT append a NodeCompleted event"
    );

    // The state was never mutated by the rejected attempts.
    let exec = backend.get_execution(&execution_id).await.unwrap().unwrap();
    assert!(
        exec.current_state.get("java_result").is_none(),
        "no rejected state_patch may be applied"
    );

    // The legitimate current holder (F2) settles exactly once.
    let (ok_status, ok_body) = http_complete(
        &state,
        &item_id,
        serde_json::json!({
            "execution_id": execution_id.to_string(),
            "node_id": "java_tool_node",
            "output": { "ok": true },
            "state_patch": { "java_result": "fresh" },
            "duration_ms": 2,
            "lease_fence": fence_2
        }),
    )
    .await;
    assert_eq!(
        ok_status,
        StatusCode::OK,
        "the current fence holder must settle"
    );
    assert_eq!(ok_body["completed"], true);
    assert_eq!(
        count_node_completed(&backend, &execution_id, "java_tool_node").await,
        1,
        "the legitimate completion appends exactly one NodeCompleted"
    );

    let _ = std::fs::remove_file(&db_path);
}

/// Backward-compat: a complete with NO `lease_fence` keeps the current unfenced
/// behavior, so existing callers that do not yet echo a fence do not break.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_without_fence_is_backward_compatible() {
    let (db_path, url) = temp_sqlite_url();
    let backend: Arc<dyn StateBackend> =
        Arc::new(SqliteBackend::open(&url).await.expect("open sqlite"));
    let execution_id = seed_claimable_java_item(&backend).await;
    let state = make_state(backend.clone());

    let claimed = http_claim_java(&state).await;
    let item_id = claimed["id"].as_str().unwrap().to_string();

    // No `lease_fence` field at all — the legacy unfenced path.
    let (status, body) = http_complete(
        &state,
        &item_id,
        serde_json::json!({
            "execution_id": execution_id.to_string(),
            "node_id": "java_tool_node",
            "output": { "legacy": true },
            "state_patch": { "java_result": "legacy" },
            "duration_ms": 3
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "absent fence must keep working");
    assert_eq!(body["completed"], true);
    assert_eq!(
        count_node_completed(&backend, &execution_id, "java_tool_node").await,
        1,
        "the unfenced path still emits NodeCompleted"
    );

    let _ = std::fs::remove_file(&db_path);
}
