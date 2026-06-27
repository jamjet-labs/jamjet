//! End-to-end test: the durable Model node routes through the sidecar seam.
//!
//! Proves that when `ModelRegistry` is configured with a `SidecarModelAdapter`,
//! `ModelNodeExecutor` routes calls through it and surfaces the sidecar's token
//! metering in `ExecutionResult`. This is a node-level test (not registry-level)
//! — the full `execute()` path runs, identical to the durable worker call path.

use jamjet_core::workflow::ExecutionId;
use jamjet_models::{ModelRegistry, SidecarModelAdapter};
use jamjet_state::backend::WorkItem;
use jamjet_worker::{ModelNodeExecutor, NodeExecutor};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Minimal work item that drives a Model node with a plain text prompt.
fn make_model_item() -> WorkItem {
    WorkItem {
        id: uuid::Uuid::new_v4(),
        execution_id: ExecutionId::new(),
        node_id: "model-1".into(),
        queue_type: "model".into(),
        payload: serde_json::json!({
            "model": "anthropic/claude-sonnet-4-6",
            "prompt": "say hi",
        }),
        attempt: 1,
        max_attempts: 3,
        created_at: chrono::Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: "default".into(),
    }
}

/// The durable Model node routes through the sidecar and carries its metering.
///
/// Wires `SidecarModelAdapter` as the registry default, drives
/// `ModelNodeExecutor::execute()`, and asserts that token counts and content
/// from the canned sidecar response appear unchanged in the `ExecutionResult`.
#[tokio::test]
async fn model_node_routes_through_sidecar_seam() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{
                "message": {"content": "hi from seam", "role": "assistant"},
                "input_tokens": 11,
                "output_tokens": 7,
                "cost_usd": 0.002,
                "model": "anthropic/claude-sonnet-4-6",
                "finish_reason": "stop"
            }"#,
        ))
        .mount(&server)
        .await;

    // Wire the registry so every call goes to the sidecar.
    let registry = Arc::new(
        ModelRegistry::new()
            .register(Arc::new(SidecarModelAdapter::new(server.uri())))
            .with_default("sidecar"),
    );

    let executor = ModelNodeExecutor::new(registry);
    let item = make_model_item();

    let result = executor
        .execute(&item)
        .await
        .expect("ModelNodeExecutor must succeed against the mock sidecar");

    // Token metering from the sidecar must flow through unmodified.
    assert_eq!(
        result.input_tokens,
        Some(11),
        "input_tokens must match the sidecar response"
    );
    assert_eq!(
        result.output_tokens,
        Some(7),
        "output_tokens must match the sidecar response"
    );
    assert_eq!(
        result.finish_reason.as_deref(),
        Some("stop"),
        "finish_reason must propagate"
    );

    // Content surfaces in the ExecutionResult output object.
    assert_eq!(
        result.output["content"].as_str(),
        Some("hi from seam"),
        "content must be the sidecar's response verbatim"
    );

    // Telemetry fields must be populated.
    assert_eq!(
        result.gen_ai_model.as_deref(),
        Some("anthropic/claude-sonnet-4-6"),
        "model name must propagate to telemetry"
    );
}
