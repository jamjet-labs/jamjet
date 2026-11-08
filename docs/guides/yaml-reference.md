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
    prompt: |                       # rendered against the run's state
      Summarize this search result:

      {{ state.search_result }}
    system_prompt: You summarise search results in three sentences.
    output_schema: schemas.Summary
    next: end                       # built-in terminal node
```

`jamjet validate` and the compiler reject a workflow with no nodes, `nodes:`
nested under `workflow:` or written as a list, a node whose `type` is not in
the table below, and a node nothing leads to (no `next`, branch or `fallback`
points at it), which the engine would otherwise run when the run starts. A key the compiler does not read is dropped with a
warning that names it and what to use instead; these warnings become errors in
a later release. Some other spellings map to the same IR field and are
accepted: `system` for `system_prompt`, `label` for `description`,
`arguments` for `input`, `message` on `human_approval` for `description`, and
`branches` (with `condition:` entries) for a condition node's `conditions`.
`temperature` and `max_tokens` on a model node become that node's model
settings.

Governance settings (`policy`, `cost_budget_usd`, `token_budget`,
`on_budget_exceeded`, `data_policy`, `continue_as_new`) may sit at the top
level or in the `workflow:` header, but not both; a malformed one is an error.
The runtime enforces `policy`, and the in-process worker enforces
`token_budget` and `cost_budget_usd` for the nodes it runs; with
`continue_as_new: true` a run that reaches its budget rolls over into a new
segment. `on_budget_exceeded` and `data_policy` (PII
redaction and retention) are compiled into the IR, and the runtime does not
act on them.

`agent_ref` (workflow) and `acts_for` (node) compile to the IR's `agent_ref`. The engine then holds the node to the named card's `allowed_tools`, `blocked_tools`, `require_approval_for` and `allowed_delegations` (see the [Agent Model Guide](agent-model-guide.md#enforcing-allowed_tools)), and refuses it if no agent is registered at that `uri`. A node uses `acts_for` because `agent_ref` on an `agent` node already names the agent it calls.

---

## Node types

| Type | Description | Who runs it | Keys besides the common ones |
|------|-------------|-------------|------------------------------|
| `model` | LLM call | the engine | `model`, `prompt`, `system_prompt`, `output_schema`, `temperature`, `max_tokens` |
| `condition` (or `branch`) | Branch on a state expression | the engine | `conditions`, `default` |
| `human_approval` | Wait for a human decision | the engine | `fallback` |
| `eval` | Evaluate node output with scorers | the engine | `scorers`, `on_fail`, `max_retries`, `input_expr` |
| `limit_exceeded` | Terminal node for a run that hit a limit | the engine | |
| `end` | Terminal node | the engine | |
| `tool` | Call a registered tool | refused at registration | `tool_ref`, `input`, `output_schema` |
| `mcp_tool` | Call a tool via MCP | refused at registration | `server`, `tool`, `input`, `output_schema` |
| `a2a_task` | Delegate to an external agent via A2A | refused at registration | `remote_agent`, `skill`, `input`, `output_schema`, `stream`, `on_input_required` |
| `agent` | Invoke a registered JamJet agent | refused at registration | `agent_ref`, `input`, `output_schema` |

Every node may also carry `type`, `description`, `labels`, `policy`,
`data_policy`, `retry_policy`, `timeout`, `acts_for` and `next`. On any node
but `human_approval`, `timeout` and `data_policy` are recorded in the IR and
not enforced; a `human_approval` node's `timeout` is its deadline. The
`python_fn`, `java_fn` and `external_fn` kinds, which external workers run,
come from the Python and Java SDKs rather than from YAML.

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

Every node but `start` must have an incoming edge (a `human_approval`
fallback counts as one), and every node must be reachable from `start`.
`POST /workflows` refuses a workflow that breaks either rule, naming the
nodes: a node with no incoming edge would run when the run starts, before
whatever it was meant to follow, and an unreachable one would never run. A
condition's branch targets need edges from the condition too. It also
refuses an edge to a node that does not exist. A version stored before
these checks is not started (`POST /executions` answers 409, naming the
nodes), and in a run of one already under way a node with no incoming edge
other than `start` is skipped, never run.

---

## Condition node

```yaml
route:
  type: condition
  conditions:
    - if: "state.verdict == \"pass\""  # or `when:`; compare with == or != to a JSON literal
      next: deliver                     # or `to:`
    - if: "state.needs_review == true"
      next: escalate
  default: escalate
