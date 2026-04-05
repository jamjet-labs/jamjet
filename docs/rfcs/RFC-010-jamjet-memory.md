# Engram — Design Specification

**Date:** 2026-04-05
**Status:** Draft
**Author:** Sunil Prakash
**Product Name:** Engram (by JamJet)

## 1. Overview

Engram is a durable, open-source memory layer for AI agents. It works as a fully independent alternative to Mem0 (zero JamJet dependency) or as a runtime-integrated module that gains crash safety, audit trails, and compliance for free.

**Tagline:** *"Memory that survives crashes, understands time, and speaks MCP."*

### Goals

- **Standalone-first:** `pip install engram` works with zero infrastructure — local SQLite, embedded vector index, ONNX embeddings. No API keys required.
- **MCP-native:** Primary interface is MCP tools. Any MCP client (Claude Code, Cursor, Windsurf, JamJet agents) gets persistent memory by connecting to the server.
- **Beyond Mem0:** Temporal knowledge graph, three-tier memory hierarchy, context assembly, background consolidation, conflict resolution — things Mem0 doesn't offer.
- **Runtime superpowers:** When used inside JamJet, memory operations become durable workflow nodes with event sourcing, PII redaction, audit trails, and crash recovery.

### Non-Goals

- Replacing vector databases (we integrate with them, not compete)
- Building a managed cloud service (open-source only, self-hosted)
- Supporting real-time streaming memory updates (batch extraction is fine)

## 2. Competitive Analysis

| Capability | Mem0 | Zep | Letta | engram |
|---|---|---|---|---|
| Zero-config local mode | Needs OpenAI key | Cloud only | Needs API key | **Works offline (ONNX + SQLite)** |
| Extraction | LLM facts | Graph builder | Self-editing | **3-stage pipeline + conflict resolution** |
| Vector search | Yes | Yes | Yes | **Yes (embedded + pluggable)** |
| Knowledge graph | Optional Neo4j | Graphiti (temporal) | No | **Built-in temporal + pluggable** |
| Context assembly | No | Context blocks | MemGPT paging | **Token-budgeted, model-aware, multi-format** |
| Temporal queries | No | Yes | No | **Yes (bi-temporal, point-in-time)** |
| Memory tiers | Flat | Flat | 3-tier | **3-tier + 4-level scoping** |
| Self-editing (agent tools) | No | No | Yes | **Yes (MCP-native tools)** |
| Background consolidation | No | No | Sleep-time | **5-op consolidation engine** |
| Crash safety | No | Cloud SLA | No | **Event-sourced (runtime mode)** |
| PII redaction | No | No | No | **Built-in (runtime mode)** |
| Audit trail | Enterprise only | Enterprise only | No | **Built-in (runtime mode)** |
| MCP native | Bolt-on | No | Client only | **Primary interface** |
| Java SDK | No | No | No | **First-class + Spring starter** |
| Mem0 migration | — | No | No | **Built-in CLI tool** |
| Deployment | Lib or cloud | Cloud | Lib or cloud | **Lib + server + runtime-integrated** |
| License | Apache 2.0 | Cloud + OSS engine | Apache 2.0 | **Apache 2.0** |

### What JamJet has that they don't

| JamJet Strength | Mem0 | Zep | Letta |
|---|---|---|---|
| Durable workflow orchestration (event-sourced, crash-safe) | No | No | Partial |
| Multi-agent coordination (coordinator, A2A, agent-as-tool) | No | No | Subagents only |
| Human-in-the-loop approval with audit trail | No | No | No |
| MCP + A2A protocol native | No | No | MCP client only |
| PII redaction / retention policies | No | No | No |
| Pluggable execution strategies (debate, consensus, reflection) | No | No | No |

## 3. Architecture

### Deployment Modes

Two modes from the same Rust codebase:

**Mode A — Standalone:** Own binary/process. Own SQLite, REST API, MCP server. Zero JamJet dependencies. Installed via `cargo install`, `pip install`, or `brew install`.

**Mode B — Runtime-Integrated:** Linked in-process with JamJet runtime. Memory operations become durable workflow nodes. Inherits event sourcing, PII redaction, audit trail, tenant isolation, cron scheduling.

