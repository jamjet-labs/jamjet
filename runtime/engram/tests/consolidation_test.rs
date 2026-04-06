use engram::consolidation::{ConsolidationConfig, ConsolidationEngine, ConsolidationOp};
use engram::embedding::MockEmbeddingProvider;
use engram::fact::{Fact, FactPatch, MemoryTier};
use engram::llm::MockLlmClient;
use engram::memory::Memory;
use engram::scope::Scope;
use engram::store::FactStore;
use serde_json::json;
use std::sync::Arc;

fn user_scope() -> Scope {
    Scope::user("default", "u1")
}

async fn test_memory() -> Memory {
    Memory::in_memory(Box::new(MockEmbeddingProvider::new(64)))
        .await
        .unwrap()
}

#[tokio::test]
async fn decay_reduces_confidence_of_stale_facts() {
    let memory = test_memory().await;

    // Add a knowledge fact with old last_accessed
    let id = memory
        .add_fact("Old knowledge fact", user_scope())
        .await
        .unwrap();
    let patch = FactPatch {
        tier: Some(MemoryTier::Knowledge),
        confidence: Some(0.95),
        ..Default::default()
    };
    memory.fact_store().update_fact(id, patch).await.unwrap();

    // Small sleep to ensure some time has elapsed for decay math
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    let config = ConsolidationConfig {
        enabled_ops: vec![ConsolidationOp::Decay],
        half_life_days: 0.0000001, // ~8.6ms — even milliseconds trigger significant decay
        ..Default::default()
    };

    let engine = ConsolidationEngine::new(
        memory.fact_store().clone(),
        memory.vector_store().clone(),
        Arc::new(MockEmbeddingProvider::new(64)),
        config,
    );

    let result = engine.run(&user_scope(), None).await.unwrap();
    assert!(result.facts_decayed > 0);
}

#[tokio::test]
async fn promote_upgrades_frequently_accessed_facts() {
    let memory = test_memory().await;

    let id = memory.add_fact("Popular fact", user_scope()).await.unwrap();

    // Simulate 3 accesses
    for _ in 0..3 {
        memory.fact_store().record_access(id).await.unwrap();
    }

    let config = ConsolidationConfig {
        enabled_ops: vec![ConsolidationOp::Promote],
        promote_access_count: 3,
        ..Default::default()
    };

    let engine = ConsolidationEngine::new(
        memory.fact_store().clone(),
        memory.vector_store().clone(),
        Arc::new(MockEmbeddingProvider::new(64)),
        config,
    );

    let result = engine.run(&user_scope(), None).await.unwrap();
    assert_eq!(result.facts_promoted, 1);

    // Verify tier changed
    let fact = memory.fact_store().get_fact(id).await.unwrap();
    assert_eq!(fact.tier, MemoryTier::Knowledge);
}

#[tokio::test]
async fn promote_skips_low_access_facts() {
    let memory = test_memory().await;
    memory
        .add_fact("Rarely accessed", user_scope())
        .await
        .unwrap();

    let config = ConsolidationConfig {
        enabled_ops: vec![ConsolidationOp::Promote],
        promote_access_count: 3,
        ..Default::default()
    };

    let engine = ConsolidationEngine::new(
        memory.fact_store().clone(),
        memory.vector_store().clone(),
        Arc::new(MockEmbeddingProvider::new(64)),
        config,
    );

    let result = engine.run(&user_scope(), None).await.unwrap();
    assert_eq!(result.facts_promoted, 0);
}

