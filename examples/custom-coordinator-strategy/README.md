# Custom Coordinator Strategy Example

Demonstrates writing a custom `CoordinatorStrategy` for domain-specific agent routing.

## Use Case

A healthcare platform routes medical queries to specialized agents. The custom strategy:
- Weights medical certification 3x higher than other dimensions
- Requires agents to be in the "healthcare" trust domain
- Uses a medical-specific LLM for tiebreaking

## Run

```bash
python examples/custom-coordinator-strategy/main.py
```
