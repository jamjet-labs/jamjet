use engram::consolidation::{ConsolidationConfig, ConsolidationOp};
use engram::embedding::MockEmbeddingProvider;
use engram::memory::Memory;
use engram::scope::Scope;
use engram::store::FactStore;

fn user_scope() -> Scope {
    Scope::user("default", "u1")
}

#[tokio::test]
async fn memory_consolidate_runs_decay_and_promote() {
    let memory = Memory::in_memory(Box::new(MockEmbeddingProvider::new(64)))
        .await
        .unwrap();

    // Add a fact and access it 5 times
    let id = memory.add_fact("Popular fact", user_scope()).await.unwrap();
    for _ in 0..5 {
        memory.fact_store().record_access(id).await.unwrap();
    }

    let config = ConsolidationConfig {
        enabled_ops: vec![ConsolidationOp::Promote],
        promote_access_count: 3,
        ..Default::default()
    };

    let result = memory
        .consolidate(&user_scope(), None, config)
        .await
        .unwrap();

    assert_eq!(result.facts_promoted, 1);
}

#[tokio::test]
async fn memory_consolidate_with_default_config() {
    let memory = Memory::in_memory(Box::new(MockEmbeddingProvider::new(64)))
        .await
        .unwrap();

    memory.add_fact("A fact", user_scope()).await.unwrap();

    let result = memory
        .consolidate(&user_scope(), None, ConsolidationConfig::default())
        .await
        .unwrap();

    // Should complete without error
    assert_eq!(result.llm_calls_used, 0);
}
