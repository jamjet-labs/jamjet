//! The REST API's bearer auth, the conversation route, and forget and stats
//! staying inside the caller's scope.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use engram::embedding::MockEmbeddingProvider;
use engram::memory::Memory;
use engram::scope::Scope;
use engram_server::config::LlmBackend;
use engram_server::rest::{build_router, build_router_with_auth, AppState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const TOKEN: &str = "3f9a1c0e7b2d4a6f8e1c3b5d7f9a2c4e";

async fn memory() -> Arc<Memory> {
    Arc::new(
        Memory::in_memory(Box::new(MockEmbeddingProvider::new(64)))
            .await
            .unwrap(),
    )
}

fn state(memory: &Arc<Memory>) -> AppState {
    AppState {
        memory: memory.clone(),
        llm_backend: LlmBackend::Mock,
        extract_on_save: false,
    }
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value, Option<String>) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let challenge = resp
        .headers()
        .get("www-authenticate")
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        challenge,
    )
}

/// With a token, a request without it, or with another, is refused, and
/// nothing is deleted. The REST API had no auth: anyone who reached the port
/// could delete facts and users.
#[tokio::test]
async fn a_delete_without_the_token_is_refused() {
    let memory = memory().await;
    let id = memory
        .add_fact("keep me", Scope::user("default", "u1"))
        .await
        .unwrap();
    let app = build_router_with_auth(state(&memory), Some(TOKEN.into()));
    let delete = |auth: Option<&str>| {
        let mut req = Request::delete(format!("/v1/memory/facts/{id}"));
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        req.body(Body::empty()).unwrap()
    };
    for auth in [
        None,
        Some("Bearer wrong-token-wrong-token-1234"),
        Some(TOKEN),
    ] {
        let (status, _, challenge) = send(&app, delete(auth)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{auth:?}");
        assert_eq!(challenge.as_deref(), Some("Bearer realm=\"engram\""));
    }
    let fact = memory.fact_store().get_fact(id).await.unwrap();
    assert!(fact.invalid_at.is_none(), "nothing was deleted");

    // The scheme is case-insensitive, and the right token works.
    let (status, _, _) = send(&app, delete(Some(&format!("bearer {TOKEN}")))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn health_needs_no_token() {
    let memory = memory().await;
    let app = build_router_with_auth(state(&memory), Some(TOKEN.into()));
    let (status, body, _) = send(&app, Request::get("/health").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

/// `GET /v1/memory/messages/:id` answers. It was registered with axum 0.8's
/// `{conversation_id}` syntax on axum 0.7, a literal path, so it was 404.
#[tokio::test]
async fn a_saved_conversation_can_be_read_and_deleted() {
    let memory = memory().await;
    let app = build_router(state(&memory));
    let save = Request::post("/v1/memory/messages")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "conversation_id": "c1",
                "user_id": "u1",
                "messages": [{ "role": "user", "content": "hello" }]
            })
            .to_string(),
        ))
        .unwrap();
    let (status, _, _) = send(&app, save).await;
    assert!(status.is_success(), "{status}");

    let (status, body, _) = send(
        &app,
        Request::get("/v1/memory/messages/c1?user_id=u1")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.to_string().contains("hello"), "{body}");

    let (status, _, _) = send(
        &app,
        Request::delete("/v1/memory/messages/c1?user_id=u1")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// A fact in another org cannot be forgotten, over MCP or REST, and reads as
/// not found. `memory_forget` took a bare id.
#[tokio::test]
async fn forget_stays_inside_the_callers_org() {
    let memory = memory().await;
    let id = memory
        .add_fact("acme fact", Scope::user("acme", "u1"))
        .await
        .unwrap();

    let err = engram_server::handlers::handle_forget(
        memory.clone(),
        json!({ "fact_id": id.to_string(), "org_id": "other" }),
    )
    .await
    .unwrap_err();
    assert!(err.contains("not found"), "{err}");
    // No org named: the default org, which does not hold it either.
    assert!(engram_server::handlers::handle_forget(
        memory.clone(),
        json!({ "fact_id": id.to_string() })
    )
    .await
    .is_err());
    // Another user of the same org.
    assert!(engram_server::handlers::handle_forget(
        memory.clone(),
        json!({ "fact_id": id.to_string(), "org_id": "acme", "user_id": "u2" })
    )
    .await
    .is_err());

    let app = build_router(state(&memory));
    let (status, _, _) = send(
        &app,
        Request::delete(format!("/v1/memory/facts/{id}?org_id=other"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(memory
        .fact_store()
        .get_fact(id)
        .await
        .unwrap()
        .invalid_at
        .is_none());

    // Its own org can.
    engram_server::handlers::handle_forget(
        memory.clone(),
        json!({ "fact_id": id.to_string(), "org_id": "acme" }),
    )
    .await
    .unwrap();
    assert!(memory
        .fact_store()
        .get_fact(id)
        .await
        .unwrap()
        .invalid_at
        .is_some());
}

/// Stats count the caller's org only. They counted the whole store.
#[tokio::test]
async fn stats_count_only_the_callers_org() {
    let memory = memory().await;
    memory
        .add_fact("a1", Scope::user("acme", "u1"))
        .await
        .unwrap();
    memory
        .add_fact("a2", Scope::user("acme", "u2"))
        .await
        .unwrap();
    memory
        .add_fact("o1", Scope::user("other", "u1"))
        .await
        .unwrap();

    let out = engram_server::handlers::handle_stats(memory.clone(), json!({ "org_id": "acme" }))
        .await
        .unwrap();
    let stats: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(stats["total_facts"], 2, "{stats}");
    let out = engram_server::handlers::handle_stats(
        memory.clone(),
        json!({ "org_id": "acme", "user_id": "u1" }),
    )
    .await
    .unwrap();
    let stats: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(stats["total_facts"], 1, "{stats}");

    let app = build_router(state(&memory));
    let (_, body, _) = send(
        &app,
        Request::get("/v1/memory/stats?org_id=other")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(body["total_facts"], 1, "{body}");
}
