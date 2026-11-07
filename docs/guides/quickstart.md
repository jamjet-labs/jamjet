# Quickstart

Get a JamJet workflow running locally in under 10 minutes.

---

## Prerequisites

- Python 3.11+
- An Anthropic API key (the starter workflow calls `claude-haiku-4-5`)

---

## 1. Install the CLI

```bash
pip install 'jamjet[model]'
```

Verify:
```bash
jamjet --version
```

---

## 2. Create a project

```bash
jamjet init my-first-agent
cd my-first-agent
```

This uses the `hello-agent` template and creates:
```
my-first-agent/
  workflow.yaml         # one model node that answers state.query
  evals/dataset.jsonl   # sample inputs for `jamjet eval run`
```

`jamjet init --list-templates` shows the other templates.

---

## 3. Start local dev mode

In the first terminal:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
jamjet dev
```

This starts the local stack: a model sidecar, the engine (SQLite-backed, on
port 7700) and a Python tool worker. It prints the binary, the engine and
sidecar URLs and `Mode: local (SQLite)`, and runs until you press Ctrl+C.

---

## 4. Validate your workflow

In a second terminal:

```bash
jamjet validate workflow.yaml
```

```
Valid. workflow_id=my-first-agent version=0.1.0
  Nodes: 1  Edges: 1
```

---

## 5. Run it

The template reads the question from `query`:

```bash
jamjet run workflow.yaml --input '{"query": "What is the capital of France?"}'
```

`jamjet run` registers the workflow, starts an execution, prints its status
every second until it finishes, and ends with a per-step timeline:

```
Registered my-first-agent v0.1.0
Execution started: exec_...
  Status: running
  ...
```

---

## 6. Inspect the execution

```bash
jamjet inspect exec_...
```

```bash
# See the full event timeline
jamjet events exec_...
```

---

## Next Steps

- [Core Concepts](concepts.md) — understand agents, workflows, nodes, and state
- [Workflow Authoring](workflow-authoring.md) — full authoring guide
- [MCP Integration](mcp-guide.md) — connect to an external MCP tool server
- [Python SDK](python-sdk.md) — define workflows in Python
