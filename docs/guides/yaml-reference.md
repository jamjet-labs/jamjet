# YAML Reference

JamJet workflows can be fully defined in YAML. This reference covers every file and every field.

---

## Project files

A JamJet project uses these files:

| File | Required | Purpose |
|------|----------|---------|
| `workflow.yaml` | Yes | Workflow graph definition |
| `agents.yaml` | Yes | Agent definitions |
| `tools.yaml` | No | Tool definitions (Python tools registered here) |
| `models.yaml` | No | Model provider configuration |
| `policies.yaml` | No | Policy rules |
| `schemas.py` | No | Pydantic state schema definitions |
| `jamjet.yaml` | Yes | Project config |

---

## `jamjet.yaml`

```yaml
project:
  name: my-agent-project
  runtime_version: "0.1"
  default_workflow: research
```

---

## `workflow.yaml`

```yaml
workflow:
  id: research                      # unique workflow ID
  version: 0.1.0                    # semver
  state_schema: schemas.ResearchState  # Pydantic model for shared state
  start: search                     # entry node ID
  agent_ref: jamjet://acme/researcher  # optional: the registered Agent Card this workflow acts for

nodes:
  search:
    type: tool                      # node type (see Node Types below)
    tool_ref: tools.web_search      # registered tool reference
    input:
      query: "{{ state.question }}" # Jinja2 template expressions
    output_schema: schemas.SearchResult
    retry_policy: io_default        # named retry policy
    acts_for: jamjet://acme/researcher  # optional: an Agent Card this node also acts for
    next: summarize                 # next node (or list for branching)

  summarize:
    type: model
    model: default_chat             # named model from models.yaml
    prompt: prompts/summarize.md    # prompt template file
    input:
      search_result: "{{ state.search_result }}"
    output_schema: schemas.Summary
    next: end                       # built-in terminal node
```