### Component Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                      engram                               │
│                                                                   │
│  ┌─────────────┐  ┌─────────────┐  ┌──────────────────────────┐ │
│  │  Extraction  │  │  Temporal    │  │  Context                 │ │
│  │  Pipeline    │  │  Fact Graph  │  │  Assembler               │ │
│  │  (LLM-based) │  │  (entities,  │  │  (token budgets,         │ │
│  │              │  │   relations,  │  │   priority ranking,      │ │
│  │              │  │   validity)   │  │   model-aware assembly)  │ │
│  └──────┬───────┘  └──────┬───────┘  └───────────┬──────────────┘ │
│         │                 │                       │                │
│  ┌──────▼─────────────────▼───────────────────────▼──────────────┐│
│  │                    Memory Store (Pluggable)                    ││
│  │  ┌────────────┐  ┌────────────┐  ┌─────────────────────────┐ ││
│  │  │  Vector     │  │  Graph     │  │  Fact Store              │ ││
│  │  │  (embedded  │  │  (embedded │  │  (SQLite default,        │ ││
│  │  │   or Qdrant,│  │   or Neo4j,│  │   Postgres optional)     │ ││
│  │  │   Pinecone) │  │   FalkorDB)│  │                          │ ││
│  │  └────────────┘  └────────────┘  └─────────────────────────┘ ││
│  └───────────────────────────────────────────────────────────────┘│
│                                                                   │
│  ┌───────────────────────────────────────────────────────────────┐│
│  │                    Interface Layer                             ││
│  │  ┌──────────┐  ┌──────────┐  ┌────────────┐  ┌────────────┐ ││
│  │  │ MCP      │  │ REST     │  │ Python     │  │ Java       │ ││
│  │  │ Server   │  │ API      │  │ SDK        │  │ SDK        │ ││
│  │  │ (native) │  │ (Axum)   │  │ (client)   │  │ (client)   │ ││
│  │  └──────────┘  └──────────┘  └────────────┘  └────────────┘ ││
│  └───────────────────────────────────────────────────────────────┘│
│                                                                   │
│  ┌───────────────────────────────────────────────────────────────┐│
│  │              Consolidation Engine (background)                ││
│  │  summarize · deduplicate · promote · reflect · decay          ││
│  └───────────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────────┘
```

### Dependency Rules

- `engram` crate has **zero dependency** on `jamjet-state`, `jamjet-ir`, or any runtime crate. Fully standalone.
- `engram` depends on `jamjet-models` (for LLM extraction via `ModelAdapter` — already a standalone crate with Anthropic, OpenAI, Google, Ollama adapters).
- `engram-nodes` is a thin bridge crate that wires engram into the runtime. Depends on `engram` + `jamjet-state` + `jamjet-audit`.

## 4. Memory Model

### Scoping (who the memory belongs to)

Four-level hierarchy:

```
org ("acme-corp")                          ← shared knowledge: policies, product catalog
 ├── agent ("support-bot")                 ← agent expertise: learned tool patterns, domain facts
 │    ├── user ("user_123")                ← personal: preferences, history, profile facts
 │    │    ├── session ("sess_abc")         ← ephemeral: current conversation context
 │    │    └── session ("sess_def")
 │    └── user ("user_456")
 └── agent ("sales-bot")
      └── user ("user_123")                ← same user, different agent = different memory
