//! Verifies that GenAI telemetry fields in `POST /work-items/:id/complete`
//! are threaded through to the emitted `NodeCompleted` event.
//!
//! Uses an in-process `InMemoryBackend` and the dev-mode router (no auth).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{AuditEnricher, NoopAuditBackend};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::{Event, InMemoryBackend};
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

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

#[tokio::test]
async fn complete_work_item_threads_gen_ai_fields() {
    let backend = Arc::new(InMemoryBackend::new());
    let state = make_state(backend.clone());
    let app = build_router_with_opts(state, true);

    // Seed an execution so `latest_sequence` has something to increment from.
    let execution_id = ExecutionId::new();
    let now = chrono::Utc::now();
    backend
        .create_execution(WorkflowExecution {
            execution_id: execution_id.clone(),
            workflow_id: "wf-genai".into(),
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
                workflow_id: "wf-genai".into(),
                workflow_version: "0.1.0".into(),
                initial_input: serde_json::json!({}),
            },
        ))
        .await
        .expect("append WorkflowStarted");

    // POST /work-items/:id/complete with all five GenAI telemetry fields.
    let work_item_id = Uuid::new_v4();
    let body = serde_json::json!({
        "execution_id": execution_id.to_string(),
        "node_id": "genai-node",
        "output": {"text": "hello"},
        "state_patch": {},
        "duration_ms": 42,
        "gen_ai_system": "anthropic",
        "gen_ai_model": "claude-3-5-sonnet-20241022",
        "input_tokens": 100,
        "output_tokens": 50,
        "finish_reason": "stop"
    });

    let resp = app
        .oneshot(
            Request::post(format!("/work-items/{work_item_id}/complete"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let resp_json = body_json(resp.into_body()).await;
    assert_eq!(resp_json["completed"], true);

    // Verify the emitted NodeCompleted event carries the GenAI fields.
    let events = backend.get_events(&execution_id).await.expect("get_events");

    let node_completed = events.iter().find(|e| {
        matches!(
            &e.kind,
            EventKind::NodeCompleted { node_id, .. } if node_id == "genai-node"
        )
    });

    let event = node_completed.expect("NodeCompleted event must be emitted for genai-node");
    match &event.kind {
        EventKind::NodeCompleted {
            gen_ai_system,
            gen_ai_model,
            input_tokens,
            output_tokens,
            finish_reason,
            ..
        } => {
            assert_eq!(
                gen_ai_system.as_deref(),
                Some("anthropic"),
                "gen_ai_system must be threaded through"
            );
            assert_eq!(
                gen_ai_model.as_deref(),
                Some("claude-3-5-sonnet-20241022"),
                "gen_ai_model must be threaded through"
            );
            assert_eq!(
                *input_tokens,
                Some(100),
                "input_tokens must be threaded through"
            );
            assert_eq!(
                *output_tokens,
                Some(50),
                "output_tokens must be threaded through"
            );
            assert_eq!(
                finish_reason.as_deref(),
                Some("stop"),
                "finish_reason must be threaded through"
            );
        }
        _ => panic!("expected NodeCompleted event, got: {:?}", event.kind),
    }
}

#[tokio::test]
async fn complete_work_item_without_gen_ai_fields_keeps_backward_compat() {
    // A body without GenAI fields must still succeed (serde defaults to None).
    let backend = Arc::new(InMemoryBackend::new());
    let state = make_state(backend.clone());
    let app = build_router_with_opts(state, true);

    let execution_id = ExecutionId::new();
    let now = chrono::Utc::now();
    backend
        .create_execution(WorkflowExecution {
            execution_id: execution_id.clone(),
            workflow_id: "wf-compat".into(),
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
                workflow_id: "wf-compat".into(),
                workflow_version: "0.1.0".into(),
                initial_input: serde_json::json!({}),
            },
        ))
        .await
        .expect("append WorkflowStarted");

    let work_item_id = Uuid::new_v4();
    // Old-style body with no GenAI fields.
    let body = serde_json::json!({
        "execution_id": execution_id.to_string(),
        "node_id": "compat-node",
        "output": {"result": 42},
        "state_patch": {},
        "duration_ms": 10
    });

    let resp = app
        .oneshot(
            Request::post(format!("/work-items/{work_item_id}/complete"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let events = backend.get_events(&execution_id).await.expect("get_events");
    let node_completed = events.iter().find(|e| {
        matches!(
            &e.kind,
            EventKind::NodeCompleted { node_id, .. } if node_id == "compat-node"
        )
    });
    let event = node_completed.expect("NodeCompleted event must be emitted");
    match &event.kind {
        EventKind::NodeCompleted {
            gen_ai_system,
            gen_ai_model,
            input_tokens,
            output_tokens,
            finish_reason,
            ..
        } => {
            assert!(
                gen_ai_system.is_none(),
                "gen_ai_system must be None for old callers"
            );
            assert!(
                gen_ai_model.is_none(),
                "gen_ai_model must be None for old callers"
            );
            assert!(input_tokens.is_none());
            assert!(output_tokens.is_none());
            assert!(finish_reason.is_none());
        }
        _ => panic!("expected NodeCompleted event"),
    }
}
