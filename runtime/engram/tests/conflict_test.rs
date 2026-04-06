use engram::conflict::ConflictDetector;
use engram::embedding::{EmbeddingProvider, MockEmbeddingProvider};
use engram::extract::ConflictVerdict;
use engram::fact::Fact;
use engram::llm::MockLlmClient;
use engram::scope::Scope;
use engram::store::FactStore;
use engram::store_sqlite::SqliteFactStore;
use engram::vector::VectorStore;
use engram::vector_embedded::EmbeddedVectorStore;
use serde_json::json;
use std::sync::Arc;

fn user_scope() -> Scope {
    Scope::user("default", "u1")
}

#[tokio::test]
async fn no_conflict_for_new_fact() {
    let embedding: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbeddingProvider::new(64));
    let fact_store = SqliteFactStore::open("sqlite::memory:").await.unwrap();
    fact_store.migrate().await.unwrap();
    let fact_store: Arc<dyn FactStore> = Arc::new(fact_store);
    let vector_store: Arc<dyn VectorStore> = Arc::new(EmbeddedVectorStore::new(64));
    let llm = MockLlmClient::new(vec![]);

    let detector = ConflictDetector::new(
        fact_store,
        vector_store,
        embedding,
        Box::new(llm),
        0.85,
    );

    let verdict = detector
        .check("User likes pizza", &user_scope())
        .await
        .unwrap();

    assert_eq!(verdict.verdict, ConflictVerdict::NoConflict);
    assert!(verdict.existing_fact_id.is_none());
}

#[tokio::test]
async fn detects_duplicate_by_high_similarity() {
    let embedding: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbeddingProvider::new(64));
    let fact_store = SqliteFactStore::open("sqlite::memory:").await.unwrap();
    fact_store.migrate().await.unwrap();
    let fact_store: Arc<dyn FactStore> = Arc::new(fact_store);
    let vector_store: Arc<dyn VectorStore> = Arc::new(EmbeddedVectorStore::new(64));

    // Insert existing fact
    let existing = Fact::new("User likes pizza", user_scope());
    let existing_id = existing.id;
    fact_store.insert_fact(existing).await.unwrap();
    let emb = embedding.embed(&["User likes pizza"]).await.unwrap();
    vector_store
        .upsert(existing_id, emb[0].clone(), json!({}))
        .await
        .unwrap();

    // LLM not needed for exact duplicate (sim > 0.99)
    let llm = MockLlmClient::new(vec![]);
    let detector = ConflictDetector::new(
        fact_store,
        vector_store,
        embedding,
        Box::new(llm),
        0.85,
    );

    // Same text = exact same embedding = similarity 1.0
    let verdict = detector
        .check("User likes pizza", &user_scope())
        .await
        .unwrap();

    assert_eq!(verdict.verdict, ConflictVerdict::Duplicate);
    assert_eq!(verdict.existing_fact_id, Some(existing_id));
}