```

Every memory entry is tagged with `(org_id, agent_id?, user_id?, session_id?)`. Nullable fields mean broader scope. A fact with only `org_id` is org-wide. A fact with all four is session-scoped.

**Cross-scope search** with configurable priority:

```python
results = memory.recall(
    query="what does this user prefer?",
    user_id="user_123",
    scope_priority=["session", "user", "agent", "org"],
    max_results=20,
)
```

In standalone mode: `org_id` defaults to `"default"`, works like Mem0's flat model.
In runtime mode: `org_id` maps to JamJet `tenant_id` automatically.

### Tiers (how deep the memory lives)

| Tier | What Lives Here | Lifetime | Storage |
|---|---|---|---|
| **Working** | Key facts the agent needs every turn. Small block (~500 tokens). Agent reads/writes it actively. | Per-session, promotable | In-memory + fact store |
| **Conversation** | Full message history + extracted facts from current and past sessions. Searchable. | Cross-session per user | Fact store + vector index |
| **Knowledge** | Consolidated long-term facts, entity graph, learned patterns. The "brain." | Permanent (with decay) | Graph store + vector index + fact store |

**Promotion flow:**

```
Session ends → working memory facts auto-promote to conversation tier
Consolidation runs → frequent/important conversation facts promote to knowledge tier
Decay runs → stale knowledge facts get confidence score reduced, eventually archived
```

### Fact Structure

```
┌─────────────────────────────────────────────────┐
│ Fact                                             │
│                                                   │
│ id:             fact_8f3a...                      │
│ text:           "User prefers dark mode"          │
│ scope:          (acme, support-bot, user_123, -)  │
│ tier:           knowledge                         │
│ category:       "preferences"                     │
│ source:         session_abc / message_42           │
│ confidence:     0.95                              │
│ valid_from:     2026-04-01T10:00:00Z              │
│ invalid_at:     null  (still valid)               │
│ created_at:     2026-04-01T10:00:05Z (ingestion)  │
│ embedding:      [0.12, -0.34, ...]               │
│ entity_refs:    [entity("user_123"), entity("dark_mode")] │
│ supersedes:     null                              │
│ superseded_by:  null                              │
│ access_count:   0                                 │
│ last_accessed:  null                              │
│ metadata:       {}                                │
└─────────────────────────────────────────────────┘
```

### Temporal Fact Lifecycle

Every fact has bi-temporal tracking:

- `valid_from` — when the fact became true in the real world
- `invalid_at` — when the fact stopped being true (null = still valid)
- `created_at` — when the fact was ingested into the system

**On contradiction:**

```
New message: "Actually I switched to light mode"
→ Extraction finds: "User prefers light mode"
→ Conflict detection matches existing: "User prefers dark mode"
→ Old fact: invalid_at = now, superseded_by = new_fact_id
→ New fact: supersedes = old_fact_id, valid_from = now
→ Neither fact is deleted — full history preserved
```

**Point-in-time query:**

```python
facts = memory.recall(
    query="user preferences",
    user_id="user_123",
    as_of="2026-04-01T10:00:00Z",  # returns facts valid at that moment
)
```

## 5. Extraction Pipeline

Three-stage pipeline that converts raw conversations into structured memory.

### Stage 1: Fact Extraction

LLM call via `ModelAdapter` (works with any configured provider):

```
Input:  [user: "I just moved to Austin from NYC. Can you help me find a vet for my golden retriever Max?"]

Output:
  facts:
    - text: "User relocated from New York City to Austin"
      entities: [user_123, "New York City", "Austin"]
      relationships: [("user_123", "lives_in", "Austin"), ("user_123", "previously_lived_in", "New York City")]
      confidence: 0.95

    - text: "User has a golden retriever named Max"
      entities: [user_123, "Max"]
      relationships: [("user_123", "owns_pet", "Max"), ("Max", "is_a", "golden retriever")]
      confidence: 0.97

    - text: "User is looking for a veterinarian in Austin"
      entities: [user_123, "Austin"]
      relationships: [("user_123", "seeking_service", "veterinarian")]
      confidence: 0.90
      category: "intent"
```

Uses structured output (JSON mode) with configurable extraction prompts per domain. Custom categories and rules supported.

### Stage 2: Dedup + Conflict Resolution

Before storing, every extracted fact is checked against existing memory:

1. Vector similarity search against existing facts (same scope)
2. If similarity > threshold (default 0.92):
   - Same meaning → skip (dedup)
   - Contradicts → invalidate old fact, store new with `supersedes` link
   - Refines/adds detail → store as new fact, link to related
3. If no match → store as new fact
4. Entity merge: if "NYC" and "New York City" refer to same entity → merge

The conflict resolution LLM call is lightweight — only triggered when vector similarity is high but not exact. Most facts pass through without a second LLM call.

### Stage 3: Graph Update

New facts and entities are wired into the temporal knowledge graph:

```
Entities created/updated:
  [user_123] ──lives_in──→ [Austin]          (valid_from: now)
  [user_123] ──prev_lived──→ [NYC]           (valid_from: now)
  [user_123] ──owns_pet──→ [Max]             (valid_from: now)
  [Max] ──is_a──→ [golden retriever]         (valid_from: now)

If user later says "I moved to Denver":
  [user_123] ──lives_in──→ [Austin]          (invalid_at: now)
  [user_123] ──lives_in──→ [Denver]          (valid_from: now)
  [user_123] ──prev_lived──→ [Austin]        (valid_from: now)  ← auto-generated
```

### Configuration

```python
from engram import MemoryConfig, ExtractionRule

