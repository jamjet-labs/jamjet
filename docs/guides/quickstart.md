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
pip install 'jamjet[model,sidecar]'
```

`model` adds the client the model sidecar calls providers with, and `sidecar`
adds the server it runs on. `jamjet dev` stops with `the model sidecar needs
uvicorn` if `sidecar` is missing.

Verify:
```bash
jamjet --version
```

`jamjet dev` uses the `jamjet-server` on your `PATH`, and downloads the binary
published with the SDK's latest release when there is none. The binary
published with `jamjet` 0.12.0 does not send a model node's prompt to the
model, so the starter workflow below fails there with `no prompt or messages
provided`. To follow this guide with the code on `main`, install from a clone
and put the runtime you build on your `PATH`:

```bash
pip install './sdk/python[model,sidecar]'
mkdir -p web/dist && touch web/dist/index.html   # the binary embeds the web UI from here
(cd runtime && cargo build --bin jamjet-server)
export PATH="$PWD/runtime/target/debug:$PATH"
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

The CLI registers the workflow, starts a run and follows it until it ends.
The model's reply is in the run's final state as `last_model_output`.

The input must have every key the prompt reads. Run it with
`{"question": ...}` instead and the `think` node fails with
``the run's state has no `query` `` (in its `node_failed` event, from
`curl localhost:7700/executions/<id>/events`): the runtime does not send the
model a prompt with a gap in it.

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
