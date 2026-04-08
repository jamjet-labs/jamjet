<div align="center">

<h1>⚡ Engram — Rust library</h1>

**Durable memory for AI agents — temporal knowledge graph, hybrid retrieval, SQLite-backed.**

[![crates.io](https://img.shields.io/crates/v/jamjet-engram?style=flat-square&color=f5c518)](https://crates.io/crates/jamjet-engram)
[![License](https://img.shields.io/badge/license-Apache%202.0-f5c518?style=flat-square)](../../LICENSE)

[Server binary](../engram-server/README.md) · [java-ai-memory.dev](https://java-ai-memory.dev) · [JamJet docs](https://docs.jamjet.dev) · [Main repo](https://github.com/jamjet-labs/jamjet)

</div>

---

`jamjet-engram` is the Rust library behind [Engram](https://github.com/jamjet-labs/jamjet/tree/main/runtime/engram-server) — a durable memory layer for AI agents. If you're building a Rust application and want to embed memory directly, depend on this crate. If you want a standalone MCP/REST server, use [`jamjet-engram-server`](../engram-server/README.md) instead.

> **State of the project, April 2026.** Engram is new — v0.3.2, small community, no public benchmark scores yet. The architecture works and the tests pass. See the [server README](../engram-server/README.md) for a full comparison against Mem0, Zep, Spring AI ChatMemory, and the other JVM memory libraries.

## What it does

- **Fact extraction** — pulls structured facts out of raw conversation messages, backed by any `LlmClient`.
- **Temporal knowledge graph** — every fact is scoped (`org` / `user` / `session`) and timestamped.
- **Hybrid retrieval** — vector search (via an `EmbeddingProvider`) + SQLite FTS5 keyword search in one query.
- **Consolidation engine** — decay, promote, dedup, summarize, reflect. All on a schedule you control.
- **Conflict detection** — invalidates superseded facts when contradictory information arrives.
- **Context assembly** — token-budgeted memory blocks ready to inject into a system prompt.
- **Pluggable backends** — ships with `OllamaLlmClient`, `OllamaEmbeddingProvider`, and test-only mocks. Bring your own for OpenAI, Anthropic, etc.
- **Zero infra** — a single SQLite file. No Postgres, no Qdrant, no Neo4j, no Python sidecar.

## Install

```toml
[dependencies]
jamjet-engram = "0.3.2"
```

## Example

```rust
use engram::{
    ExtractionConfig, Memory, Message, OllamaEmbeddingProvider, OllamaLlmClient, Scope,
};
use engram::memory::RecallQuery;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open (or create) a SQLite-backed memory store.
    let embedding = Box::new(OllamaEmbeddingProvider::new()); // localhost:11434, nomic-embed-text, 768d
    let memory = Memory::open("sqlite:engram.db?mode=rwc", embedding).await?;

    // Scope every operation to an org + user.
    let scope = Scope::user("acme", "alice");

    // Extract and store facts from a conversation turn.
    let messages = vec![Message {
        role: "user".into(),
        content: "I'm allergic to peanuts and I love sourdough.".into(),
    }];
    let llm = Box::new(OllamaLlmClient::new()); // llama3.2 by default
    let fact_ids = memory
        .add_messages(&messages, scope.clone(), llm, ExtractionConfig::default())
        .await?;
    println!("stored {} facts", fact_ids.len());

    // Recall relevant facts later.
    let facts = memory
        .recall(&RecallQuery {
            query: "food allergies".into(),
            scope: Some(scope),
            max_results: 5,
            as_of: None,
            min_score: None,
        })
        .await?;
    for fact in facts {
        println!("{}: {}", fact.tier, fact.text);
    }

    Ok(())
}
```

## Built-in LLM clients

All implement `LlmClient` (`engram::llm`) and are ready to drop into `Memory::add_messages`:

| Client | Constructor | Default model | Notes |
|---|---|---|---|
| `OllamaLlmClient` | `::new()` → `localhost:11434` | `llama3.2` | Local, free; no API key |
| `OpenAiLlmClient` | `::new(api_key)` | `gpt-4o-mini` | OpenAI, Azure, Groq, Together, Mistral, DeepSeek, Perplexity, OpenRouter, Fireworks, vLLM, LM Studio — anything speaking OpenAI chat-completions. Use `with_config(base_url, key, model)`. |
| `AnthropicLlmClient` | `::new(api_key)` | `claude-haiku-4-5-20251001` | Anthropic Messages API |
| `GoogleLlmClient` | `::new(api_key)` | `gemini-flash-latest` | Gemini `generateContent` with native JSON mode |
| `CommandLlmClient` | `::new(command)` | — | Shell-out escape hatch. Wraps any provider via stdin/stdout JSON. See module docs for the contract. |
| `MockLlmClient` | `::new(responses)` | — | For tests; returns canned responses |

For embeddings, `OllamaEmbeddingProvider` is the only real provider shipped today. `EmbeddingProvider` is a simple trait so wrapping OpenAI `text-embedding-3`, Cohere, or a local ONNX model takes a few lines.

Both traits are `Send + Sync + async`.

### Wrapping a new provider without forking Engram

For providers that don't fit the built-ins — a corporate LLM gateway, an exotic RPC, a research model — use `CommandLlmClient` to shell out to your own 15-line script in any language:

```rust
use engram::CommandLlmClient;
let llm = CommandLlmClient::new("python /path/to/my-wrapper.py").with_timeout(60);
// Pass as Box<dyn LlmClient> to Memory::add_messages.
```

Your script reads `{"system": "...", "user": "...", "structured": true}` from stdin and writes either the raw JSON value or an envelope `{"content": {...}}` / `{"error": "..."}` to stdout. See the `llm_command` module docs for the full contract.

## Core types

| Type | Purpose |
|------|---------|
| `Memory` | The main handle. Owns the embedding provider, exposes `add_messages`, `recall`, `context`, `forget`, `consolidate`, `stats` |
| `Scope` | `org` / `user` / `session` — the axis every query is filtered on |
| `Fact`, `FactId`, `MemoryTier` | The unit of knowledge, with a tier (`short`, `long`, `core`) and confidence |
| `Entity`, `Relationship` | The graph layer on top of facts |
| `ContextBuilder`, `ContextBlock` | Token-budgeted prompt assembly |
| `ConsolidationEngine`, `ConsolidationOp` | Background maintenance of the memory store |

## License

Apache 2.0 — see [LICENSE](../../LICENSE).

---

<div align="center">
  <sub>Part of <a href="https://jamjet.dev">JamJet</a> · Built by <a href="https://github.com/sunilp">Sunil Prakash</a> · © 2026 JamJet Labs</sub>
</div>
