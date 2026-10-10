//! The router's fallback serves the embedded web companion, and nothing else.
//!
//! Before this, `serve_spa` answered EVERY unmatched request with
//! `200 text/html` (index.html), whatever the method. A client on a wrong path
//! (the Java SDK posted to `/api/v1/...` for its whole life) got a 200 and an
//! HTML page, failed to parse it, and the frameworks above it logged a warning
//! and carried on with no durability and no audit. These tests pin that an
//! API-shaped miss is a JSON 404, while a browser page load still gets the SPA.

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{AuditEnricher, NoopAuditBackend};
use jamjet_state::backend::StateBackend;
use jamjet_state::InMemoryBackend;
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

fn make_state() -> AppState {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let backend_for_fn = backend.clone();
    let audit: Arc<dyn jamjet_audit::AuditBackend> = Arc::new(NoopAuditBackend);
    let enricher = Arc::new(AuditEnricher::new(Arc::clone(&audit)));
    AppState {
        backend,
        backend_for_fn: Arc::new(move |_tenant_id: &jamjet_state::TenantId| backend_for_fn.clone()),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        audit,
        enricher,
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
    }
}

async fn send(dev_mode: bool, method: Method, path: &str) -> (StatusCode, String, Vec<u8>) {
    let body = if method == Method::GET || method == Method::HEAD {
        Body::empty()
    } else {
        Body::from("{}")
    };
    let res = build_router_with_opts(make_state(), dev_mode)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let ctype = res
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let bytes = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, ctype, bytes)
}

/// Asserts the exact JSON 404 contract: status, content type, and both keys.
async fn assert_json_404(dev_mode: bool, method: Method, path: &str) {
    let (status, ctype, bytes) = send(dev_mode, method.clone(), path).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "{method} {path} (dev_mode={dev_mode}) must be a 404, not the SPA"
    );
    assert!(
        ctype.starts_with("application/json"),
        "{method} {path}: content-type must be JSON, got {ctype:?}"
    );
    let v: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("{method} {path}: body must be JSON ({e})"));
    assert_eq!(v["error"], "not found", "{method} {path}: {v}");
    let hint = v["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("/executions"),
        "{method} {path}: hint must name the real routes, got {hint:?}"
    );
    // Both JamJet Java clients that sent /api/v1 must be named, so a user of
    // either one can tell the 404 is about their client.
    for client in ["jamjet-sdk", "jamjet-spring-boot-starter"] {
        assert!(
            hint.contains(client),
            "{method} {path}: hint must name {client}, got {hint:?}"
        );
    }
}

async fn assert_spa(method: Method, path: &str) {
    let (status, ctype, _) = send(true, method.clone(), path).await;
    assert_eq!(status, StatusCode::OK, "{method} {path} must serve the SPA");
    assert!(
        ctype.starts_with("text/html"),
        "{method} {path}: SPA must be text/html, got {ctype:?}"
    );
}

#[tokio::test]
async fn api_v1_paths_are_json_404_for_every_method() {
    for dev_mode in [true, false] {
        // The exact calls the published 0.5.0 Java SDK makes.
        assert_json_404(dev_mode, Method::POST, "/api/v1/workflows").await;
        assert_json_404(dev_mode, Method::GET, "/api/v1/workflows").await;
        assert_json_404(dev_mode, Method::POST, "/api/v1/workflows/wf/execute").await;
        assert_json_404(dev_mode, Method::GET, "/api/v1/executions/exec_1").await;
        assert_json_404(dev_mode, Method::POST, "/api/v1/executions/exec_1/approve").await;
    }
}

#[tokio::test]
async fn get_under_api_prefix_is_json_404() {
    // The web companion fetched `/api/executions`; the runtime never served it.
    assert_json_404(true, Method::GET, "/api/executions").await;
    assert_json_404(true, Method::GET, "/api").await;
    assert_json_404(true, Method::GET, "/api/").await;
}

#[tokio::test]
async fn non_get_on_unknown_path_is_json_404() {
    assert_json_404(true, Method::POST, "/").await;
    assert_json_404(true, Method::POST, "/no-such-route").await;
    assert_json_404(true, Method::PUT, "/some/page").await;
    assert_json_404(true, Method::DELETE, "/some/page").await;
    assert_json_404(true, Method::PATCH, "/some/page").await;
}

#[tokio::test]
async fn page_loads_still_serve_the_spa() {
    assert_spa(Method::GET, "/").await;
    assert_spa(Method::GET, "/some/page").await;
    assert_spa(Method::HEAD, "/some/page").await;
    // Only the `/api` SEGMENT is reserved, not every path starting with "api".
    assert_spa(Method::GET, "/apiary").await;
}

#[tokio::test]
async fn real_routes_are_unaffected() {
    let (status, ctype, _) = send(true, Method::GET, "/health").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ctype.starts_with("application/json"));
    let (status, _, _) = send(true, Method::GET, "/executions").await;
    assert_eq!(status, StatusCode::OK);
}
