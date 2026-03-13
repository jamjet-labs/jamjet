# JamJet vs Google ADK — Side-by-Side Comparison

A detailed comparison using the wealth management multi-agent use case.

---

## 1. Tool Definition

### JamJet
```python
from jamjet import tool

@tool
async def get_client_profile(client_id: str) -> dict[str, Any]:
    """Retrieve a client's full financial profile."""
    ...
```
- `@tool` decorator registers function in global registry
- Creates `ToolDefinition` object with JSON Schema auto-generated from type hints
- Supports `name=`, `permissions=` kwargs
- Can be exposed as MCP server via `serve_tools([get_client_profile])`
- Tool is an async function — enforced at decoration time

### Google ADK
```python
def get_client_profile(client_id: str) -> dict:
    """Retrieve a client's full financial profile.

    Args:
        client_id: The client identifier.
    """
    ...
```
- Plain Python function — no decorator needed
- Schema extracted from type hints + Google-style docstring
- No permissions model
- No MCP server support (tools are framework-internal)
- Sync functions — ADK handles async wrapping internally

### Verdict
| Aspect | JamJet | Google ADK |
|--------|--------|------------|
| Boilerplate | Minimal (`@tool` + type hints) | None (plain functions) |
| Type safety | Strong (Pydantic models, JSON Schema) | Moderate (docstring-based) |
| Reusability | High (MCP export, tool registry) | Framework-locked |
| Permissions | Built-in | Not available |

---

## 2. Agent Definition

### JamJet
```python
from jamjet import Agent

risk_profiler = Agent(
    name="risk_profiler",
    model="claude-sonnet-4-6",
    tools=[get_client_profile, assess_risk_score],
    instructions="You are a CFP specializing in risk...",
    strategy="plan-and-execute",   # or "react", "critic"
    max_iterations=5,
    max_cost_usd=1.0,
    timeout_seconds=300,
)

result = await risk_profiler.run("Assess risk for client C-1001")
# result.output  — text
# result.tool_calls — structured call log
# result.duration_us — timing
```

### Google ADK
```python
from google.adk.agents import Agent

risk_profiler = Agent(
    name="risk_profiler",
    model="gemini-2.0-flash",
    instruction="You are a CFP specializing in risk...",
    tools=[get_client_profile, assess_risk_score],
)
# No direct .run() — must use a Runner
```

### Verdict
| Aspect | JamJet | Google ADK |
|--------|--------|------------|
| Strategy selection | 3 built-in (plan-and-execute, react, critic) | None (model decides) |
| Cost controls | `max_cost_usd`, `max_iterations`, `timeout` | None built-in |
| Direct execution | `agent.run(prompt)` returns `AgentResult` | Requires Runner + Session |
| Structured output | `AgentResult` with tool_calls, timing | Event stream |
| Compile to IR | `agent.compile()` → canonical IR dict | Not available |

---

## 3. State Management

### JamJet
```python
from pydantic import BaseModel

@workflow.state
class AdvisoryState(BaseModel):
    client_id: str
    risk_assessment: str | None = None
    market_analysis: str | None = None
    # ...

# State flows through steps — immutable updates
@workflow.step
async def assess_risk(state: AdvisoryState) -> AdvisoryState:
    result = await risk_profiler.run(...)
    return state.model_copy(update={"risk_assessment": result.output})
```

- **Typed Pydantic model** — compile-time validation, IDE autocomplete
- **Immutable updates** — `model_copy(update={...})` creates new state
- **JSON Schema** generated automatically for runtime validation
- **Event-sourced** — every state transition recorded

### Google ADK
```python
# State is a plain dict on the session
session = await session_service.create_session(
    app_name="wealth_management",
    state={"client_id": "C-1001"},
)

# Agents mutate state directly via session.state['key'] = value
# (The agent writes to state via tool calls or output instructions)
```

