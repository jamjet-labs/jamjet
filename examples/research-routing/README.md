# Agent Routing by Complexity

Identity-aware routing that dispatches tasks to the most appropriate agent based on complexity classification — inspired by [arXiv:2603.08852](https://arxiv.org/abs/2603.08852) (LDP).

Simple questions get fast answers. Complex problems get deep analysis. The router decides.

## Agents

| Agent | Tier | Strategy | When to Use |
|-------|------|----------|-------------|
| fast-agent | Simple | `react` | Quick factual answers |
| balanced-agent | Moderate | `plan-and-execute` | Multi-step reasoning |
| deep-agent | Complex | `critic` | Deep analysis with self-critique |

## Quick Start

```bash
pip install jamjet
export OPENAI_API_KEY=your-key          # or use Ollama:
# export OPENAI_BASE_URL=http://localhost:11434/v1
# export OPENAI_API_KEY=ollama
python routing.py
```

## What It Demonstrates

- Dynamic agent routing based on task classification
- Cost-aware dispatch (cheap model for simple, expensive for complex)
- Conditional workflow execution with typed state
- Multiple reasoning strategies in a single workflow
