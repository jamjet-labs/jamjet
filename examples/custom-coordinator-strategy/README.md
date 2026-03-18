# Custom Coordinator Strategy Example

Write a custom `CoordinatorStrategy` for domain-specific agent routing.

## Use Case

A healthcare platform routes medical queries to specialized agents:
- Weights medical certification 3x higher than other dimensions
- Requires "healthcare" trust domain
- No LLM tiebreaker (predictability over flexibility in medical contexts)

## Run

```bash
python examples/custom-coordinator-strategy/main.py
```
