# Agent Model Guide

Everything you need to know about defining, deploying, and managing agents in JamJet.

---

## Defining an Agent

```yaml
# agents.yaml
agents:
  research-analyst:
    name: Research Analyst
    description: Deep research on any topic
    model: default_chat
    system_prompt: prompts/researcher.md
    autonomy: bounded_autonomous
    constraints:
      max_iterations: 25
      max_tool_calls: 50
      token_budget: 100000
      cost_budget_usd: 5.00
      time_budget_secs: 600
      max_consecutive_errors: 3
      on_limit_exceeded: escalate_to_human
      allowed_tools: [search_web, read_document, analyze_data]
      blocked_tools: ["delete_*", "write_to_production_*"]
      require_approval_for: [publish, external_api_write]
```

The runtime reads these settings from the Agent Card registered with `POST /agents` (`JamjetClient.register_agent` in the Python SDK), where they sit under `autonomy` and `constraints`. Nothing in this repository compiles an `agents.yaml` file into a card yet; the file above shows the card's fields.

### What each constraint does

| Constraint | Enforced | Where |
|------------|----------|-------|
| `allowed_tools`, `blocked_tools`, `require_approval_for` | Yes | Every node that acts for the agent (`agent_ref`), on the HTTP claim route, the in-process worker and the completion gate. See below. |
| `allowed_delegations` | Yes | Same nodes: `agent`, `agent_tool` and `a2a_task` targets. See below. |
| `max_iterations`, `max_tool_calls`, `token_budget`, `cost_budget_usd`, `time_budget_secs`, `max_consecutive_errors` | Yes | Every node that acts for the agent, and an `agent` node calling it, at every autonomy level, on the HTTP claim route and the in-process worker. Counted from the run's event log. See [Autonomy limits](#autonomy-limits). |
| `on_limit_exceeded` | Yes | `fail` (default) or `escalate_to_human`. See [Autonomy limits](#autonomy-limits). |

### Enforcing `allowed_tools`

The runtime holds a node to an agent's `allowed_tools` when the node acts for that agent: the workflow IR's top-level `agent_ref`, or a node's own `agent_ref`, names the card's `uri`. A node acting for two agents must be on both lists.

From the Python SDK, `Agent("researcher", ..., agent_ref="jamjet://acme/research-analyst")` emits the workflow's `agent_ref` on the durable path (`run_durable`); in YAML use `agent_ref:` in the `workflow:` header and `acts_for:` on a node.

```json
{
  "workflow_id": "research",
  "agent_ref": "jamjet://acme/research-analyst",
  "nodes": { "search": { "id": "search", "agent_ref": "jamjet://acme/research-analyst", "kind": { "type": "mcp_tool", "server": "web", "tool": "search_web", "input_mapping": {}, "output_schema": "" } } }
}
```

- Matched: an `mcp_tool`, `tool` or `finalizer` node's tool, an `a2a_task` skill, an `agent` node's agent, an `agent_tool` node's explicit agent, a function node's handler (`module:function`, never its node id). Nodes that choose their target at run time (`coordinator`, `agent_discovery`, an `auto` agent tool) are refused under a list. Model and condition nodes are not tool uses.
- ADK tool calls: each call's name and the code it resolves to must both be listed. A path pattern (`app.tools:*`) matches the code's `module:function`; a bare name pattern matches only the function's own name. A tool keeps working with just its name unless it is registered under a name different from its function's. The code is known only when the run's `tools` map has an entry for the call; without one, the call is matched on its name.
- The agent's status is not consulted, and cards are global: the first card registered at a URI is the one enforced.
- An absent or empty list restricts nothing. To refuse every tool, use a policy `blocked_tools: ["*"]`.
- Policy still applies on top: a tool on the list can still be blocked or approval-gated.
- `allowed_delegations` restricts whom the agent delegates to: an `agent` node's `agent_ref`, an `agent_tool` node's explicit target, or the URL an `a2a_task` node's `remote_agent` maps to in the workflow's `remote_agents` (a name with no entry is refused), matched as glob patterns. A node that picks its target at run time is refused under a list; an empty list restricts nothing. Subgraphs are not delegations.
- The card's `blocked_tools` and `require_approval_for` apply to the same nodes and match the same way. A card block wins over the card's `allowed_tools`; a card gate holds the node for an ordinary approval, with the policy chain's deadline. For an ADK dispatch, the request lists every gated call with its arguments, and a call the policy chain blocks refuses the batch first. A node that picks its target at run time (coordinator, agent discovery, `auto` agent tool) is refused by any non-empty card block list and gated by any non-empty card gate list.

---

## Autonomy Levels

| Level | What it means | What the runtime enforces for it |
|-------|--------------|------|
| `deterministic` | Follows the workflow graph exactly, no self-direction | The card's lists and limits |
| `guided` | Follows the graph, but chooses how to approach each node (default) | The card's lists and limits |
| `bounded_autonomous` | Self-directs within iteration, token, and cost budgets | The card's lists and limits |
| `fully_autonomous` | Operates freely within guardrails | The card's lists and limits |

The level does not change what is enforced: every level gets the tool and delegation lists and every limit the card declares, and the circuit breaker applies even to a card that declares no constraints. Guided mode does not hold every tool call for approval; name the tools in `require_approval_for` for that.

---

## Agent Lifecycle

```bash
# Register and activate
jamjet agents activate research-analyst

# Check status
jamjet agents inspect research-analyst

# Pause (stops accepting new tasks, existing tasks continue)
jamjet agents pause research-analyst

# Resume
jamjet agents resume research-analyst

# Deactivate (drains in-flight tasks, then goes offline)
jamjet agents deactivate research-analyst

# List all agents
jamjet agents list
```

---

## Agent Card

The Agent Card is the machine-readable identity of your agent. Once activated, it is:
- Stored in the JamJet agent registry
- Served at `/.well-known/agent.json` (if A2A is enabled)
- Discoverable by other agents via the registry API

```bash
# View the Agent Card for a registered agent
jamjet agents inspect research-analyst --card
```

---

## Using Agents in Workflows

```yaml
nodes:
  research:
    type: agent
    agent_ref: research-analyst
    input:
      topic: "{{ state.research_topic }}"
    output_schema: schemas.ResearchReport
    next: review
```

When an `agent` node runs, JamJet invokes the referenced agent with the given input and maps its output into the workflow state.

---

## Autonomy limits

A node that acts for an agent (the workflow's `agent_ref`, or the node's own) is one step of that agent. So is an `agent` node, for the agent it calls; that agent's tool and delegation lists do not apply to the node, only its limits. When the runtime lets such a node fire, it records an `agent_step_admitted` event naming the agents and the tool calls the step makes. Every limit is checked before the step fires, against what the agent used in the run so far, all of it read from the run's event log:

| Limit | What is counted | Refused when |
|-------|-----------------|--------------|
| `max_iterations` | Steps admitted, retries included | The agent has taken that many steps |
| `max_tool_calls` | One per step whose node uses a tool (`tool`, `mcp_tool`, `finalizer`, an `a2a_task` with a skill, `agent`, `agent_tool`, function nodes, and nodes that pick a target at run time), plus each call of an ADK dispatch batch. Model and condition steps count none | This step's calls would take the total past the limit (a step that makes no call is not limited) |
| `token_budget` | `input_tokens + output_tokens` on the agent's completed steps | The total is at or over the budget |
| `cost_budget_usd` | `cost_usd` on the agent's completed steps | The total is at or over the budget |
| `time_budget_secs` | Wall-clock time since the agent's first step in the run | That long has passed |
| `max_consecutive_errors` (default 3) | The agent's failed steps since its last completed step | That many in a row have failed |

- Counted over the whole run: a continue-as-new segment and an engine restart keep the counts. A node's output or `state_patch` cannot change them.
- A limit of 0 refuses the first step (for `time_budget_secs`, the second).
- Token and cost totals grow only when a step reports. The step that crosses a budget is not refused (its usage is unknown until it finishes), so a total can end above the budget by that step's usage. A time budget is checked when a step starts; a running step is not interrupted.
- Usage is what the step reports. The in-process model executor reports tokens. No executor in this runtime reports a cost, so cost is counted only when a worker reports `cost_usd` on `POST /work-items/:id/complete` (the Python worker forwards `input_tokens`, `output_tokens` and `cost_usd` from a node's output); with nothing reported, the cost total stays 0 and a budget above 0 never trips. A worker that under-reports, or reports a failed call as completed, is believed.
- Only steps admitted while the guard records them count. A step running when the runtime is upgraded to this release has no record and adds nothing.
- Two claims cannot both take the last step: the step is recorded in the same compare-and-set that checks the limits.
- Over HTTP, a result for a node acting for an agent is recorded only for a work item the claim route admitted (409 otherwise), and only once per admission.
- A step is recorded when the guard lets the node fire. In the in-process worker a few things can still stop it after that (a spent workflow-level budget, an idempotency key another worker holds); such a step counts though it did not run. This errs toward refusing.
- Only an `agent` node charges the agent it calls. An `agent_tool`, `a2a_task` or coordinator that reaches a registered agent does not charge that agent's limits; attribute the node with `acts_for` to count it. An in-process worker built without an agent registry does not apply a called agent's limits (nodes acting for an agent are refused there).

On a limit, with `on_limit_exceeded: fail` (the default), the node is refused and fails for good, which fails the run. The runtime records `autonomy_limit_reached` (`limit_type` is `max_iterations`, `max_tool_calls`, `token_budget`, `cost_budget` or `time_budget`) or `circuit_breaker_tripped`, and the node's `node_failed` error names the agent and the limit (it contains `agent autonomy`).

With `on_limit_exceeded: escalate_to_human`, the node is held for an ordinary approval instead: the request is bound to the action like any other (the same digest), carries the policy chain's deadline, and shows the limit under `autonomy_limit` in its context. `autonomy_limit_reached` (or `circuit_breaker_tripped`) and `escalation_required` (target `human_approval`) are recorded. An approval releases that one step, once: a retry of a step that failed, a duplicate item, or the next step past the limit asks again. Only a request raised for the limit (one whose context shows `autonomy_limit`) can release it, so an approval given for a policy or card gate before the limit was reached does not. A failing card's limit reached while the approved step was being decided still refuses it. Without a deadline in the policy chain the request does not expire. A rejection or an expired request fails the node. If a node acts for several agents and one card says `fail`, the node is refused. RFC-005's `escalate_to_supervisor` is not implemented, and a card that sets it is refused at registration.

A workflow-level `token_budget` or `cost_budget_usd` in the IR is a separate check, made by the in-process worker for the nodes it runs; the HTTP claim route does not make it.