- **Untyped dict** — no compile-time checks, no autocomplete
- **Mutable shared state** — agents write directly, risk of conflicts
- **No schema validation** — runtime errors if key missing/wrong type
- **No event sourcing** — state changes are not recorded

### Verdict
| Aspect | JamJet | Google ADK |
|--------|--------|------------|
| Type safety | Pydantic models | Plain dict |
| Immutability | Yes (model_copy) | No (mutable) |
| Validation | JSON Schema + Pydantic | None |
| Audit trail | Event-sourced | Not available |
| Serialization | Automatic | Manual |

---

## 4. Orchestration

### JamJet
```python
workflow = Workflow("wealth_management", version="0.1.0")

@workflow.step
async def assess_risk(state): ...

@workflow.step
async def analyze_markets(state): ...

@workflow.step(human_approval=True)
async def build_recommendation(state): ...

# Conditional routing
@workflow.step(
    next={"fast_path": lambda s: s.score > 80,
          "review_path": lambda s: True}
)
async def evaluate(state): ...
```

- **Decorator-based** step registration
- **Conditional routing** with lambda predicates
- **Human approval gates** as first-class primitive
- **Compiles to IR** → can run locally or on durable Rust runtime
- **Graph-based execution** with loop detection

### Google ADK
```python
from google.adk.agents import SequentialAgent, ParallelAgent

advisor = SequentialAgent(
    name="wealth_advisor",
    sub_agents=[risk_profiler, market_analyst, tax_strategist, architect],
)

# Or parallel execution:
research = ParallelAgent(
    name="research",
    sub_agents=[market_analyst, tax_strategist],
)
```

- **Composition-based** (SequentialAgent, ParallelAgent, LoopAgent)
- **No conditional routing** — must implement in agent instructions
- **No human approval** — must build custom solution
- **In-memory only** — no durable execution
- **Agent transfer** — agents can delegate to sub-agents dynamically

### Verdict
| Aspect | JamJet | Google ADK |
|--------|--------|------------|
| Orchestration model | Workflow DAG with typed state | Agent composition tree |
| Conditional routing | Lambda predicates on state | Agent decides at runtime |
| Parallel execution | Via workflow graph (Phase 2) | `ParallelAgent` built-in |
| Human-in-the-loop | First-class `human_approval=True` | Not available |
| Durable execution | Rust runtime + event sourcing | In-memory only |
| Loop agents | Graph cycles with safety limits | `LoopAgent` primitive |

---

## 5. Execution Model

### JamJet
```
Local:    workflow.run(state) → in-process, Python
Runtime:  jamjet dev → Rust server, durable, multi-tenant

Features:
  ✅ Durable execution (survives process crashes)
  ✅ Event sourcing (full audit trail)
  ✅ Tenant isolation (multi-tenant by design)
  ✅ MCP bridge (8 tools for external clients)
  ✅ Compile-time IR (inspect before running)
  ✅ Human approval gates
  ✅ Worker-based execution (scale horizontally)
```

### Google ADK
```
Local:    InMemoryRunner → in-process, Python
Cloud:    Vertex AI Agent Engine (managed)

Features:
  ✅ Simple in-memory execution
  ✅ Event streaming (real-time agent output)
  ✅ Vertex AI integration (managed scaling)
  ✅ Built-in Gemini model access
  ❌ No durable execution in OSS version
  ❌ No event sourcing / audit trail
  ❌ No human approval primitive
  ❌ No multi-tenant support
```

### Verdict
| Aspect | JamJet | Google ADK |
|--------|--------|------------|
| Local dev | `workflow.run()` | `InMemoryRunner` |
| Production | Self-hosted Rust runtime | Vertex AI (managed) |
| Durability | Event-sourced, crash-resilient | In-memory (OSS) |
| Scaling | Worker pool + durable queue | Vertex AI auto-scale |
| Vendor lock-in | None (self-hosted, any LLM) | Moderate (Gemini-first) |

---

## 6. Protocol Support

