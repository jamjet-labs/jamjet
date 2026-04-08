<div align="center">

<h1>⚡ Engram</h1>

**Durable memory for AI agents — temporal knowledge graph, hybrid retrieval, SQLite-backed.**

[![crates.io (lib)](https://img.shields.io/crates/v/jamjet-engram?label=jamjet-engram&style=flat-square&color=f5c518)](https://crates.io/crates/jamjet-engram)
[![crates.io (server)](https://img.shields.io/crates/v/jamjet-engram-server?label=jamjet-engram-server&style=flat-square&color=f5c518)](https://crates.io/crates/jamjet-engram-server)
[![Docker](https://img.shields.io/badge/ghcr.io-engram--server-f5c518?style=flat-square&logo=docker)](https://github.com/jamjet-labs/jamjet/pkgs/container/engram-server)
[![MCP Registry](https://img.shields.io/badge/MCP%20Registry-engram--server-5865F2?style=flat-square)](https://registry.modelcontextprotocol.io/servers/io.github.jamjet-labs/engram-server)
[![License](https://img.shields.io/badge/license-Apache%202.0-f5c518?style=flat-square)](../../LICENSE)

[java-ai-memory.dev](https://java-ai-memory.dev) · [Main repo](https://github.com/jamjet-labs/jamjet) · [JamJet docs](https://docs.jamjet.dev) · [Discord](https://discord.gg/SAYnEj86fr)

</div>

---

Engram is a **durable memory layer for AI agents**. It extracts facts from conversations, stores them in a temporal knowledge graph, and retrieves them with hybrid semantic + keyword search — all backed by a single SQLite file.

It ships in two shapes:

- **`jamjet-engram`** — a Rust library you embed in your own application.
- **`jamjet-engram-server`** (this crate) — a standalone binary that speaks **MCP over stdio** and **REST over HTTP**, so Claude Desktop, Cursor, and any HTTP client can use it with no code.

Engram is **provider-agnostic**. Five LLM backends are wired in out of the box, and a sixth — `command` — lets you shell out to any external script, so you can plug in a provider Engram does not ship natively without touching Rust code:

| `ENGRAM_LLM_PROVIDER=` | What it does |
|---|---|
| `ollama` (default) | Local Ollama via `/api/chat`. Free, no API keys, runs on your laptop. |
| `openai-compatible` | **Any endpoint that speaks OpenAI's chat-completions protocol** — see the long list below. |
| `anthropic` | Anthropic Claude via the Messages API. |
| `google` | Google Gemini via `generateContent` with native JSON mode. |
| `command` | Shell out to a user-supplied script. Infinite extensibility, zero recompile. |
| `mock` | Deterministic tests-only backend — returns empty facts. |

Pick one with `ENGRAM_LLM_PROVIDER=…` — the same binary handles all of them.

> **State of the project, April 2026.** Engram is new — v0.3.2, small community, no public LongMemEval / DMR numbers yet. The architecture below works, the tests pass, the Docker image runs. If you need production-scale memory today, Mem0 Cloud and Zep Cloud are more mature. If you need a tryable, self-hostable, single-binary memory layer that doesn't require Python, Postgres, Qdrant, or Neo4j, Engram is built for you.

## Why Engram?

| Problem | Engram's answer |
|---------|-----------------|
| Every agent memory library is Python-first | **Rust core** with native Python, Java, and MCP clients — no sidecar required |
| Needs Postgres + Qdrant + Neo4j just to try | **Single SQLite file**, zero infra |
| Conversation history is not knowledge memory | **Fact extraction pipeline** — pulls structured facts out of messages |
| Old facts drift and contradict each other | **Conflict detection + consolidation** — decay, promote, dedup, summarize, reflect |
| Memory recall is either semantic OR keyword | **Hybrid retrieval** — vector search + SQLite FTS5 in one query |
| Agents lose memory across processes | **Durable by default** — one SQLite file, crash-safe, portable |
| MCP support is an afterthought | **MCP-native** — 7 tools exposed by a single binary |
| No time-travel over what the agent knew | **Temporal knowledge graph** — every fact is scoped and timestamped |
| Can't isolate memory per user or tenant | **First-class scopes** — org / user / session built into every query |

---

## Quickstart — 30 seconds

Engram speaks to four LLM providers out of the box: **Ollama** (default — free, local), **OpenAI**, **Anthropic Claude**, and **Google Gemini**. Pick one.

### Option A — Ollama (free, local, zero API keys)

**Requirements:** [Ollama](https://ollama.com/) with `llama3.2` and `nomic-embed-text` pulled:

```bash
ollama pull llama3.2
ollama pull nomic-embed-text
```

**Docker (easiest):**

```bash
docker run --rm -i \
  -v engram-data:/data \
  ghcr.io/jamjet-labs/engram-server:0.3.2
```

The container defaults to Ollama at `host.docker.internal:11434` — works out of the box on Docker Desktop for Mac and Windows. **Linux users** need `--add-host=host.docker.internal:host-gateway`.

### Option B — Anthropic Claude (hosted, highest quality)

```bash
docker run --rm -i \
  -e ENGRAM_LLM_PROVIDER=anthropic \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  -v engram-data:/data \
  ghcr.io/jamjet-labs/engram-server:0.3.2
```

Defaults to `claude-haiku-4-5-20251001`. Override with `-e ENGRAM_ANTHROPIC_MODEL=claude-sonnet-4-6`.

> Note: Anthropic has no native JSON mode, so Engram parses JSON from text responses. The `llm_util::extract_json_payload` helper strips markdown fences that Claude occasionally emits.

### Option C — Google Gemini

```bash
docker run --rm -i \
  -e ENGRAM_LLM_PROVIDER=google \
  -e GOOGLE_API_KEY=AIza... \
  -v engram-data:/data \
  ghcr.io/jamjet-labs/engram-server:0.3.2
```

Defaults to `gemini-flash-latest`. Override with `-e ENGRAM_GOOGLE_MODEL=gemini-2.5-flash`.

### Option D — Any OpenAI-compatible endpoint

```bash
docker run --rm -i \
  -e ENGRAM_LLM_PROVIDER=openai-compatible \
  -e OPENAI_API_KEY=sk-... \
  -v engram-data:/data \
  ghcr.io/jamjet-labs/engram-server:0.3.2
```

Defaults to OpenAI itself (`https://api.openai.com/v1`, `gpt-4o-mini`). **Change one env var to point at any of these providers without recompiling:**

| Provider | `ENGRAM_OPENAI_BASE_URL=` | Note |
|---|---|---|
| OpenAI | `https://api.openai.com/v1` | default |
| Azure OpenAI | `https://<resource>.openai.azure.com/openai/deployments/<deployment>` | |
| Groq | `https://api.groq.com/openai/v1` | very fast inference |
| Together.ai | `https://api.together.xyz/v1` | |
| Mistral | `https://api.mistral.ai/v1` | |
| DeepSeek | `https://api.deepseek.com/v1` | |
| Perplexity | `https://api.perplexity.ai` | |
| OpenRouter | `https://openrouter.ai/api/v1` | one key, many models |
| Fireworks | `https://api.fireworks.ai/inference/v1` | |
| vLLM (self-hosted) | `http://your-host:8000/v1` | |
| LM Studio | `http://localhost:1234/v1` | local desktop app |
| LocalAI | `http://localhost:8080/v1` | self-hosted |
| Ollama `/v1` compat layer | `http://localhost:11434/v1` | alternate path for Ollama |
| Your corporate LLM gateway | whatever URL your infra team exposes | |

`openai` is accepted as a backwards-compatible alias for `openai-compatible`.

### Option E — `command` (shell out to anything)

For providers Engram does not ship natively — an internal model behind a custom RPC, a raw SOAP endpoint, a local inference binary, a quick wrapper over a Python SDK, or tomorrow's new provider — use the `command` backend. Engram will spawn your script per extraction call, pipe a JSON request to its stdin, and read a JSON response from its stdout.

**The contract.** Your script reads one JSON object from stdin:

```json
{"system": "<extraction prompt>", "user": "<conversation text>", "structured": true}
```

Then writes one JSON value to stdout. Either a bare value:

```json
{"facts": [{"text": "...", "entities": [...], "confidence": 0.95, "category": "..."}]}
```

Or an envelope (useful for surfacing errors):

```json
{"content": {"facts": [...]}}
{"error": "rate limited, try again"}
```

Exit 0 on success, non-zero (with stderr) on failure.

**Example Python wrapper (~15 lines):**

```python
#!/usr/bin/env python3
# my-llm.py — wraps any Python SDK for Engram.
import json, sys
from my_llm_sdk import chat  # your SDK

req = json.loads(sys.stdin.read())
resp = chat(
    system=req["system"],
    user=req["user"],
    json_mode=req.get("structured", False),
)
sys.stdout.write(json.dumps(resp))
```

**Point Engram at it:**

```bash
docker run --rm -i \
  -e ENGRAM_LLM_PROVIDER=command \
  -e ENGRAM_LLM_COMMAND="python /scripts/my-llm.py" \
  -v $(pwd)/scripts:/scripts \
  -v engram-data:/data \
  ghcr.io/jamjet-labs/engram-server:0.3.2
```

Timeout is 120 seconds by default — override with `ENGRAM_LLM_COMMAND_TIMEOUT=30`.

> **Security:** the `command` provider runs **arbitrary commands** as the Engram process user. Never use it in a multi-tenant deployment where untrusted users can set `ENGRAM_LLM_COMMAND`. It is a local and single-tenant feature.

### Option F — cargo install (all providers same binary)

```bash
cargo install jamjet-engram-server
engram serve                                            # ollama (default)
ENGRAM_LLM_PROVIDER=anthropic ANTHROPIC_API_KEY=... engram serve
ENGRAM_LLM_PROVIDER=command ENGRAM_LLM_COMMAND="python /path/to/wrapper.py" engram serve
```

### Option G — REST mode for testing

```bash
engram serve --mode rest --port 9090
```

```bash
curl -X POST http://localhost:9090/v1/memory \
  -H 'content-type: application/json' \
  -d '{
    "user_id": "alice",
    "messages": [
      {"role": "user", "content": "I am allergic to peanuts and I love sourdough."}
    ]
  }'

curl 'http://localhost:9090/v1/memory/recall?q=food%20allergies&user_id=alice'
```

No server. No config. No Python sidecar. One binary.

---

## MCP client configuration

### Claude Desktop (`~/Library/Application Support/Claude/claude_desktop_config.json`)

```json
{
  "mcpServers": {
    "engram": {
      "command": "docker",
      "args": [
        "run", "--rm", "-i",
        "-v", "engram-data:/data",
        "ghcr.io/jamjet-labs/engram-server:0.3.2"
      ]
    }
  }
}
```

### Cursor / any MCP-aware IDE

Point it at the same `docker run` command, or at a locally-installed `engram serve` binary. After restart, seven `memory_*` tools are available to the model.

---

## The seven MCP tools

| Tool | What it does |
|------|--------------|
| `memory_add` | Extract and store facts from conversation messages (calls the LLM for extraction) |
| `memory_recall` | Semantic search over stored facts, scoped by `user_id` / `org_id` |
| `memory_context` | Assemble a token-budgeted context block for an LLM prompt, with tier-aware selection |
| `memory_search` | Keyword search over facts (SQLite FTS5) |
| `memory_forget` | Soft-delete a fact by ID with an optional reason |
| `memory_stats` | Aggregate counts: total facts, valid facts, entities, relationships |
| `memory_consolidate` | Run a consolidation cycle — decay stale facts, promote high-value ones, dedup near-duplicates |

All scoped by `(org_id, user_id, session_id)` — org is the coarsest, session the finest.

---

## REST API

Nine endpoints, rooted at `/v1/memory`. Full OpenAPI-style surface:

| Method | Path | Handler |
|--------|------|---------|
| GET | `/health` | Liveness probe |
| POST | `/v1/memory` | Add messages (fact extraction) |
| GET | `/v1/memory/recall?q=…&user_id=…` | Semantic recall |
| POST | `/v1/memory/context` | Token-budgeted context assembly |
| GET | `/v1/memory/search?q=…&user_id=…` | Keyword search |
| GET | `/v1/memory/stats` | Aggregate statistics |
| POST | `/v1/memory/consolidate` | Trigger consolidation |
| DELETE | `/v1/memory/facts/:id` | Forget a fact |
| DELETE | `/v1/memory/users/:id` | GDPR user-data delete |

---

## Configuration

All settings flow through CLI flags or environment variables. Env vars are the recommended way to configure the Docker image.

### Core

| CLI flag | Env var | Default | Notes |
|----------|---------|---------|-------|
| `--db` | `ENGRAM_DB_PATH` | `engram.db` | SQLite file path |
| `--mode` | `ENGRAM_MODE` | `mcp` | `mcp` (stdio) or `rest` (HTTP) |
| `--port` | `ENGRAM_PORT` | `9090` | HTTP port in REST mode |
| `--llm-provider` | `ENGRAM_LLM_PROVIDER` | `ollama` | `ollama`, `openai-compatible` (alias `openai`), `anthropic`, `google`, `command`, `mock` |
| `--embedding-provider` | `ENGRAM_EMBEDDING_PROVIDER` | `ollama` | `ollama` or `mock` (cloud embedding providers are planned) |
| `--embedding-model` | `ENGRAM_EMBEDDING_MODEL` | `nomic-embed-text` | Ollama embedding model |
| `--embedding-dims` | `ENGRAM_EMBEDDING_DIMS` | `768` | Must match the embedding model's output |

### Ollama

| CLI flag | Env var | Default |
|----------|---------|---------|
| `--ollama-url` | `ENGRAM_OLLAMA_URL` | `http://localhost:11434` (Docker image: `http://host.docker.internal:11434`) |
| `--ollama-llm-model` | `ENGRAM_OLLAMA_LLM_MODEL` | `llama3.2` |

### OpenAI-compatible (OpenAI, Groq, Together, Mistral, DeepSeek, Azure, vLLM, …)

| CLI flag | Env var | Default |
|----------|---------|---------|
| `--openai-api-key` | `OPENAI_API_KEY` | *(required when provider is `openai-compatible`)* |
| `--openai-base-url` | `ENGRAM_OPENAI_BASE_URL` | `https://api.openai.com/v1` — **change this to switch provider**, see the big table above |
| `--openai-model` | `ENGRAM_OPENAI_MODEL` | `gpt-4o-mini` |

### Anthropic

| CLI flag | Env var | Default |
|----------|---------|---------|
| `--anthropic-api-key` | `ANTHROPIC_API_KEY` | *(required when provider is `anthropic`)* |
| `--anthropic-base-url` | `ENGRAM_ANTHROPIC_BASE_URL` | `https://api.anthropic.com` |
| `--anthropic-model` | `ENGRAM_ANTHROPIC_MODEL` | `claude-haiku-4-5-20251001` |

### Google Gemini

| CLI flag | Env var | Default |
|----------|---------|---------|
| `--google-api-key` | `GOOGLE_API_KEY` | *(required when provider is `google`)* |
| `--google-base-url` | `ENGRAM_GOOGLE_BASE_URL` | `https://generativelanguage.googleapis.com/v1beta` |
| `--google-model` | `ENGRAM_GOOGLE_MODEL` | `gemini-flash-latest` |

### Command (shell-out)

| CLI flag | Env var | Default |
|----------|---------|---------|
| `--llm-command` | `ENGRAM_LLM_COMMAND` | *(required when provider is `command`)* — run via `sh -c` |
| `--llm-command-timeout` | `ENGRAM_LLM_COMMAND_TIMEOUT` | `120` — seconds before the child is killed |

> The `mock` backend returns empty facts and deterministic byte-cycled vectors. It exists for tests and CI. The server prints a clear warning on startup if it detects mock mode in use. API keys and command contents are never echoed to stdout or logs — the server only describes `provider model at base_url` or `command \`<first 60 chars>\``.

---

## Embedding the library

If you don't want a separate process, depend on `jamjet-engram` directly:

```toml
[dependencies]
jamjet-engram = "0.3.2"
```

```rust
use engram::{Memory, OllamaEmbeddingProvider, OllamaLlmClient, Scope, ExtractionConfig, Message};

let embedding = Box::new(OllamaEmbeddingProvider::new());
let memory = Memory::open("sqlite:engram.db?mode=rwc", embedding).await?;

let scope = Scope::user("default", "alice");
let messages = vec![Message {
    role: "user".into(),
    content: "I am allergic to peanuts.".into(),
}];

let llm = Box::new(OllamaLlmClient::new());
let fact_ids = memory
    .add_messages(&messages, scope.clone(), llm, ExtractionConfig::default())
    .await?;

// Later, recall
let facts = memory.recall(&engram::memory::RecallQuery {
    query: "food allergies".into(),
    scope: Some(scope),
    max_results: 5,
    as_of: None,
    min_score: None,
}).await?;
```

---

## Other languages

Engram is part of the JamJet ecosystem. These clients speak to `engram-server` over REST:

| Language | Package | Install |
|----------|---------|---------|
| Python | `jamjet` (includes `EngramClient`) | `pip install jamjet` |
| Java | `dev.jamjet:jamjet-sdk` (includes `EngramClient`) | Maven Central |
| Spring Boot | `dev.jamjet:engram-spring-boot-starter` | Maven Central — zero-config `@Bean EngramMemory` |
| Rust | `jamjet-engram` (embed directly) | `cargo add jamjet-engram` |

See [java-ai-memory.dev](https://java-ai-memory.dev) for how Engram compares to LangChain4j ChatMemory, Spring AI ChatMemory, Koog, Embabel, Google ADK Memory Bank, the Mem0 community wrapper, and Zep — with honest notes on where Engram fits and where the alternatives are more mature.

---

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│           Clients: Claude Desktop, Cursor, JamJet,        │
│           Python SDK, Java SDK, Spring Boot, cURL         │
├─────────────────┬────────────────────────────────────────┤
│   MCP (stdio)   │           REST (HTTP + JSON)            │
├─────────────────┴────────────────────────────────────────┤
│                     engram-server                         │
│                 (jamjet-engram-server)                    │
├──────────────────────────────────────────────────────────┤
│                       engram (lib)                        │
│   Extraction pipeline  │  Hybrid retrieval  │  Scopes     │
│   Conflict detection   │  Consolidation     │  Context    │
├──────────────────────────────────────────────────────────┤
│    LlmClient trait        │    EmbeddingProvider trait    │
│    Ollama · Mock          │    Ollama · Mock              │
├──────────────────────────────────────────────────────────┤
│                       Storage                             │
│       SQLite (facts, entities, FTS5, vectors)            │
└──────────────────────────────────────────────────────────┘
```

---

## What Engram is **not**

- **Not a chat-history store.** If all you need is the last N messages of a conversation, use LangChain4j `ChatMemory`, Spring AI `ChatMemory`, or any framework's built-in window.
- **Not a state checkpointer.** If you need to snapshot agent execution state for resume and replay, that's what LangGraph, Koog persistence, or JamJet's own durable runtime does. Pair them with Engram — they solve different problems.
- **Not a managed service.** No hosted plane, no auth layer beyond scopes, no SLA. Bring your own process manager.
- **Not benchmarked yet.** LongMemEval and DMR numbers are on the roadmap. Until they exist, treat comparative claims with the skepticism they deserve.

---

## Roadmap

| Status | Item |
|--------|------|
| ✅ | Fact extraction pipeline + SQLite storage |
| ✅ | Hybrid retrieval (vector + FTS5 with prefix matching) |
| ✅ | Consolidation engine (decay, promote, dedup, summarize, reflect) |
| ✅ | MCP stdio server (7 tools) |
| ✅ | REST API (9 endpoints) |
| ✅ | Multi-provider LLM backends: Ollama, OpenAI-compatible (OpenAI/Azure/Groq/Together/Mistral/vLLM/LM Studio/…), Anthropic, Google, `command` shell-out |
| ✅ | Python, Java, Spring Boot clients |
| ✅ | Docker image, MCP Registry publish |
| 🔄 | Postgres backend (in parallel to SQLite) |
| 🔄 | Spring AI `ChatMemoryRepository` implementation |
| 📋 | LongMemEval + DMR benchmark scores |
| 📋 | Quarkus extension |
| 📋 | Cloud embedding providers (OpenAI `text-embedding-3`, Google `text-embedding-004`) |

---

## License

Apache 2.0 — see [LICENSE](../../LICENSE).

---

<div align="center">
  <sub>Part of <a href="https://jamjet.dev">JamJet</a> · Built by <a href="https://github.com/sunilp">Sunil Prakash</a> · © 2026 JamJet Labs</sub>
</div>