config = MemoryConfig(
    extraction_rules=[
        ExtractionRule(category="preferences", priority=1.0),
        ExtractionRule(category="intent", priority=0.5, ttl="24h"),
        ExtractionRule(category="personal_info", priority=0.8, pii=True),
    ],
    custom_prompt="Also extract: dietary restrictions, timezone, preferred communication style",
    skip_categories=["small_talk"],
)
```

### Performance

- **Async by default** — extraction runs in background after `memory.add()` returns
- **Batching** — multiple messages extracted in one LLM call when possible
- **Caching** — embedding cache avoids re-embedding identical text
- **Cost control** — configurable model per stage (use Haiku/nano for extraction, bigger model only for conflict resolution)

## 6. Retrieval & Context Assembly

### Hybrid Retrieval

Every `recall()` query runs three retrieval strategies in parallel:

```
Query: "What does this user need help with?"
                    │
        ┌───────────┼───────────┐
        ▼           ▼           ▼
   Vector Search  Graph Walk  Keyword/BM25
   (semantic)     (relational) (exact match)
        │           │           │
        └───────────┼───────────┘
                    ▼
              Reranker (LLM or cross-encoder)
                    │
                    ▼
             Scored, deduplicated results
```

- **Vector search** — cosine similarity on fact embeddings. Finds semantically related facts.
- **Graph walk** — starting from entities in the query, traverse relationships 1-2 hops. Finds structurally connected facts that vector search misses.
- **Keyword/BM25** — exact term matching. Catches proper nouns, IDs, specific values that embeddings can blur.

Results are merged with configurable weights (default: vector 0.5, graph 0.3, keyword 0.2), deduplicated, and optionally reranked.

### Context Assembler

Takes retrieved facts and builds a token-budgeted context block:

```python
context = memory.context(
    query="help user find a vet",
    user_id="user_123",
    session_id="sess_abc",
    model="claude-sonnet-4-6",
    token_budget=2000,
    format="system_prompt",
)

# Returns:
# ContextBlock(
#   text="<memory>\n<user_profile>\n...\n</memory>",
#   token_count=847,
#   facts_included=12,
#   facts_omitted=3,
#   scope_breakdown={"user": 8, "agent": 3, "org": 1},
# )
```

**Priority ranking within the budget:**

1. Working memory (always included, highest priority)
2. Session-scoped facts (current conversation context)
3. User-scoped facts (personal preferences, history — ranked by recency × relevance)
4. Agent-scoped facts (domain knowledge — ranked by relevance)
5. Org-scoped facts (shared knowledge — ranked by relevance)

If the budget is tight, lower-priority facts are summarized or dropped. The assembler never exceeds the token budget.

**Output formats:**

| Format | Use Case |
|---|---|
| `system_prompt` | XML-tagged block for system prompt injection |
| `messages` | List of ChatMessage objects for conversation history injection |
| `markdown` | Human-readable summary |
| `raw` | Structured JSON — do your own formatting |

## 7. MCP-Native Interface + REST API

### MCP Tools (primary interface)

When `engram serve` runs, it exposes an MCP server. Any MCP client connects and gets these tools:

| Tool | Purpose |
|---|---|
| `memory_add` | Ingest conversation messages, extract and store facts |
| `memory_recall` | Semantic search over memory with scope/temporal filters |
| `memory_forget` | Soft-delete facts with audit trail and reason |
| `memory_reflect` | Trigger background consolidation on a topic |
| `memory_context` | Assemble token-budgeted context block for LLM injection |
| `memory_inspect` | Inspect facts, entities, and graph for a scope |

**MCP tool parameters:**

```
memory_add:
  messages: [{role, content}]
  user_id?: string
  agent_id?: string
  session_id?: string
  metadata?: object

memory_recall:
  query: string
  user_id?: string
  agent_id?: string
  scope_priority?: string[]
  as_of?: datetime
  max_results?: int
  include_graph?: bool

memory_forget:
  fact_id?: string
  query?: string
  user_id?: string
  reason: string

memory_reflect:
  user_id?: string
  topic?: string
  depth?: "light" | "deep"

memory_context:
  query: string
  user_id?: string
  token_budget?: int
  model?: string
  format?: string

memory_inspect:
  user_id?: string
  entity?: string
  include_invalidated?: bool
