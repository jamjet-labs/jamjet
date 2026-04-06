use engram::embedding::MockEmbeddingProvider;
use engram::memory::Memory;
use engram::scope::Scope;
use serde_json::{json, Value};
use std::sync::Arc;

async fn test_memory() -> Arc<Memory> {
    let memory = Memory::in_memory(Box::new(MockEmbeddingProvider::new(64)))
        .await
        .unwrap();
    Arc::new(memory)
}

fn user_scope() -> Scope {
    Scope::user("default", "u1")
}

#[tokio::test]
async fn stats_returns_counts() {
    let memory = test_memory().await;
    let result = engram_server::handlers::handle_stats(memory, json!({}))
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["total_facts"], 0);
    assert_eq!(parsed["valid_facts"], 0);
}

#[tokio::test]
async fn recall_with_empty_store() {
    let memory = test_memory().await;
    let result = engram_server::handlers::handle_recall(
        memory,
        json!({"query": "test", "user_id": "u1"}),
    )
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["total"], 0);
}

#[tokio::test]
async fn context_returns_block() {
    let memory = test_memory().await;

    memory
        .add_fact("User likes pizza", user_scope())
        .await
        .unwrap();

    let result = engram_server::handlers::handle_context(
        memory,
        json!({"query": "pizza", "user_id": "u1"}),
    )
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert!(parsed["facts_included"].as_u64().unwrap() > 0);
    assert!(parsed["text"].as_str().unwrap().contains("<memory>"));
}

#[tokio::test]
async fn search_finds_keyword_match() {
    let memory = test_memory().await;

    memory
        .add_fact("User is allergic to peanuts", user_scope())
        .await
        .unwrap();

    let result = engram_server::handlers::handle_search(
        memory,
        json!({"query": "peanuts", "user_id": "u1"}),
    )
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["total"], 1);
}

#[tokio::test]
async fn forget_and_verify() {
    let memory = test_memory().await;

    let id = memory
        .add_fact("Temp fact", user_scope())
        .await
        .unwrap();

    let result = engram_server::handlers::handle_forget(
        memory.clone(),
        json!({"fact_id": id.to_string()}),
    )
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["success"], true);

    // Verify fact is gone from keyword search
    let search = engram_server::handlers::handle_search(
        memory,
        json!({"query": "Temp", "user_id": "u1"}),
    )
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&search).unwrap();
    assert_eq!(parsed["total"], 0);
}

#[tokio::test]
async fn add_with_empty_messages_errors() {
    let memory = test_memory().await;
    let result =
        engram_server::handlers::handle_add(memory, json!({"messages": []})).await;
    assert!(result.is_err());
}
