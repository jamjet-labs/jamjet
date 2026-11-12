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
reaches the agent. The `{"message": "support_message"}` mapping tells the
runtime to extract `support_message` from the workflow state and pass it as
`message` to the agent invocation.

To use the graph with a runtime, replace `https://sentiment.example.com` with
your deployed HTTPS agent endpoint. That agent must implement `/tasks/send` and
perform the classification. A positive/neutral/negative label is the suggested
application contract, not a schema enforced by this example.

Use a worker configured with an agent-tool executor for a live run. The API
server's embedded development worker can complete this node with an empty
result without calling the classifier.