`agent_ref` (workflow) and `acts_for` (node) compile to the IR's `agent_ref`. The engine then holds the node to the named card's `allowed_tools`, `blocked_tools`, `require_approval_for` and `allowed_delegations` (see the [Agent Model Guide](agent-model-guide.md#enforcing-allowed_tools)), and refuses it if no agent is registered at that `uri`. A node uses `acts_for` because `agent_ref` on an `agent` node already names the agent it calls.

---

## Node types

| Type | Description | Who runs it |
|------|-------------|-------------|
| `model` | LLM call | the engine |
| `condition` | Branch on a state expression | the engine |
| `human_approval` | Wait for a human decision | the engine |
| `eval` | Evaluate node output with scorers *(Phase 3)* | the engine |
| `python_fn` | A Python `@tool` or `@workflow.step` function | an external `jamjet worker` |
| `java_fn` | A Java `@Tool` method | an external Java tool worker |
| `external_fn` | A handler on a queue the node names | an external worker on that queue |

The engine has no executor for `tool`, `mcp_tool`, `a2a_task`, `agent`,
`agent_tool`, `coordinator`, `wait`, `parallel`, `join`, `subgraph`,
`memory_retrieval`, `policy` or `finalizer`. `POST /workflows` refuses a
workflow with any of them (400, naming each node and its kind), including
when it re-registers a version already stored. Until 2026-11 such a node was
registered and then completed with an empty output without running. A
version stored before then still starts (`POST /executions`, cron); when the
run reaches such a node, the node fails with `no executor for node kind <kind>`
and so does the run. Fan-out and fan-in need no node: a node with several `next` targets
runs them all, and a node with several incoming edges waits for all of them.

---

## Condition node

```yaml
route:
  type: condition
  branches:
    - condition: "state.confidence >= 0.8"
      next: deliver
    - condition: "state.confidence < 0.8"
      next: escalate
  default: escalate
```

---

## Human approval node

```yaml
review:
  type: human_approval
  description: "Please review the generated report"
  timeout: 48h                      # optional: the request expires after this
  fallback: escalate                # optional: where the run goes on expiry (`end` to stop)
  next: deliver                     # taken when approved
```

Approved, the run continues along `next`, and `state.review` holds the
decision. Rejected, the node and the run fail. Expired, the run goes to
`fallback` (or stops, for `end`), or fails if there is none. See
[Human-in-the-Loop](hitl.md#approval-nodes).

---

## Wait / external event node

Not supported: the engine has no executor for `wait`, and registration
refuses it. To wait for a person, use a `human_approval` node.

---

## MCP tool node

Registration refuses this kind: the engine has no executor for it. The shape
below is what the compiler emits.

```yaml
search_github:
  type: mcp_tool
  server: github                    # server name from agents.yaml mcp.servers
  tool: search_code
  input:
    query: "{{ state.search_query }}"
  output_schema: schemas.SearchResults
  retry_policy: io_default
```

---

## A2A task node

Registration refuses this kind: the engine has no executor for it. The shape
below is what the compiler emits.

```yaml
code_review:
  type: a2a_task
  remote_agent: partner_reviewer    # agent name from agents.yaml a2a.remote_agents
  skill: security_review
  input:
    code: "{{ state.generated_code }}"
  stream: true
  timeout: 300s
  on_input_required: human_review   # route if remote agent needs more info
  retry_policy: io_default
```

---

## `agents.yaml`

```yaml
agents:
  researcher:
    model: default_chat
    system_prompt: prompts/researcher.md
    output_schema: schemas.ResearchResult
    autonomy: guided                # deterministic | guided | bounded_autonomous | fully_autonomous
    constraints:
      max_iterations: 20
      token_budget: 50000

    # MCP servers this agent connects to
    mcp:
      servers:
        github:
          transport: http_sse
          url: https://mcp.github.com/v1
          auth:
            type: bearer
            token_env: GITHUB_TOKEN
        filesystem:
          transport: stdio
          command: npx
          args: ["-y", "@modelcontextprotocol/server-filesystem", "/tmp/workspace"]

    # A2A agents this agent can delegate to
    a2a:
      remote_agents:
        code_reviewer:
          url: https://agents.partner.com
          auth:
            type: bearer
            token_env: PARTNER_API_KEY
```

`autonomy` and `constraints` mirror the Agent Card's fields. The runtime reads them from the card registered with `POST /agents`, not from this file: see [What each constraint does](agent-model-guide.md#what-each-constraint-does).

---

## `models.yaml`

```yaml
models:
  default_chat:
    provider: openai
    model: gpt-4o
    timeout: 60s
    retry_policy: llm_default
    temperature: 0.2

  fast_chat:
    provider: anthropic
    model: claude-3-haiku-20240307
    timeout: 30s
```

---

## `tools.yaml`

```yaml
tools:
  get_ticket:
    kind: python
    ref: app.tools:get_ticket       # module:function
    input_schema: schemas.TicketInput
    output_schema: schemas.Ticket
    permissions: [read_only]
```

---

## Retry policies

Built-in named policies:

| Name | Max attempts | Backoff |
|------|-------------|---------|
| `llm_default` | 3 | Exponential, jitter |
| `io_default` | 5 | Exponential, jitter |
| `fast` | 2 | Fixed 1s |

Custom policy:

```yaml
retry_policies:
  my_policy:
    max_attempts: 4
    backoff: exponential
    base_delay: 2s
    max_delay: 30s
    jitter: true
    retryable_errors: [timeout, rate_limit, 503]
```

---

## Template expressions

Node inputs support [Jinja2](https://jinja.palletsprojects.com/) expressions:

```yaml
input:
  query: "{{ state.question }}"
  context: "{{ state.search_result.answer | truncate(500) }}"
  flag: "{{ state.retries > 2 }}"
```

---

## Next steps

- [Python SDK](python-sdk.md) — equivalent Python authoring
- [Workflow Authoring Guide](workflow-authoring.md) — patterns and best practices
- [MCP Integration](mcp-guide.md) — MCP server configuration
- [A2A Integration](a2a-guide.md) — A2A agent configuration
