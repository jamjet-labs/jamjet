# Agent-as-Tool Example

Demonstrates JamJet's Agent-as-Tool feature with all three invocation modes.

## Use Case

A research paper processing pipeline uses three agents as tools:
- **Classifier** (sync) — categorizes papers by field and methodology
- **Researcher** (streaming) — performs deep literature analysis with streamed progress
- **Reviewer** (conversational) — iterative peer review with multi-turn feedback

## Invocation Modes

| Mode | Use When | Example |
|------|----------|---------|
| **Sync** | Quick, stateless tasks | Classification, summarization |
| **Streaming** | Long tasks where partial results help | Research, data analysis |
| **Conversational** | Iterative refinement needed | Review, negotiation |

## Run

```bash
python examples/agent-as-tool/main.py
```