```

A condition is a `state.<path>` reference, optionally compared with `==` or
`!=` to a JSON literal; the runtime refuses anything else at registration. A
conditional `next:` list on any other node type (`- when: ...`) compiles to a
condition node of its own, `<node>__route`.

---

## Human approval node

```yaml
review:
  type: human_approval
  description: "Please review the generated report"
  timeout: 48h
  fallback: second_review           # where the run goes if nobody decides in time
  next: deliver                     # where an approved run continues

second_review:
  type: human_approval
  description: "Escalated: the first reviewer did not decide within 48 hours"
  timeout: 48h
  next: deliver
```

Approved, the run continues along `next`, and `state.__approvals.review`
holds the decision. Rejected, the node and the run fail. Expired, the run goes to
`fallback` (or stops, for `end`), or fails if there is none; here that is a
second approver. Do not point `fallback` at the step an approval would
unlock: that approves the action by timeout. Only the engine writes
`state.__approvals`, so a condition can route on it; a plain `state.review`
key is ordinary state that any later node can overwrite. Decide with
`jamjet approvals <exec-id>` and
`jamjet approve <exec-id> --decision approved|rejected`. See
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
  server: github                    # a server under mcp.servers in this file
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

A node's `retry_policy` is a name, and the runtime reads it for the number of
attempts the node gets:

| Name | Attempts |
|------|----------|
| `no_retry` | 1 |
| `io_default` | 5 |
| `llm_default` | 3 |
| any other name, or none | 3 |

A failed node is tried again until its attempts run out, waiting 2, 4, 8 and
so on seconds between attempts (at most 64). Some failures end the node at
once: a policy block, or a prompt that does not render. A `retry_policies:`
block (custom `max_attempts`, `backoff`, `jitter`, `retryable_errors`) is
compiled into the IR but the runtime does not read it.

---

## Prompt templates

A `model` node's `prompt` is rendered against the run's state when the node is
dispatched, and the model receives the result as the user message:

```yaml
prompt: |
  Title: {{ state.pr.title }}
  First file: {{ state.pr.files.0 }}
```

- The only expression is `{{ state.<path> }}`: dotted keys, and digits to
  index a list. Filters (`| join(...)`), operators, calls, `{% ... %}` blocks
  and `{# ... #}` comments are not Jinja here. A workflow that uses them is
  refused when it is registered (400, naming the node).
- To write `{{`, `{%` or `{#` as text (a JSON example, say), quote it,
  `{{ '{{' }}`, or wrap the passage in `{% raw %}...{% endraw %}`, as in
  Jinja2.
- A string is inserted as is; any other value as compact JSON. A rendered
  prompt over 256 KiB fails the node.
- A path the run's state does not have fails the node, with the path in the
  error, and so does a prompt that renders empty. The model is not called
  with a prompt that has a gap in it.
- A prompt file reference (`prompt: prompts/summarize.md`: one line, ending
  in `.md`, `.txt`, `.j2` or a similar extension, with no spaces unless it has
  a `/`) is not read: the file stays on the machine that compiled the
  workflow. Put the text inline. A node with a file reference and no
  conversation (`messages`) in state fails and says so.
- A node with a prompt sends only the prompt. The conversation in
  `state.messages` is sent by nodes with no prompt (the agent loop), and is
  left out of a prompted node's work item, so an approval of the node binds
  what the model receives.

The runtime does not render `input:` mappings.

---

## Next steps

- [Python SDK](python-sdk.md) — equivalent Python authoring
- [Workflow Authoring Guide](workflow-authoring.md) — patterns and best practices
- [MCP Integration](mcp-guide.md) — MCP server configuration
- [A2A Integration](a2a-guide.md) — A2A agent configuration
