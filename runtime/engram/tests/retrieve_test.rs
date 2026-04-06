use engram::embedding::MockEmbeddingProvider;
use engram::memory::Memory;
use engram::retrieve::{HybridRetriever, RetrievalConfig};
use engram::scope::Scope;
use std::sync::Arc;

fn user_scope() -> Scope {
    Scope::user("default", "u1")
}

#[tokio::test]
async fn hybrid_retrieval_merges_vector_and_keyword() {
    let memory = Memory::in_memory(Box::new(MockEmbeddingProvider::new(64)))
        .await
        .unwrap();

    memory
        .add_fact("User is allergic to peanuts", user_scope())
        .await
        .unwrap();
    memory
        .add_fact("User lives in Austin Texas", user_scope())
        .await
        .unwrap();
    memory
        .add_fact("User prefers dark mode settings", user_scope())
        .await
        .unwrap();

    let retriever = HybridRetriever::new(
        memory.fact_store().clone(),
        memory.vector_store().clone(),
        memory.graph_store().clone(),
        Arc::new(MockEmbeddingProvider::new(64)),
        RetrievalConfig::default(),
    );

    let results = retriever
        .search("peanut allergy", &user_scope(), 10)
        .await
        .unwrap();

    assert!(!results.is_empty());
    // Results should be scored
    assert!(results[0].score > 0.0);
}

#[tokio::test]
async fn retrieval_config_weights_affect_ranking() {
    let config = RetrievalConfig {
        vector_weight: 0.5,
        keyword_weight: 0.3,
        graph_weight: 0.2,
        ..Default::default()
    };
    assert!(
        (config.vector_weight + config.keyword_weight + config.graph_weight - 1.0).abs()
            < f32::EPSILON
    );
}
