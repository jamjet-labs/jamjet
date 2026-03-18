# LDP Identity-Aware Routing

Route tasks to the best agent using LDP (LLM Delegate Protocol) identity cards, then synthesize answers weighted by provenance metadata.

## What It Demonstrates

1. **LDP identity cards on agents** -- each agent carries metadata (`reasoning_profile`, `cost_profile`, `quality_score`, `domains`) in `ldp.*` labels on its AgentCard
2. **Identity-aware routing** -- `LdpCoordinatorStrategy` scores agents by matching task difficulty to identity metadata, mirroring the LDP paper's routing algorithm (RQ1: ~12x latency improvement on easy tasks)
3. **Provenance-weighted synthesis** -- all agents answer in parallel; a synthesis step weights answers by routing rank + provenance quality + confidence (RQ3)

## Agents

| Agent | Profile | Cost | Quality | Best For |
|-------|---------|------|---------|----------|
| quick-lookup | fast | low | 0.55 | Simple factual questions |
| deep-analyst | analytical | high | 0.92 | Complex multi-step analysis |
| domain-specialist | domain-expert | medium | 0.78 | Finance, healthcare, legal |

## Run

```bash
python examples/ldp-identity-routing/main.py
```

No API keys needed -- uses mock responses by default.

## Using Real LLMs

To run with actual LLM calls:

1. Set your API key:
   ```bash
   export ANTHROPIC_API_KEY=sk-...
   ```

2. In `main.py`, uncomment the `REAL_AGENTS` block and the `jamjet.Agent` import

3. In `execute_agent()`, replace the mock logic with:
   ```python
   agent = REAL_AGENTS[agent_uri]
   result = await agent.run(question)
   return {"output": result.output, "confidence": 0.8}
   ```

4. For real synthesis, replace `synthesize()` with an LLM call using the generated prompt

## Key Concepts

### LDP Identity Cards

LDP extends agent metadata beyond name + skills. Each agent declares its reasoning profile, cost/latency characteristics, and quality score. This enables routing decisions based on *who the agent is*, not just what it claims to do.

```python
labels={
    "ldp.delegate_id": "ldp:delegate:deep-analyst",
    "ldp.model_family": "Claude",
    "ldp.reasoning_profile": "analytical",
    "ldp.cost_profile": "high",
    "ldp.quality_score": "0.92",
}
```

### Difficulty-Based Routing

The strategy classifies task difficulty using lightweight heuristics (~2ms, no LLM call), then matches to the best agent:

- **Easy** tasks prefer fast + cheap agents (quick-lookup)
- **Hard** tasks prefer analytical + high-quality agents (deep-analyst)
- **Domain** tasks get a bonus for matching domain expertise

### Provenance-Weighted Synthesis

Every result carries provenance: who produced it, their quality score, and their self-reported confidence. The synthesis step uses this to weight conflicting answers rather than treating all sources equally.

## Connection to the LDP Paper

This example implements concepts from two research questions:

- **RQ1 (Routing Quality):** Identity-aware routing sends easy tasks to fast/cheap agents and hard tasks to capable ones, reducing latency ~12x on simple queries
- **RQ3 (Provenance Value):** Structured provenance enables weighted synthesis, improving decision quality when provenance signals are accurate

Paper: [arXiv:2603.08852](https://arxiv.org/abs/2603.08852)
Interactive explainer: [sunilprakash.com/research/ldp/](https://sunilprakash.com/research/ldp/)
Reference implementation: [github.com/sunilp/ldp-protocol](https://github.com/sunilp/ldp-protocol)
