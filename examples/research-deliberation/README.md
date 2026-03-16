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
pip install jamjet
export OPENAI_API_KEY=your-key          # or use Ollama:
# export OPENAI_BASE_URL=http://localhost:11434/v1
# export OPENAI_API_KEY=ollama
python deliberation.py
```

## What It Demonstrates

- Multi-agent workflows with typed shared state
- Composing different reasoning strategies (react, critic, plan-and-execute)
- Sequential agent collaboration with structured handoffs
- Research-grade patterns running on a durable workflow engine
