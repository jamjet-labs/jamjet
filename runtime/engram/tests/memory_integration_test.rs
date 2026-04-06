use engram::embedding::MockEmbeddingProvider;
use engram::extract::{ExtractionConfig, Message};
use engram::llm::MockLlmClient;
use engram::memory::Memory;
use engram::scope::Scope;
use serde_json::json;

fn user_scope() -> Scope {
    Scope::user("default", "u1")
}

fn mock_extraction_llm() -> MockLlmClient {
    MockLlmClient::new(vec![json!({
        "facts": [
            {
                "text": "User is allergic to peanuts",
                "entities": [
                    {"name": "user_123", "entity_type": "person"},
                    {"name": "peanuts", "entity_type": "allergen"}
                ],
                "relationships": [
                    {"source": "user_123", "relation": "allergic_to", "target": "peanuts"}
                ],
                "confidence": 0.95,
                "category": "health"
            },
            {
                "text": "User lives in Austin",
                "entities": [
                    {"name": "user_123", "entity_type": "person"},
                    {"name": "Austin", "entity_type": "place"}
                ],
                "relationships": [
                    {"source": "user_123", "relation": "lives_in", "target": "Austin"}
                ],
                "confidence": 0.97,
                "category": "location"
            }
        ]
    })])
}

#[tokio::test]
async fn add_messages_extracts_and_stores_facts() {
    let memory = Memory::in_memory(Box::new(MockEmbeddingProvider::new(64)))
        .await
        .unwrap();

    let messages = vec![
        Message::user("I'm allergic to peanuts and I live in Austin"),
        Message::assistant("Got it! I'll keep that in mind."),
    ];

    let llm = mock_extraction_llm();
    let ids = memory
        .add_messages(
            &messages,
            user_scope(),
            Box::new(llm),
            ExtractionConfig::default(),
        )
        .await
        .unwrap();

    assert_eq!(ids.len(), 2);

    // Facts should be stored and retrievable
    let facts = memory.list_facts(Some(user_scope())).await.unwrap();
    assert_eq!(facts.len(), 2);
}

#[tokio::test]
async fn add_messages_creates_entities_and_relationships() {
    let memory = Memory::in_memory(Box::new(MockEmbeddingProvider::new(64)))
        .await
        .unwrap();

    let messages = vec![Message::user(
        "I'm allergic to peanuts and live in Austin",
    )];

    let llm = mock_extraction_llm();
    memory
        .add_messages(
            &messages,
            user_scope(),
            Box::new(llm),
            ExtractionConfig::default(),
        )
        .await
        .unwrap();

    // Entities should be in graph store
    let entities = memory
        .graph_store()
        .search_entities("Austin", 10)
        .await
        .unwrap();
    assert!(!entities.is_empty());
    assert_eq!(entities[0].name, "Austin");
}
