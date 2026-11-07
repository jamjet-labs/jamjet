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
    system_prompt: You summarise search results in three sentences.
    output_schema: schemas.Summary
    next: end                       # built-in terminal node
```

`jamjet validate` and the compiler reject a workflow with no nodes, a node
whose `type` is not in the table below, and a node, condition or `next` entry
with a key the compiler does not read, naming the key. An unknown key in the
`workflow:` header or at the top level is reported as a warning.

`agent_ref` (workflow) and `acts_for` (node) compile to the IR's `agent_ref`. The engine then holds the node to the named card's `allowed_tools`, `blocked_tools`, `require_approval_for` and `allowed_delegations` (see the [Agent Model Guide](agent-model-guide.md#enforcing-allowed_tools)), and refuses it if no agent is registered at that `uri`. A node uses `acts_for` because `agent_ref` on an `agent` node already names the agent it calls.

---

## Node types

| Type | Description | Keys besides the common ones |
|------|-------------|------------------------------|
| `tool` | Call a registered tool | `tool_ref`, `input`, `output_schema` |
| `model` | LLM call | `model`, `prompt`, `system_prompt`, `output_schema` |
| `condition` (or `branch`) | Branch on a state expression | `conditions`, `default` |
| `human_approval` | Pause for a human decision | `fallback` |
| `mcp_tool` | Call a tool via MCP | `server`, `tool`, `input`, `output_schema` |
| `a2a_task` | Delegate to an external agent via A2A | `remote_agent`, `skill`, `input`, `output_schema`, `stream`, `on_input_required` |
| `agent` | Invoke a registered JamJet agent | `agent_ref`, `input`, `output_schema` |
| `eval` | Evaluate node output with scorers | `scorers`, `on_fail`, `max_retries`, `input_expr` |
| `end` | Terminal node | |

Every node may also carry `type`, `description`, `labels`, `policy`,
`retry_policy`, `timeout`, `acts_for` and `next`.

---

## Condition node

```yaml
route:
  type: condition
  conditions:
    - if: "state.confidence >= 0.8"   # or `when:`
      next: deliver                   # or `to:`
    - if: "state.confidence < 0.8"
      next: escalate
  default: escalate
```

---

## Human approval node

```yaml
review:
  type: human_approval
  description: "Please review the generated report"
  timeout: 48h
  fallback: revise                  # where the run goes if nobody decides in time
  next: deliver                     # where an approved run continues
```

Decide with `jamjet approvals <exec-id>` and
`jamjet approve <exec-id> --decision approved|rejected`.

---

## MCP tool node

```yaml
search_github:
  type: mcp_tool
  server: github                    # a server under mcp.servers in this file
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
  remote_agent: partner_reviewer    # an agent under a2a.remote_agents in this file
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
