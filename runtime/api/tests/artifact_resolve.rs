//! Task 2i-3: verify that GET /executions/:id/events resolves ArtifactRef
//! sentinels in NodeCompleted.output before returning the response.
//!
//! Covers three cases:
//!   1. Spilled output (sentinel) -> resolved original value.
//!   2. Inline output (no sentinel) -> passes through unchanged.
//!   3. Dangling sentinel (artifact missing) -> 200 with unresolved=true; no panic.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{AuditEnricher, NoopAuditBackend};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::{ArtifactRef, Event, InMemoryBackend};
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_state(backend: Arc<InMemoryBackend>) -> AppState {
    let backend_clone = backend.clone();
    let audit: Arc<dyn jamjet_audit::AuditBackend> = Arc::new(NoopAuditBackend);
    let enricher = Arc::new(AuditEnricher::new(Arc::clone(&audit)));
    AppState {
        backend: backend.clone() as Arc<dyn jamjet_state::StateBackend>,
        backend_for_fn: Arc::new(move |_tenant_id: &jamjet_state::TenantId| {
            backend_clone.clone() as Arc<dyn jamjet_state::StateBackend>
        }),
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

async fn create_execution(backend: &Arc<InMemoryBackend>) -> ExecutionId {
    let execution_id = ExecutionId::new();
    let now = chrono::Utc::now();
    backend
        .create_execution(WorkflowExecution {
            execution_id: execution_id.clone(),
            workflow_id: "test-wf".into(),
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
    execution_id
}

fn node_completed_kind(node_id: &str, output: Value) -> EventKind {
    EventKind::NodeCompleted {
        node_id: node_id.into(),
        output,
        state_patch: serde_json::json!({}),
        duration_ms: 1,
        gen_ai_system: None,
        gen_ai_model: None,
        input_tokens: None,
        output_tokens: None,
        finish_reason: None,
        cost_usd: None,
        provenance: None,
        idempotency_key: None,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// A spilled NodeCompleted.output (sentinel stored in the event log) must be
/// resolved back to the original JSON value by GET /executions/:id/events.
#[tokio::test]
async fn list_events_resolves_spilled_node_output() {
    let backend = Arc::new(InMemoryBackend::new());
    let state = make_state(backend.clone());
    let app = build_router_with_opts(state, true);

    let execution_id = create_execution(&backend).await;

    // Build a large output that exceeds the 8 KB spill threshold.
    let original_output = serde_json::json!({
        "content": "a".repeat(10_000),
        "model": "claude-test",
        "finish_reason": "stop",
    });

    // Spill: write the artifact to the store BEFORE appending the event
    // (mirrors the write-order invariant enforced by the 2i-2 write path).
    let bytes = serde_json::to_vec(&original_output).expect("serialize");
    let artifact_ref = backend
        .put_artifact(&bytes, Some("application/json"))
        .await
        .expect("put_artifact");
    let sentinel = artifact_ref.to_sentinel();

    // Append a NodeCompleted event that carries the sentinel instead of inline bytes.
    let seq = backend
        .latest_sequence(&execution_id)
        .await
        .expect("latest_sequence")
        + 1;
    backend
        .append_event(Event::new(
            execution_id.clone(),
            seq,
            node_completed_kind("spilled-node", sentinel),
        ))
        .await
        .expect("append_event");

    // GET /executions/:id/events — must return the RESOLVED original output.
    let resp = app
        .oneshot(
            Request::get(format!("/executions/{}/events", execution_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json: Value = body_json(resp.into_body()).await;

    let events = json["events"].as_array().expect("events array");
    let nc = events
        .iter()
        .find(|e| e["kind"]["type"] == "node_completed")
        .expect("NodeCompleted event must be present");

    let output = &nc["kind"]["output"];

    // Sentinel must be gone — original value is restored.
    assert!(
        output.get("$artifact").is_none(),
        "output must not contain a sentinel after resolve; got: {output}"
    );
    assert_eq!(output["model"], "claude-test");
    assert_eq!(output["finish_reason"], "stop");
    let content = output["content"].as_str().expect("content is a string");
    assert_eq!(
        content.len(),
        10_000,
        "resolved content must match original length"
    );
}

/// An inline (non-spilled) NodeCompleted.output must be passed through as-is.
/// resolve_value is a no-op for non-sentinel values.
#[tokio::test]
async fn list_events_passes_through_inline_output() {
    let backend = Arc::new(InMemoryBackend::new());
    let state = make_state(backend.clone());
    let app = build_router_with_opts(state, true);

    let execution_id = create_execution(&backend).await;

    let inline_output = serde_json::json!({ "status": "ok", "value": 42 });

    let seq = backend
        .latest_sequence(&execution_id)
        .await
        .expect("latest_sequence")
        + 1;
    backend
        .append_event(Event::new(
            execution_id.clone(),
            seq,
            node_completed_kind("inline-node", inline_output.clone()),
        ))
        .await
        .expect("append_event");

    let resp = app
        .oneshot(
            Request::get(format!("/executions/{}/events", execution_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json: Value = body_json(resp.into_body()).await;

    let events = json["events"].as_array().expect("events array");
    let nc = events
        .iter()
        .find(|e| e["kind"]["type"] == "node_completed")
        .expect("NodeCompleted event must be present");

    let output = &nc["kind"]["output"];
    assert_eq!(
        output, &inline_output,
        "inline output must pass through unchanged"
    );
}

/// A dangling sentinel (artifact missing from the store) must return 200 with
/// the sentinel still in place and an "unresolved": true flag — never panic or 5xx.
#[tokio::test]
async fn list_events_dangling_artifact_ref_returns_unresolved_flag() {
    let backend = Arc::new(InMemoryBackend::new());
    let state = make_state(backend.clone());
    let app = build_router_with_opts(state, true);

    let execution_id = create_execution(&backend).await;

    // Craft a sentinel whose hash was NEVER put in the store (dangling ref).
    let dangling_ref = ArtifactRef {
        hash: "deadbeef".repeat(8), // 64-char hex-shaped string
        size: 1234,
        media_type: Some("application/json".into()),
    };
    let dangling_sentinel = dangling_ref.to_sentinel();

    let seq = backend
        .latest_sequence(&execution_id)
        .await
        .expect("latest_sequence")
        + 1;
    backend
        .append_event(Event::new(
            execution_id.clone(),
            seq,
            node_completed_kind("dangling-node", dangling_sentinel),
        ))
        .await
        .expect("append_event");

    // Must not panic — must return 200.
    let resp = app
        .oneshot(
            Request::get(format!("/executions/{}/events", execution_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "dangling ref must not cause a 5xx"
    );
    let json: Value = body_json(resp.into_body()).await;

    let events = json["events"].as_array().expect("events array");
    let nc = events
        .iter()
        .find(|e| e["kind"]["type"] == "node_completed")
        .expect("NodeCompleted event must be present");

    let output = &nc["kind"]["output"];

    // Sentinel is kept (we cannot resolve), and unresolved=true is added.
    assert!(
        output.get("$artifact").is_some(),
        "sentinel must be present for a dangling ref: {output}"
    );
    assert_eq!(
        output["$artifact"]["unresolved"], true,
        "unresolved flag must be set for dangling ref: {output}"
    );
}
