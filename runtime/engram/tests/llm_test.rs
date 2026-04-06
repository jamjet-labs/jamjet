use engram::llm::{LlmClient, MockLlmClient};
use serde_json::json;

#[tokio::test]
async fn mock_returns_structured_responses_in_order() {
    let mock = MockLlmClient::new(vec![
        json!({"answer": "second"}),
        json!({"answer": "first"}),
    ]);
    // Pop order is LIFO (Vec::pop)
    let r1 = mock.structured_output("sys", "user").await.unwrap();
    assert_eq!(r1["answer"], "first");
    let r2 = mock.structured_output("sys", "user").await.unwrap();
    assert_eq!(r2["answer"], "second");
}

#[tokio::test]
async fn mock_complete_returns_string() {
    let mock = MockLlmClient::new(vec![json!("hello world")]);
    let result = mock.complete("sys", "user").await.unwrap();
    assert_eq!(result, "hello world");
}

#[tokio::test]
async fn mock_errors_when_exhausted() {
    let mock = MockLlmClient::new(vec![]);
    let result = mock.structured_output("sys", "user").await;
    assert!(result.is_err());
}
