# Agent-as-Tool Example

Demonstrates JamJet's Agent-as-Tool feature with all three invocation modes.

## Use Case

A research paper processing pipeline uses agents as tools:
- **Classifier** (sync) -- categorizes papers by field
- **Researcher** (streaming) -- deep analysis with progress
- **Reviewer** (conversational) -- iterative peer review

## Invocation Modes

| Mode | Use When | Example |
|------|----------|---------|
| **Sync** | Quick, stateless tasks | Classification, summarization |
| **Streaming** | Long tasks, partial results | Research, data analysis |
| **Conversational** | Iterative refinement | Review, negotiation |

## Run

```bash
python examples/agent-as-tool/main.py
```