```

**Zero-code integration example:**

```json
{
  "mcpServers": {
    "memory": {
      "command": "engram",
      "args": ["serve", "--mcp"]
    }
  }
}
```

### REST API

Same operations for non-MCP clients:

```
POST   /v1/memory                     # add (extract from conversation)
GET    /v1/memory/recall?q=...        # recall (search)
DELETE /v1/memory/facts/{id}          # forget
POST   /v1/memory/reflect             # trigger consolidation
POST   /v1/memory/context             # assemble context block
GET    /v1/memory/inspect             # inspect facts/graph
GET    /v1/memory/facts               # list facts (with filters)
GET    /v1/memory/entities            # list entities
GET    /v1/memory/entities/{id}/graph # entity neighborhood
GET    /v1/memory/stats               # usage stats, fact counts, storage
POST   /v1/memory/export              # export all memory for a scope
POST   /v1/memory/import              # import (migration from Mem0, etc.)
DELETE /v1/memory/users/{id}          # GDPR: delete all user data
```

Auth via API key header (`Authorization: Bearer jm_...`).

### Mem0 Migration

```bash
engram migrate --from mem0 --mem0-api-key=... --org=default
```

Pulls all memories from Mem0 API, converts to engram fact format, re-indexes in vector + graph stores.

## 8. Consolidation Engine

Background process that keeps memory healthy. Runs on configurable interval — standalone uses a built-in timer loop, runtime mode uses JamJet's cron scheduler.

### Five Operations

**1. Summarize** — When conversation memory exceeds a configurable count (default: 100 facts per user):
- Groups conversation facts by topic (using embeddings)
- LLM summarizes each group into 1-3 knowledge-tier facts
- Original conversation facts marked `summarized: true` (excluded from default recall, still available for deep search)

**2. Deduplicate** — Batch vector similarity scan across all facts in a scope:
- Pairs with similarity > 0.95 flagged
- LLM confirms: same meaning? → merge (keep higher-confidence, link the other as superseded)
- No LLM call if similarity > 0.99 — auto-merge

**3. Promote** — Facts accessed more than N times (default: 3) in the conversation tier get promoted to knowledge tier:
- Confidence boosted by access frequency
- Embedding re-indexed in knowledge store

**4. Reflect** — LLM reads a user's top facts and generates higher-order insights:

```
Input facts:
  - "User orders salads for lunch"
  - "User asked about calorie counts"
  - "User mentioned gym membership"
  - "User prefers sugar-free drinks"

Reflected insight:
  - "User is health-conscious and actively managing diet/fitness"
    (confidence: 0.85, source: "reflection", evidence_refs: [fact_1, ..., fact_4])
```

Reflections are stored as knowledge-tier facts with full provenance linking back to source facts.

**5. Decay** — Time-based confidence reduction:

```
confidence_new = confidence × decay_factor ^ (days_since_last_access / half_life)

Defaults: decay_factor=0.95, half_life=30 days
Below 0.3 confidence → archived
```

### Configuration

```python
from engram import ConsolidationConfig

config = ConsolidationConfig(
    interval="6h",
    summarize_threshold=100,
    dedup_similarity=0.95,
    promote_access_count=3,
    reflect_min_facts=10,
    decay_half_life_days=30,
    archive_threshold=0.3,
    max_llm_calls_per_cycle=10,
    enabled_ops=["summarize", "dedup", "promote", "reflect", "decay"],
)
```

## 9. Storage Layer

### Traits

Three pluggable storage traits:

```rust
trait FactStore: Send + Sync {
    async fn insert_fact(&self, fact: Fact) -> Result<FactId>;
    async fn get_fact(&self, id: FactId) -> Result<Option<Fact>>;
    async fn update_fact(&self, id: FactId, patch: FactPatch) -> Result<()>;
    async fn list_facts(&self, filter: FactFilter) -> Result<Vec<Fact>>;
    async fn invalidate_fact(&self, id: FactId, superseded_by: FactId, reason: &str) -> Result<()>;
    async fn delete_user_data(&self, scope: Scope) -> Result<u64>;  // GDPR
    async fn export(&self, scope: Scope) -> Result<Vec<Fact>>;
    async fn import(&self, facts: Vec<Fact>) -> Result<u64>;
    async fn stats(&self, scope: Scope) -> Result<StoreStats>;
}

trait VectorStore: Send + Sync {
    async fn upsert(&self, id: FactId, embedding: Vec<f32>, metadata: Value) -> Result<()>;
    async fn search(&self, query: Vec<f32>, filter: VectorFilter, top_k: usize) -> Result<Vec<VectorMatch>>;
    async fn delete(&self, id: FactId) -> Result<()>;
    async fn delete_by_scope(&self, scope: Scope) -> Result<u64>;
}

