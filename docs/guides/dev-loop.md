# Five-Minute Dev Loop

Get a new agent running locally and scoring its tool trajectory in about five minutes.

---

## Prerequisites

- Python 3.11+
- `pip install 'jamjet[sidecar]'` -- the sidecar extra brings in uvicorn, which `jamjet dev` needs by default
- A model provider key (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, or similar) for live model calls
- The `jamjet-server` binary -- `jamjet dev` downloads it on first run

---

## 1. Scaffold a project

```bash
jamjet create myagent
cd myagent
```

Creates `myagent/` with a minimal, runnable agent:

```
myagent/
  agent.py       # Agent + one @tool function; calls agent.run("Hello")
  pyproject.toml # declares jamjet as a dependency
  README.md
```

To use a different template or list all available templates:

```bash
jamjet create myagent --template quickstart   # explicit (same as default)
jamjet init --list-templates                  # list every template
```

---

## 2. Start the local dev stack

```bash
jamjet dev
```

This brings up three processes in order:

1. **Model sidecar** (`http://127.0.0.1:4280`) -- the governed model seam that
   fronts the AI provider. Health-gated: the engine does not start until the
   sidecar's `/health` endpoint returns `{"ok": true}`.
2. **Engine** (`http://localhost:7700`) -- the durable, event-sourced runtime,
   started with `JAMJET_MODEL_SEAM_URL` wired to the sidecar so every model call
   flows through the governed seam.
3. **Worker** -- drains the `python_tool` queue so your `@tool` functions execute.

Press **Ctrl+C** to stop the entire stack (graceful group teardown, no orphaned
processes).

### Dev stack flags

| Flag | Effect |
|------|--------|
| `--modules myagent.agent` | Worker pre-imports this module so `@tool` decorators register before polling |
| `--sidecar-port 4281` | Use a different sidecar port |
| `--no-sidecar` | Engine + worker only; providers are called directly (no governed seam) |
| `--no-worker` | Sidecar + engine only; `@tool` functions will not execute |
| `--engine-only` | Just the engine, legacy mode (no sidecar, no worker) |
| `--port 7701` | Engine port (default 7700) |

Example -- pre-import your tools so `@tool` functions register before the worker polls:

```bash
jamjet dev --modules myagent.agent
```

---

## 3. Run the agent

Open a second terminal (the dev stack occupies the first):

```bash
export ANTHROPIC_API_KEY=sk-ant-...
python agent.py
```

The scaffolded `agent.py` runs in-process via `agent.run(...)`. To run on the
durable engine (event log, replay, approvals, trajectory scoring), switch to
`agent.run_durable(...)` -- see [`react-agent-durable/`](../../examples/react-agent-durable/)
for the full durable pattern.

---

## 4. Inspect what happened

```bash
# List recent executions
jamjet inspect <execution-id>

# See the full event timeline (tool calls, node transitions)
jamjet events <execution-id>
```

---

## 5. Score the agent's trajectory

Create a small evalset (JSONL) that asserts which tools the agent should call:

```jsonl
{"id": "smoke", "input": {"query": "Hello"}, "expected_trajectory": {"used_tool": "greet", "max_turns": 3}}
```

Run it:

```bash
jamjet eval run evalset.jsonl --workflow myagent
```

The runner scores each row's output AND its tool trajectory. A row passes only
when both the output scorer and all trajectory assertions pass.

For a full worked example with multiple assertion types, see
[`examples/trajectory-eval/`](../../examples/trajectory-eval/).

### Trajectory-diff (replay-regression gate)

After a model or prompt change, diff the tool sequences between two runs to
catch unexpected behavior:

```bash
# Compare two event-log files
jamjet eval trajectory-diff before.json after.json

# CI: exit 1 if the trajectory changed
jamjet eval trajectory-diff v1.json v2.json --fail-on-change

# Report only, no exit code change
jamjet eval trajectory-diff a.json b.json --no-fail-on-change

# Output as JSON
jamjet eval trajectory-diff a.json b.json --format json
```

---

## Next steps

- [Python SDK guide](python-sdk.md) -- `Agent`, `@tool`, `run_durable`, sessions
- [eval harness example](../../examples/eval-harness/) -- output-only eval with LLM judge + assertions
- [trajectory-eval example](../../examples/trajectory-eval/) -- evalset with `expected_trajectory`
- [react-agent-durable](../../examples/react-agent-durable/) -- durable engine, event log, replay
- [Human-in-the-loop](hitl.md) -- gating agent actions on human approval