### JamJet
```
✅ MCP Client  — consume external MCP tools
✅ MCP Server  — expose tools to MCP clients (serve_tools())
✅ MCP Bridge  — expose runtime ops as 8 MCP tools at /mcp
✅ A2A Client  — delegate tasks to remote agents
✅ A2A Server  — receive tasks from other frameworks
✅ ANP         — W3C DID-based agent discovery
```

### Google ADK
```
✅ Google tools — Search, Code Execution, etc.
✅ MCP Client  — connect to MCP servers (via McpToolset)
❌ MCP Server  — cannot expose tools as MCP server
❌ A2A         — no Agent-to-Agent protocol support
❌ ANP         — no DID-based discovery
```

### Verdict
JamJet is protocol-native — designed from the ground up for multi-framework
interoperability. Google ADK is ecosystem-focused — optimized for Google's
model and tool ecosystem.

---

## 7. Code Comparison — Same Task, Both Frameworks

### "Assess risk for a client"

**JamJet (13 lines)**
```python
risk_profiler = Agent(
    name="risk_profiler",
    model="claude-sonnet-4-6",
    tools=[get_client_profile, assess_risk_score],
    instructions="You are a CFP...",
    strategy="plan-and-execute",
    max_iterations=5,
)
result = await risk_profiler.run(f"Assess risk for client {client_id}")
print(result.output)
print(f"Tool calls: {len(result.tool_calls)}")
print(f"Duration: {result.duration_us / 1e6:.2f}s")
```

**Google ADK (25+ lines)**
```python
agent = Agent(
    name="risk_profiler",
    model="gemini-2.0-flash",
    instruction="You are a CFP...",
    tools=[get_client_profile, assess_risk_score],
)
session_service = InMemorySessionService()
runner = InMemoryRunner(agent=agent, app_name="risk", session_service=session_service)
session = await session_service.create_session(app_name="risk", user_id="u1", state={})
prompt = types.Content(role="user", parts=[types.Part(text=f"Assess risk for {client_id}")])
async for event in runner.run_async(session_id=session.id, user_id="u1", new_message=prompt):
    if event.content and event.content.parts:
        print(event.content.parts[0].text)
```

---

## 8. Summary

| Dimension | JamJet | Google ADK |
|-----------|--------|------------|
| **Philosophy** | Runtime-first, protocol-native | SDK-first, Gemini-native |
| **Best for** | Production multi-agent systems needing durability, audit, and interop | Rapid prototyping with Gemini models |
| **Agent strategies** | 3 built-in (plan-and-execute, react, critic) | Model-determined |
| **State model** | Typed Pydantic + immutable updates | Mutable dict |
| **Orchestration** | Workflow DAG + conditional routing + HITL | Agent composition (sequential/parallel/loop) |
| **Durability** | Event-sourced Rust runtime | In-memory (OSS) / Vertex AI (managed) |
| **Protocol support** | MCP + A2A + ANP (full interop) | MCP client only |
| **Execution** | Local + self-hosted runtime | Local + Vertex AI |
| **LLM support** | Any OpenAI-compatible API | Gemini-first |
| **Language** | Python SDK + Rust runtime | Python SDK |
| **Human-in-the-loop** | First-class workflow primitive | Not available |
| **Compliance/Audit** | Event sourcing + audit log | Not available |

### When to choose JamJet
- You need **durable, crash-resilient** multi-agent workflows
- **Regulatory compliance** requires full audit trails (financial services, healthcare)
- You want to use **any LLM** (Claude, GPT, Gemini, open-source)
- Your agents need to **interoperate** with other frameworks via MCP/A2A
- You need **human approval gates** in your workflow
- **Multi-tenant** isolation is a requirement

### When to choose Google ADK
- You're building with **Gemini models** and want tight integration
- You want the **fastest path to a prototype** with minimal boilerplate
- **Vertex AI** managed hosting fits your deployment model
- You need **parallel agent execution** out of the box
- Your use case doesn't require durability or compliance audit trails