trait GraphStore: Send + Sync {
    async fn upsert_entity(&self, entity: Entity) -> Result<EntityId>;
    async fn upsert_relationship(&self, rel: Relationship) -> Result<RelationshipId>;
    async fn invalidate_relationship(&self, id: RelationshipId, invalid_at: DateTime) -> Result<()>;
    async fn get_entity(&self, id: EntityId) -> Result<Option<Entity>>;
    async fn neighbors(&self, id: EntityId, depth: u8, as_of: Option<DateTime>) -> Result<SubGraph>;
    async fn search_entities(&self, query: &str, top_k: usize) -> Result<Vec<Entity>>;
    async fn delete_by_scope(&self, scope: Scope) -> Result<u64>;
}
```

### Built-in Implementations (zero-infra)

| Trait | Embedded Default | How |
|---|---|---|
| `FactStore` | SQLite | SQLx, WAL mode, ACID. Single file. |
| `VectorStore` | Embedded HNSW | In-process vector index (`usearch` or `hnsw_rs`). Persisted to disk. Handles 100K+ facts. |
| `GraphStore` | SQLite triple store | Entities + relationships tables with temporal columns. BFS/DFS traversal in Rust. |

All three share one SQLite file by default: `~/.engram/memory.db`

### Pluggable Backends (production scale)

| Trait | Adapters | When to use |
|---|---|---|
| `FactStore` | Postgres | Multi-instance server, >1M facts |
| `VectorStore` | Qdrant, Pinecone, Weaviate | >500K facts, distributed search |
| `GraphStore` | Neo4j, FalkorDB, Memgraph | Complex graph queries, >100K entities |

### Embedding Providers

```rust
trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn dimensions(&self) -> usize;
}
```

| Implementation | Notes |
|---|---|
| ONNX local | Ship a small embedded model for true zero-config. Default fallback. |
| Ollama | Zero-cost, offline. `nomic-embed-text` or `mxbai-embed-large`. |
| OpenAI | `text-embedding-3-small`. 1536 dims. |
| Anthropic (Voyage) | If Anthropic key configured. |

**Priority resolution:** ONNX local → Ollama (if running) → configured cloud provider. Always works out of the box.

### Configuration

```toml
# ~/.engram/config.toml

[storage]
path = "~/.engram/memory.db"

[storage.vector]
backend = "embedded"                  # or "qdrant", "pinecone"

[storage.graph]
backend = "embedded"                  # or "neo4j", "falkordb"

[embedding]
provider = "auto"                     # auto-detect: ONNX → Ollama → cloud

[extraction]
provider = "auto"                     # same ModelAdapter resolution
```

## 10. Runtime Integration Mode

When engram is used inside JamJet runtime, memory operations become durable workflow nodes.

### Memory Workflow Nodes

Three new node types in `WorkflowIr`:

```yaml
nodes:
  extract_memory:
    kind: memory_extract
    config:
      source: "{{state.messages}}"
      user_id: "{{state.user_id}}"

  recall_context:
    kind: memory_recall
    config:
      query: "{{state.current_question}}"
      user_id: "{{state.user_id}}"
      token_budget: 2000
      format: system_prompt
      output_field: "memory_context"

  answer:
    kind: model
    config:
      model: claude-sonnet
      system: "{{state.memory_context}}"
      messages: "{{state.messages}}"

edges:
  - from: extract_memory
    to: recall_context
  - from: recall_context
    to: answer
```

### Runtime Superpowers (free when integrated)

| Capability | How |
|---|---|
| Crash-safe extraction | Memory extract is a durable node — replays from last event on crash |
| PII redaction on facts | JamJet's `DataPolicyIr` applies automatically before storage |
| Audit trail | Every memory op becomes an `AuditLogEntry` with actor_id, timestamp, policy_decision |
| Retention policies | Facts inherit workflow's `retain_outputs` / `expires_at` settings |
| Tenant isolation | `org_id` = `tenant_id`. No cross-tenant data leaks. |
| Human-in-the-loop on forget | Wire `memory_forget` through an approval node |
| Memory in coordinator graphs | Multiple agents share/scope memory per agent |
| Replay & fork | Fork execution → memory state forks with it. A/B test memory strategies. |
| Cost tracking | Extraction/reflection LLM calls tracked as node costs in `jamjet inspect` |
| Provenance | Extracted facts carry `ProvenanceMetadata` — model, confidence, source |

### Auto-Injection Mode

For workflows that want memory without explicit nodes:

```yaml
workflow_id: support-bot
memory:
  enabled: true
  auto_inject: true
  token_budget: 2000
  user_id_field: "state.user_id"
  extract_on: "model_complete"
