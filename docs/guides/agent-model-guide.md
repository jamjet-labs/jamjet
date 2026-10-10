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

### Enforcing `allowed_tools`

The runtime holds a node to an agent's `allowed_tools` when the node acts for that agent: the workflow IR's top-level `agent_ref`, or a node's own `agent_ref`, names the card's `uri`. A node acting for two agents must be on both lists.

```json
{
  "workflow_id": "research",
  "agent_ref": "jamjet://acme/research-analyst",
  "nodes": { "search": { "id": "search", "agent_ref": "jamjet://acme/research-analyst", "kind": { "type": "mcp_tool", "server": "web", "tool": "search_web", "input_mapping": {}, "output_schema": "" } } }
}
```

- Matched: an `mcp_tool`, `tool` or `finalizer` node's tool, an `a2a_task` skill, an `agent` node's agent, a function node's handler (`module:function`, never its node id). Model and condition nodes are not tool uses.
- ADK tool calls: each call's name and the code it resolves to must both be listed. The code is matched by `module:function` or by the function's name, so a tool keeps working with just its name unless it is registered under a name different from its function's.
- An absent or empty list restricts nothing. To refuse every tool, use a policy `blocked_tools: ["*"]`.
- Policy still applies on top: a tool on the list can still be blocked or approval-gated.
- Not enforced yet: the card's `blocked_tools`, `require_approval_for` and `allowed_delegations`. Use policy for the first two.

---

## Autonomy Levels

| Level | What it means |
|-------|--------------|
| `deterministic` | Follows the workflow graph exactly — no self-direction |
| `guided` | Follows the graph, but chooses how to approach each node (default) |
| `bounded_autonomous` | Self-directs within iteration, token, and cost budgets |
| `fully_autonomous` | Operates freely within guardrails — requires explicit policy approval |

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

When a `bounded_autonomous` agent hits a budget limit:
1. A `budget_exceeded` event is emitted
2. The workflow routes to the configured escalation handler:
   - `human_approval` node — a human decides what to do
   - supervisor agent node — a supervisor agent takes over
   - `fail` — the workflow fails with a clear error
