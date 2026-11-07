# Deliberative Collective Intelligence (DCI)

Four reasoning archetypes collaborate through structured deliberation to solve complex problems — inspired by [arXiv:2603.11781](https://arxiv.org/abs/2603.11781).

Each agent uses a different JamJet reasoning strategy, showing how multiple strategies compose in a single workflow.

## Agents

| Agent | Role | Strategy |
|-------|------|----------|
| Framer | Structure the problem space | `react` |
| Explorer | Generate alternative approaches | `react` |
| Challenger | Stress-test proposals, find weaknesses | `critic` |
| Integrator | Synthesize all perspectives | `plan-and-execute` |

## Quick Start

```bash
pip install 'jamjet[model]'
export ANTHROPIC_API_KEY=sk-ant-...     # the agents use claude-haiku-4-5
python deliberation.py
```

## What It Demonstrates

- Multi-agent workflows with typed shared state
- Composing different reasoning strategies (react, critic, plan-and-execute)
- Sequential agent collaboration with structured handoffs
- A research pattern written as a typed, step-by-step workflow
