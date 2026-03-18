# Coordinator Routing Example

Demonstrates JamJet's Coordinator node for dynamic agent routing with structured scoring.

## Use Case

A customer support system routes incoming tickets to specialized agents:
- **Billing Agent** — handles payment, refund, and subscription issues
- **Technical Agent** — handles bugs, API errors, and integration problems
- **General Agent** — handles everything else (FAQ, account questions)

The Coordinator discovers available agents, scores them on capability fit and cost,
and routes each ticket to the best match.

## How It Works

1. Ticket arrives with a category and description
2. Coordinator queries the agent registry for candidates
3. Structured scoring ranks agents by skill match, cost, and latency
4. If top candidates are close, LLM tiebreaker reasons about the best fit
5. Selected agent processes the ticket

## Run

```bash
jamjet run examples/coordinator-routing/workflow.yaml
# or
python examples/coordinator-routing/main.py
```
