use axum::body::Body;
use axum::http::{Request, StatusCode};
use engram::embedding::MockEmbeddingProvider;
use engram::memory::Memory;
use engram::scope::Scope;
use engram_server::rest::{build_router, AppState};
use http_body_util::BodyExt;
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

fn user_scope() -> Scope {
    Scope::user("default", "u1")
}

async fn test_app() -> (axum::Router, Arc<Memory>) {
    let memory = Memory::in_memory(Box::new(MockEmbeddingProvider::new(64)))
        .await
        .unwrap();
    let memory = Arc::new(memory);
    let state = AppState {
        memory: memory.clone(),
    };
    (build_router(state), memory)
}

async fn body_json(body: Body) -> Value {
    let bytes = body.collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn health_endpoint() {
    let (app, _) = test_app().await;

    let resp = app
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp.into_body()).await;
    assert_eq!(json["status"], "ok");
    assert_eq!(json["service"], "engram");
}

#[tokio::test]
async fn stats_endpoint_returns_zeros() {
    let (app, _) = test_app().await;

    let resp = app
        .oneshot(
            Request::get("/v1/memory/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp.into_body()).await;
    assert_eq!(json["total_facts"], 0);
}

#[tokio::test]
async fn recall_with_empty_store() {
    let (app, _) = test_app().await;

    let resp = app
        .oneshot(
            Request::get("/v1/memory/recall?q=test&user_id=u1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp.into_body()).await;
    assert_eq!(json["total"], 0);
}

#[tokio::test]
async fn search_finds_keyword_match() {
    let (app, memory) = test_app().await;

    memory
        .add_fact("User is allergic to peanuts", user_scope())
        .await
        .unwrap();

    let resp = app
        .oneshot(
            Request::get("/v1/memory/search?q=peanuts&user_id=u1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp.into_body()).await;
    assert_eq!(json["total"], 1);
}

#[tokio::test]
async fn context_returns_block() {
    let (app, memory) = test_app().await;

    memory
        .add_fact("User likes pizza", user_scope())
        .await
        .unwrap();

    let resp = app
        .oneshot(
            Request::post("/v1/memory/context")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "query": "pizza",
                        "user_id": "u1"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp.into_body()).await;
    assert!(json["facts_included"].as_u64().unwrap() > 0);
    assert!(json["text"].as_str().unwrap().contains("<memory>"));
}

#[tokio::test]
async fn forget_and_verify() {
    let (app, memory) = test_app().await;

    let id = memory.add_fact("Temp fact", user_scope()).await.unwrap();

    let resp = app
        .oneshot(
            Request::delete(&format!("/v1/memory/facts/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp.into_body()).await;
    assert_eq!(json["success"], true);
}

#[tokio::test]
async fn delete_user_data() {
    let (app, memory) = test_app().await;

    memory.add_fact("User data", user_scope()).await.unwrap();

    let resp = app
        .oneshot(
            Request::delete("/v1/memory/users/u1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp.into_body()).await;
    assert_eq!(json["success"], true);
}

#[tokio::test]
async fn consolidate_endpoint() {
    let (app, memory) = test_app().await;

    memory.add_fact("A fact", user_scope()).await.unwrap();

    let resp = app
        .oneshot(
            Request::post("/v1/memory/consolidate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({"user_id": "u1"})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp.into_body()).await;
    assert_eq!(json["llm_calls_used"], 0);
}
