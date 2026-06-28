//! Task T4-4: the developer-facing artifact HTTP API over the 2i CAS.
//!
//! Exercises the real router (dev mode injects the `default` tenant) end to end:
//!   1. POST /artifacts (raw bytes + Content-Type) -> 200 with {hash, size, media_type}.
//!   2. GET /artifacts/:hash -> 200 with the EXACT bytes that were stored.
//!   3. GET /artifacts/<unknown hash> -> 404.
//!
//! Tenant isolation (a put under tenant A is not GETtable as tenant B) is covered
//! by the in-crate unit test `routes::tests::artifacts_are_tenant_isolated`, which
//! drives the handlers with distinct `TenantId`s (the dev/prod middleware pins a
//! single tenant, so it cannot vary the tenant through the HTTP layer).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{AuditEnricher, NoopAuditBackend};
use jamjet_state::InMemoryBackend;
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

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

async fn body_bytes(body: Body) -> Vec<u8> {
    body.collect().await.unwrap().to_bytes().to_vec()
}

/// POST raw bytes -> 200 + ArtifactRef JSON; GET by that hash -> the exact bytes.
#[tokio::test]
async fn put_then_get_roundtrips_bytes() {
    let backend = Arc::new(InMemoryBackend::new());
    let app = build_router_with_opts(make_state(backend), true);

    let payload = b"hello artifact world".to_vec();

    // POST /artifacts with a Content-Type header (the media type source).
    let put_resp = app
        .clone()
        .oneshot(
            Request::post("/artifacts")
                .header("content-type", "text/plain")
                .body(Body::from(payload.clone()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(put_resp.status(), StatusCode::OK);
    let put_json: Value = serde_json::from_slice(&body_bytes(put_resp.into_body()).await).unwrap();

    let hash = put_json["hash"].as_str().expect("hash present").to_string();
    assert_eq!(hash.len(), 64, "hash is sha256 hex");
    assert!(
        hash.chars().all(|c| c.is_ascii_hexdigit()),
        "hash is hex: {hash}"
    );
    assert_eq!(put_json["size"].as_u64(), Some(payload.len() as u64));
    assert_eq!(
        put_json["media_type"], "text/plain",
        "media type comes from the Content-Type header"
    );

    // GET /artifacts/:hash -> the exact bytes back.
    let get_resp = app
        .oneshot(
            Request::get(format!("/artifacts/{hash}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(get_resp.status(), StatusCode::OK);
    let got = body_bytes(get_resp.into_body()).await;
    assert_eq!(got, payload, "GET returns the exact stored bytes");
}

/// The `?media_type=` query parameter overrides the Content-Type header.
#[tokio::test]
async fn put_media_type_query_overrides_header() {
    let backend = Arc::new(InMemoryBackend::new());
    let app = build_router_with_opts(make_state(backend), true);

    let put_resp = app
        .oneshot(
            Request::post("/artifacts?media_type=application/json")
                .header("content-type", "text/plain")
                .body(Body::from(b"{}".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(put_resp.status(), StatusCode::OK);
    let put_json: Value = serde_json::from_slice(&body_bytes(put_resp.into_body()).await).unwrap();
    assert_eq!(put_json["media_type"], "application/json");
}

/// GET for a well-formed-but-absent hash -> 404 (never a 5xx, never a panic).
#[tokio::test]
async fn get_unknown_hash_returns_404() {
    let backend = Arc::new(InMemoryBackend::new());
    let app = build_router_with_opts(make_state(backend), true);

    let unknown = "0".repeat(64);
    let resp = app
        .oneshot(
            Request::get(format!("/artifacts/{unknown}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
