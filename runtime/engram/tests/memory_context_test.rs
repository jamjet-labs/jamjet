use engram::context::{ContextConfig, OutputFormat};
use engram::embedding::MockEmbeddingProvider;
use engram::memory::Memory;
use engram::scope::Scope;

fn user_scope() -> Scope {
    Scope::user("default", "u1")
}

#[tokio::test]
async fn memory_context_returns_block() {
    let memory = Memory::in_memory(Box::new(MockEmbeddingProvider::new(64)))
        .await
        .unwrap();

    memory
        .add_fact("User is allergic to peanuts", user_scope())
        .await
        .unwrap();
    memory
        .add_fact("User lives in Austin", user_scope())
        .await
        .unwrap();

    let block = memory
        .context("peanut allergy", &user_scope(), ContextConfig::default())
        .await
        .unwrap();

    assert!(block.facts_included > 0);
    assert!(block.text.contains("<memory>"));
}

#[tokio::test]
async fn memory_context_with_markdown_format() {
    let memory = Memory::in_memory(Box::new(MockEmbeddingProvider::new(64)))
        .await
        .unwrap();

    memory
        .add_fact("User prefers dark mode", user_scope())
        .await
        .unwrap();

    let config = ContextConfig {
        format: OutputFormat::Markdown,
        ..Default::default()
    };

    let block = memory
        .context("preferences", &user_scope(), config)
        .await
        .unwrap();

    assert!(block.text.contains("## Memory Context"));
}

#[tokio::test]
async fn memory_context_respects_budget() {
    let memory = Memory::in_memory(Box::new(MockEmbeddingProvider::new(64)))
        .await
        .unwrap();

    for i in 0..10 {
        memory
            .add_fact(&format!("Fact number {i} about the user"), user_scope())
            .await
            .unwrap();
    }

    let small_budget = ContextConfig {
        token_budget: 40,
        ..Default::default()
    };

    let block = memory
        .context("user", &user_scope(), small_budget)
        .await
        .unwrap();

    assert!(block.token_count <= 40);
    assert!(block.facts_included < 10);
}