```

Transparently wraps every model node: recall before, extract after. Zero workflow changes.

### Consolidation via Cron

```yaml
workflow_id: memory-consolidation
schedule: "0 */6 * * *"
nodes:
  consolidate:
    kind: memory_consolidate
    config:
      ops: [summarize, dedup, promote, reflect, decay]
      max_llm_calls: 10
```

## 11. Module Structure

### Rust Crates

```
runtime/
├── memory/                          # engram (standalone)
│   ├── Cargo.toml
│   ├── src/
│   │   ├── lib.rs                   # Public API: Memory, MemoryConfig
│   │   ├── fact.rs                  # Fact, Entity, Relationship, Scope
│   │   ├── store.rs                 # FactStore trait
│   │   ├── store_sqlite.rs          # SQLite FactStore impl
│   │   ├── vector.rs                # VectorStore trait
│   │   ├── vector_embedded.rs       # HNSW in-process impl
│   │   ├── vector_qdrant.rs         # Qdrant adapter (feature-gated)
│   │   ├── graph.rs                 # GraphStore trait
│   │   ├── graph_embedded.rs        # SQLite triple store impl
│   │   ├── graph_neo4j.rs           # Neo4j adapter (feature-gated)
│   │   ├── embedding.rs             # EmbeddingProvider trait
│   │   ├── embedding_onnx.rs        # Local ONNX model
│   │   ├── embedding_ollama.rs      # Ollama adapter
│   │   ├── extract.rs               # Extraction pipeline (stages 1-3)
│   │   ├── retrieve.rs              # Hybrid retrieval (vector + graph + BM25)
│   │   ├── assemble.rs              # Context assembler (token budgeting)
│   │   ├── consolidate.rs           # Sleep-time engine (5 operations)
│   │   ├── conflict.rs              # Contradiction detection + resolution
│   │   ├── decay.rs                 # Confidence decay + archival
│   │   ├── migrate.rs               # Import from Mem0 / Zep / raw JSON
│   │   ├── mcp_server.rs            # MCP tool definitions + handler
│   │   ├── api.rs                   # REST API routes (Axum)
│   │   ├── config.rs                # MemoryConfig, ConsolidationConfig
│   │   └── server.rs                # Standalone binary entrypoint
│   ├── migrations/                  # SQLite DDL
│   └── tests/
│
├── memory-nodes/                    # Runtime integration (thin glue)
│   ├── Cargo.toml                   # depends on engram + jamjet-state + jamjet-audit
│   └── src/
│       ├── lib.rs
│       ├── extract_node.rs          # MemoryExtractNode executor
│       ├── recall_node.rs           # MemoryRecallNode executor
│       ├── consolidate_node.rs      # MemoryConsolidateNode executor
│       └── auto_inject.rs           # Transparent injection for model nodes
```

### Feature Flags

```toml
[features]
default = ["embedded", "onnx"]
embedded = []                        # SQLite + HNSW + triple store
onnx = ["ort"]                       # Local embedding model
server = ["axum", "tower"]           # REST API binary
mcp = ["jamjet-protocols-mcp"]       # MCP server
qdrant = ["qdrant-client"]           # Qdrant vector adapter
pinecone = ["pinecone-sdk"]          # Pinecone vector adapter
neo4j = ["neo4rs"]                   # Neo4j graph adapter
falkordb = ["redis"]                 # FalkorDB graph adapter
postgres = ["sqlx/postgres"]         # Postgres fact store
```

### Python SDK

```
sdk/python/engram/            # Standalone Python package
├── __init__.py                      # Memory class (main API)
├── client.py                        # HTTP/MCP client to server
├── config.py                        # MemoryConfig, Pydantic models
├── types.py                         # Fact, Entity, Scope, ContextBlock
├── local.py                         # In-process mode (PyO3 or subprocess)
└── testing.py                       # MockMemory, assertion helpers

