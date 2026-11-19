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

## Support-message sentiment analysis (sync)

[`sentiment_analysis.py`](sentiment_analysis.py) defines a customer-support
classifier with `agent_tool()` and uses its target, mode and timeout in a
`WorkflowGraph`. The wrapper description documents the definition locally.
Sync mode fits this task
because each message needs one answer and does not need progress updates or a
multi-turn conversation.

From the repository root, with the Python SDK installed:

```bash
python examples/agent-as-tool/sentiment_analysis.py
```

The example prints sample workflow state and the compiled workflow. It runs offline;
it does not classify the message or require an API key. The graph contains one
`classify_sentiment` node, with a five-second timeout and `sentiment` as its
output key.

The workflow demonstrates `input_mapping`, which shows how workflow state
would reach the agent. The `{"message": "support_message"}` mapping is recorded
in the compiled workflow node; nothing in the bundled runtime applies that
mapping today.

This example stops at the compiled workflow. The bundled JamJet runtime has no
executor for `agent_tool` nodes and refuses to register a workflow that contains
one, so there is no live run to try yet. The endpoint URL is a placeholder.
