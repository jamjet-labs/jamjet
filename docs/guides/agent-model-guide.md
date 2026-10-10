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
| `token_budget`, `cost_budget_usd` | Partly | Only for a `bounded_autonomous` card, only on an `agent` node that calls it (the card named by that node's own `agent_ref` field, which says whom it calls, not the workflow's attribution), and only in the in-process worker, not on the claim route. Compared with the run's token and cost totals as the in-process worker accumulates them (results reported through the HTTP worker protocol add nothing); at or over the limit, or when the registry cannot be read, the node does not run and `autonomy_limit_reached` and `escalation_required` events are recorded (see Budget Enforcement below). |
| `max_iterations`, `max_tool_calls` | No, in effect | Read in the same place as the budgets, but compared with counters the runtime never increments, so a limit above 0 never trips (a limit of 0 always does). |
| `time_budget_secs` | No | Stored, never read. |

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
| `deterministic` | Follows the workflow graph exactly, no self-direction | Nothing beyond the card's tool and delegation lists |
| `guided` | Follows the graph, but chooses how to approach each node (default) | Nothing beyond the card's tool and delegation lists |
| `bounded_autonomous` | Self-directs within iteration, token, and cost budgets | The token and cost budgets, as described in the table above |
| `fully_autonomous` | Operates freely within guardrails | Nothing beyond the card's tool and delegation lists |

The tool and delegation lists apply at every level. A circuit breaker for repeated errors is defined for every level, but the runtime does not count consecutive errors, so it never trips.

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

## Budget Enforcement

When an `agent` node calls a `bounded_autonomous` agent whose card's `token_budget` or `cost_budget_usd` the run has already reached (in-process worker only):
1. `autonomy_limit_reached` and `escalation_required` (target `human_approval`) events are recorded
2. The node does not run. The worker leaves the item to its lease, so it is retried, and refused again, until its attempts run out. Nothing routes to a human or a supervisor agent automatically.

A workflow-level `token_budget` or `cost_budget_usd` in the IR is a separate check, made by the in-process worker for the nodes it runs; the HTTP claim route does not make it.
