use engram::context::{ContextBuilder, ContextConfig, OutputFormat};
use engram::embedding::MockEmbeddingProvider;
use engram::memory::Memory;
use engram::scope::Scope;
use std::sync::Arc;

fn user_scope() -> Scope {
    Scope::user("default", "u1")
}

async fn seeded_memory() -> Memory {
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
        .add_fact("User prefers dark mode", user_scope())
        .await
        .unwrap();

    memory
}

#[tokio::test]
async fn build_returns_context_block_with_facts() {
    let memory = seeded_memory().await;

    let builder = ContextBuilder::new(
        memory.fact_store().clone(),
        memory.vector_store().clone(),
        memory.graph_store().clone(),
        Arc::new(MockEmbeddingProvider::new(64)),
        ContextConfig::default(),
    );

    let block = builder
        .build("peanut allergy", &user_scope())
        .await
        .unwrap();

    assert!(block.facts_included > 0);
    assert!(block.token_count > 0);
    assert!(!block.text.is_empty());
}

#[tokio::test]
async fn system_prompt_format_produces_xml_tags() {
    let memory = seeded_memory().await;

    let builder = ContextBuilder::new(
        memory.fact_store().clone(),
        memory.vector_store().clone(),
        memory.graph_store().clone(),
        Arc::new(MockEmbeddingProvider::new(64)),
        ContextConfig {
            format: OutputFormat::SystemPrompt,
            ..Default::default()
        },
    );

    let block = builder.build("peanuts", &user_scope()).await.unwrap();

    assert!(block.text.contains("<memory>"));
    assert!(block.text.contains("</memory>"));
}

#[tokio::test]
async fn markdown_format_produces_sections() {
    let memory = seeded_memory().await;

    let builder = ContextBuilder::new(
        memory.fact_store().clone(),
        memory.vector_store().clone(),
        memory.graph_store().clone(),
        Arc::new(MockEmbeddingProvider::new(64)),
        ContextConfig {
            format: OutputFormat::Markdown,
            ..Default::default()
        },
    );

    let block = builder.build("peanuts", &user_scope()).await.unwrap();

    assert!(block.text.contains("## Memory Context"));
}

#[tokio::test]
async fn raw_format_produces_valid_json() {
    let memory = seeded_memory().await;

    let builder = ContextBuilder::new(
        memory.fact_store().clone(),
        memory.vector_store().clone(),
        memory.graph_store().clone(),
        Arc::new(MockEmbeddingProvider::new(64)),
        ContextConfig {
            format: OutputFormat::Raw,
            ..Default::default()
        },
    );

    let block = builder.build("peanuts", &user_scope()).await.unwrap();

    let parsed: Vec<serde_json::Value> = serde_json::from_str(&block.text).unwrap();
    assert!(!parsed.is_empty());
}

#[tokio::test]
async fn token_budget_is_respected() {
    let memory = seeded_memory().await;

    let builder = ContextBuilder::new(
        memory.fact_store().clone(),
        memory.vector_store().clone(),
        memory.graph_store().clone(),
        Arc::new(MockEmbeddingProvider::new(64)),
        ContextConfig {
            token_budget: 30,
            ..Default::default()
        },
    );

    let block = builder.build("peanuts", &user_scope()).await.unwrap();

    assert!(block.token_count <= 30);
}

#[tokio::test]
async fn tier_breakdown_is_populated() {
    let memory = seeded_memory().await;

    let builder = ContextBuilder::new(
        memory.fact_store().clone(),
        memory.vector_store().clone(),
        memory.graph_store().clone(),
        Arc::new(MockEmbeddingProvider::new(64)),
        ContextConfig::default(),
    );

    let block = builder.build("peanuts", &user_scope()).await.unwrap();

    let total: usize = block.tier_breakdown.values().sum();
    assert_eq!(total, block.facts_included);
}

#[tokio::test]
async fn empty_store_returns_empty_block() {
    let memory = Memory::in_memory(Box::new(MockEmbeddingProvider::new(64)))
        .await
        .unwrap();

    let builder = ContextBuilder::new(
        memory.fact_store().clone(),
        memory.vector_store().clone(),
        memory.graph_store().clone(),
        Arc::new(MockEmbeddingProvider::new(64)),
        ContextConfig::default(),
    );

    let block = builder.build("anything", &user_scope()).await.unwrap();

    assert_eq!(block.facts_included, 0);
    assert_eq!(block.facts_omitted, 0);
}
