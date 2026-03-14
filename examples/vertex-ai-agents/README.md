# JamJet + Vertex AI (Gemini) Integration

Run JamJet agents powered by Google Gemini. No JamJet CLI required — pure Python or Java.

## How it works

JamJet agents use the OpenAI-compatible chat completions API under the hood. Google Gemini exposes this same API format, so switching from OpenAI to Gemini is two environment variables:

```
OPENAI_BASE_URL → https://generativelanguage.googleapis.com/v1beta/openai/
OPENAI_API_KEY  → your Gemini API key
```

Everything else — tools, strategies, workflows — works exactly the same.

## Get a Gemini API key

1. Go to [Google AI Studio](https://aistudio.google.com/apikey)
2. Click **Create API key**
3. Copy the key

## Python

### Setup

```bash
cd examples/vertex-ai-agents/python
pip install jamjet-sdk openai
export GOOGLE_API_KEY="your-gemini-api-key"
```

### Run the agent

```bash
python agent.py
```

Custom prompt:

```bash
python agent.py "Analyze GOOG stock and competitive position"
```

### Run the workflow

```bash
python workflow.py
python workflow.py "Globex"
```

### What the examples show

**`agent.py`** — Single research agent using `react` strategy (observe → reason → act loop). Defines three tools (`search_documents`, `get_stock_data`, `save_note`) and runs them with Gemini.

**`workflow.py`** — Two-agent workflow with typed Pydantic state. A data collector agent (react strategy) feeds into a report writer agent (critic strategy). Shows `@workflow.state`, `@workflow.step`, and immutable state updates.

### Switching models

```python
# In agent.py or workflow.py, change the model parameter:
agent = Agent(
    name="researcher",
    model="gemini-1.5-pro",      # or gemini-2.0-flash, gemini-1.5-flash
    ...
)
```

### Switching strategies

```python
agent = Agent(
    name="researcher",
    model="gemini-2.0-flash",
    strategy="plan-and-execute",  # structured multi-step reasoning
    # strategy="react",          # observe-reason-act loop (default for exploration)
    # strategy="critic",         # draft-evaluate-refine loop (best for quality output)
    ...
)
```

## Java

### Prerequisites

- Java 21+
- Maven 3.8+

### Setup

```bash
cd examples/vertex-ai-agents/java

# Set Gemini credentials
export OPENAI_BASE_URL="https://generativelanguage.googleapis.com/v1beta/openai/"
export OPENAI_API_KEY="your-gemini-api-key"

# Build
mvn compile
```

### Run

```bash
mvn exec:java -Dexec.mainClass=dev.jamjet.examples.VertexAiAgent
```

Custom prompt:

```bash
mvn exec:java -Dexec.mainClass=dev.jamjet.examples.VertexAiAgent \
  -Dexec.args="Analyze GOOG stock and competitive position"
```

### What the example shows

**`VertexAiAgent.java`** — Two agents in one file:

1. **React agent** — Research analyst that searches, checks stock data, and saves notes using the observe-reason-act loop
2. **Plan-and-execute agent** — Structured analyst that creates a plan first, then executes each step

Tools are defined as Java records implementing `ToolCall<T>`:

```java
@Tool(description = "Search documents for information")
record SearchDocuments(String query) implements ToolCall<String> {
    public String execute() {
        return "Results for: " + query;
    }
}
```

### Switching models

```java
var agent = Agent.builder("researcher")
        .model("gemini-1.5-pro")     // or gemini-2.0-flash, gemini-1.5-flash
        ...
        .build();
```

## Available Gemini models

| Model | Best for | Speed |
|-------|----------|-------|
| `gemini-2.0-flash` | General tasks, tool use | Fast |
| `gemini-1.5-flash` | Simple tasks, high throughput | Fastest |
| `gemini-1.5-pro` | Complex reasoning, long context | Moderate |

## Using Vertex AI (GCP managed)

For production workloads on Google Cloud, use Vertex AI instead of the public Gemini API:

```bash
# Authenticate with GCP
gcloud auth application-default login

# Set Vertex AI endpoint
export OPENAI_BASE_URL="https://${LOCATION}-aiplatform.googleapis.com/v1beta1/projects/${PROJECT_ID}/locations/${LOCATION}/endpoints/openapi"
export OPENAI_API_KEY="$(gcloud auth application-default print-access-token)"
```

The code stays identical — only the endpoint and auth change.

## File structure

```
vertex-ai-agents/
├── README.md
├── python/
│   ├── requirements.txt
│   ├── agent.py          ← Single agent with react strategy
│   └── workflow.py       ← Two-agent workflow with typed state
└── java/
    ├── pom.xml
    └── src/main/java/dev/jamjet/examples/
        └── VertexAiAgent.java  ← React + plan-and-execute agents
```