# Published as: pip install engram
```

### Java SDK

```
sdk/java/engram/              # Maven module
├── pom.xml
└── src/main/java/dev/engram/
    ├── Engram.java                  # Main API
    ├── EngramClient.java            # HTTP client
    ├── Fact.java
    ├── Scope.java
    ├── ContextBlock.java
    ├── EngramConfig.java
    ├── spring/
    │   └── EngramAutoConfiguration.java
    └── langchain4j/
        └── EngramMemoryStore.java

# Published as: dev.jamjet:engram on Maven Central
```

### Standalone Binary

```bash
# Install
cargo install engram
pip install engram
brew install jamjet-labs/tap/engram

# Run
engram serve                  # REST API + MCP server on :9090
engram serve --mcp-only       # MCP stdio mode (Claude Code, Cursor)
engram migrate --from mem0    # import from Mem0
engram inspect --user user_123
engram stats
engram export --user user_123 --format json
```

## 12. Developer Experience — Quick Start

### Python (standalone, zero infra)

```python
from engram import Memory

# One line. Uses local SQLite + ONNX embeddings. No API keys needed.
memory = Memory()

# Add a conversation
memory.add(
    messages=[
        {"role": "user", "content": "I'm allergic to peanuts and I live in Austin"},
        {"role": "assistant", "content": "Got it! I'll keep that in mind."},
    ],
    user_id="user_123",
)

# Recall
results = memory.recall("dietary restrictions", user_id="user_123")

# Assemble context for your next LLM call
context = memory.context(
    query="recommend a restaurant",
    user_id="user_123",
    token_budget=1000,
)

# Use with any LLM
response = anthropic.messages.create(
    model="claude-sonnet-4-6",
    system=context.text,
    messages=[{"role": "user", "content": "Recommend a restaurant for dinner"}],
)
```

### Java (standalone)

```java
var memory = Engram.create();

memory.add(
    Messages.of("I'm allergic to peanuts and I live in Austin"),
    Scope.user("user_123")
);

var context = memory.context("recommend a restaurant",
    Scope.user("user_123"),
    ContextOptions.builder().tokenBudget(1000).build()
);
```

### MCP (zero code)

```json
{
  "mcpServers": {
    "memory": {
      "command": "engram",
      "args": ["serve", "--mcp"]
    }
  }
}
```

### Spring Boot (auto-configured)

```xml
<dependency>
    <groupId>dev.jamjet</groupId>
    <artifactId>engram-spring-boot-starter</artifactId>
</dependency>
```

```yaml
jamjet:
  memory:
    enabled: true
```

## 13. Implementation Phases

| Phase | Deliverables | Depends On |
|---|---|---|
| **M1: Core + Storage** | Fact/Entity/Scope types, FactStore trait + SQLite impl, GraphStore trait + SQLite triple store impl, VectorStore trait + embedded HNSW, EmbeddingProvider trait + ONNX/Ollama impls, basic `Memory` API (add raw facts, recall by scope, list/delete) | Nothing |
| **M2: Extraction** | Extraction pipeline (stages 1-3), ModelAdapter integration for LLM calls, conflict detection + resolution, entity merge, configurable extraction rules | M1 |
| **M3: Graph + Temporal** | Temporal fact lifecycle (valid_from/invalid_at, supersedes), graph traversal (BFS/DFS with temporal filters), point-in-time queries, hybrid retrieval (vector + graph + BM25 merge) | M1 (parallel with M2) |
| **M4: Context Assembly** | Token counting per model, priority-ranked assembly, budget enforcement, output formats (system_prompt, messages, markdown, raw), `memory.context()` API | M1, M3 |
| **M5: Interfaces** | MCP server (6 tools), REST API (Axum), CLI commands (serve, inspect, stats, export, migrate), API key auth | M1-M4 |
| **M6: Consolidation** | 5-op background engine (summarize, dedup, promote, reflect, decay), configurable scheduling, cost controls | M2, M3 |
| **M7: SDKs** | Python SDK (`engram` package), Java SDK (`dev.jamjet:engram`), Spring Boot auto-configuration | M5 |
| **M8: Runtime Integration** | `engram-nodes` crate, MemoryExtract/Recall/Consolidate node executors, auto-injection mode, PII redaction integration, audit trail wiring | M1-M6 |
| **M9: Pluggable Backends** | Qdrant adapter, Neo4j adapter, Postgres adapter, Pinecone adapter (all feature-gated) | M1 |
| **M10: Migration + Polish** | Mem0 import tool, Zep import tool, benchmarks vs. Mem0, documentation, examples | M5 |
