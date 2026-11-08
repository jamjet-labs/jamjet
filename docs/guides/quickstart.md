# Quickstart

Get a JamJet workflow running locally in under 10 minutes.

---

## Prerequisites

- Python 3.11+
- `ANTHROPIC_API_KEY` set in the shell that runs `jamjet dev` (the starter
  workflow calls a Claude model)

---

## 1. Install the CLI

```bash
pip install jamjet
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

This creates the `hello-agent` starter:
```
my-first-agent/
  workflow.yaml         # one model node
  evals/dataset.jsonl   # sample inputs for `jamjet eval`
```

The workflow has one `model` node whose prompt reads the run's input:

```yaml
nodes:
  think:
    type: model
    model: claude-haiku-4-5-20251001
    prompt: |
      Answer this question clearly and concisely:

      {{ state.query }}
    next: end
```

The runtime fills in `{{ state.query }}` from the run's state before it calls
the model. A prompt can only substitute `{{ state.<path> }}`; see
[Prompt templates](yaml-reference.md#prompt-templates).

---

## 3. Start local dev mode

```bash
jamjet dev
```

This starts the local runtime (SQLite-backed) in the background. You'll see:

```
JamJet runtime started
  Mode: local (SQLite)
  API: http://localhost:7700
  Ready.
```

---

## 4. Validate your workflow

```bash
jamjet validate workflow.yaml
```

---

## 5. Run it

```bash
jamjet run workflow.yaml --input '{"query": "What is the capital of France?"}'
```

The CLI registers the workflow, starts a run and follows it until it ends.
The model's reply is in the run's final state as `last_model_output`.

The input must have every key the prompt reads. Run it with
`{"question": ...}` instead and the `think` node fails with
``the run's state has no `query` ``: the runtime does not send the model a
prompt with a gap in it.

---

## 6. Inspect the execution

```bash
jamjet inspect exec_01abc
```

```bash
# See the full event timeline
jamjet events exec_01abc
```

---

## Next Steps

- [Core Concepts](concepts.md) — understand agents, workflows, nodes, and state
- [Workflow Authoring](workflow-authoring.md) — full authoring guide
- [MCP Integration](mcp-guide.md) — connect to an external MCP tool server
- [Python SDK](python-sdk.md) — define workflows in Python