#[tokio::test]
async fn dedup_merges_near_identical_facts() {
    let memory = test_memory().await;

    // Add two facts with identical text (same embedding = sim 1.0)
    memory
        .add_fact("User likes pizza", user_scope())
        .await
        .unwrap();
    memory
        .add_fact("User likes pizza", user_scope())
        .await
        .unwrap();

    let config = ConsolidationConfig {
        enabled_ops: vec![ConsolidationOp::Dedup],
        dedup_similarity: 0.95,
        ..Default::default()
    };

    let engine = ConsolidationEngine::new(
        memory.fact_store().clone(),
        memory.vector_store().clone(),
        Arc::new(MockEmbeddingProvider::new(64)),
        config,
    );

    let result = engine.run(&user_scope(), None).await.unwrap();
    assert_eq!(result.facts_deduped, 1);
}

#[tokio::test]
async fn summarize_skips_when_below_threshold() {
    let memory = test_memory().await;
    memory
        .add_fact("Just one fact", user_scope())
        .await
        .unwrap();

    let llm = MockLlmClient::new(vec![]);
    let config = ConsolidationConfig {
        enabled_ops: vec![ConsolidationOp::Summarize],
        summarize_threshold: 100,
        ..Default::default()
    };

    let engine = ConsolidationEngine::new(
        memory.fact_store().clone(),
        memory.vector_store().clone(),
        Arc::new(MockEmbeddingProvider::new(64)),
        config,
    );

    let result = engine.run(&user_scope(), Some(&llm)).await.unwrap();
    assert_eq!(result.facts_summarized, 0);
    assert_eq!(result.llm_calls_used, 0);
}

#[tokio::test]
async fn reflect_generates_insights() {
    let memory = test_memory().await;

    // Add enough knowledge facts to trigger reflection
    for i in 0..12 {
        let id = memory
            .add_fact(&format!("Knowledge fact {i}"), user_scope())
            .await
            .unwrap();
        let patch = FactPatch {
            tier: Some(MemoryTier::Knowledge),
            ..Default::default()
        };
        memory.fact_store().update_fact(id, patch).await.unwrap();
    }

    let llm = MockLlmClient::new(vec![json!({
        "insights": ["User has broad interests"]
    })]);

    let config = ConsolidationConfig {
        enabled_ops: vec![ConsolidationOp::Reflect],
        reflect_min_facts: 10,
        ..Default::default()
    };

    let engine = ConsolidationEngine::new(
        memory.fact_store().clone(),
        memory.vector_store().clone(),
        Arc::new(MockEmbeddingProvider::new(64)),
        config,
    );

    let result = engine.run(&user_scope(), Some(&llm)).await.unwrap();
    assert_eq!(result.insights_generated, 1);
    assert_eq!(result.llm_calls_used, 1);
}

#[tokio::test]
async fn reflect_skips_when_too_few_facts() {
    let memory = test_memory().await;

    let id = memory.add_fact("Only fact", user_scope()).await.unwrap();
    let patch = FactPatch {
        tier: Some(MemoryTier::Knowledge),
        ..Default::default()
    };
    memory.fact_store().update_fact(id, patch).await.unwrap();

    let llm = MockLlmClient::new(vec![]);
    let config = ConsolidationConfig {
        enabled_ops: vec![ConsolidationOp::Reflect],
        reflect_min_facts: 10,
        ..Default::default()
    };

    let engine = ConsolidationEngine::new(
        memory.fact_store().clone(),
        memory.vector_store().clone(),
        Arc::new(MockEmbeddingProvider::new(64)),
        config,
    );

    let result = engine.run(&user_scope(), Some(&llm)).await.unwrap();
    assert_eq!(result.insights_generated, 0);
}

#[tokio::test]
async fn full_cycle_runs_all_ops() {
    let memory = test_memory().await;

    memory.add_fact("A fact", user_scope()).await.unwrap();

    let config = ConsolidationConfig::default();
    let engine = ConsolidationEngine::new(
        memory.fact_store().clone(),
        memory.vector_store().clone(),
        Arc::new(MockEmbeddingProvider::new(64)),
        config,
    );

    // Should not error even with no LLM (LLM ops are skipped)
    let result = engine.run(&user_scope(), None).await.unwrap();
    assert_eq!(result.llm_calls_used, 0);
}
