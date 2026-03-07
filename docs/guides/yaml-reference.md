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

nodes:
  search:
    type: tool                      # node type (see Node Types below)
    tool_ref: tools.web_search      # registered tool reference
    input:
      query: "{{ state.question }}" # Jinja2 template expressions
    output_schema: schemas.SearchResult
    retry_policy: io_default        # named retry policy
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

---

## Node types

| Type | Description |
|------|-------------|
| `tool` | Call a Python function or HTTP endpoint |
| `model` | LLM call with structured output |
| `condition` | Branch based on state expression |
| `human_approval` | Pause for human decision |
| `wait` | Suspend until timer or external event |
| `parallel` | Fan out to concurrent branches |
| `join` | Wait for all parallel branches |
| `mcp_tool` | Call a tool via MCP protocol |
| `a2a_task` | Delegate to an external agent via A2A |
| `agent` | Invoke a local JamJet agent |
| `eval` | Evaluate node output with scorers *(Phase 3)* |

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
  message: "Please review the generated report"
  timeout: 48h                      # auto-reject after timeout
  on_approved: deliver
  on_rejected: revise
```

---

## Wait / external event node

```yaml
await_payment:
  type: wait
  event: payment_confirmed          # event name sent via POST /executions/{id}/events
  timeout: 24h
  on_timeout: cancel_order
  next: fulfill_order
```

---

## MCP tool node

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
