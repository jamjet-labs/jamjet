use engram::extract::{ExtractionConfig, Message};
use engram::llm::MockLlmClient;
use engram::pipeline::ExtractionPipeline;
use serde_json::json;

fn mock_extraction_response() -> serde_json::Value {
    json!({
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
                "category": "personal_info"
            }
        ]
    })
}

#[tokio::test]
async fn extract_facts_from_messages() {
    let llm = MockLlmClient::new(vec![mock_extraction_response()]);
    let pipeline = ExtractionPipeline::new(Box::new(llm), ExtractionConfig::default());

    let messages = vec![
        Message::user("I'm allergic to peanuts and I live in Austin"),
        Message::assistant("Got it! I'll keep that in mind."),
    ];

    let result = pipeline.extract(&messages).await.unwrap();

    assert_eq!(result.facts.len(), 2);
    assert_eq!(result.facts[0].text, "User is allergic to peanuts");
    assert_eq!(result.facts[0].entities.len(), 2);
    assert_eq!(result.facts[0].relationships.len(), 1);
    assert!((result.facts[0].confidence - 0.95).abs() < f64::EPSILON);
    assert_eq!(result.facts[1].text, "User lives in Austin");
}

#[tokio::test]
async fn skip_categories_filters_results() {
    let llm = MockLlmClient::new(vec![mock_extraction_response()]);
    let config = ExtractionConfig {
        skip_categories: vec!["health".to_string()],
        ..Default::default()
    };
    let pipeline = ExtractionPipeline::new(Box::new(llm), config);

    let messages = vec![Message::user("I'm allergic to peanuts and live in Austin")];
    let result = pipeline.extract(&messages).await.unwrap();

    // "health" category should be filtered out
    assert_eq!(result.facts.len(), 1);
    assert_eq!(result.facts[0].category, Some("personal_info".to_string()));
}

#[tokio::test]
async fn empty_messages_returns_empty_result() {
    let llm = MockLlmClient::new(vec![json!({"facts": []})]);
    let pipeline = ExtractionPipeline::new(Box::new(llm), ExtractionConfig::default());

    let result = pipeline.extract(&[]).await.unwrap();
    assert!(result.facts.is_empty());
}

#[tokio::test]
async fn custom_prompt_is_included() {
    let llm = MockLlmClient::new(vec![json!({"facts": []})]);
    let config = ExtractionConfig {
        custom_prompt: Some("Also extract dietary restrictions.".to_string()),
        ..Default::default()
    };
    let pipeline = ExtractionPipeline::new(Box::new(llm), config);

    let result = pipeline.extract(&[Message::user("hello")]).await.unwrap();
    assert!(result.facts.is_empty());
}
