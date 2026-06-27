# react-agent-durable

A small ReAct-style agent (model + two `@tool` functions + instructions) that runs
on the durable, event-sourced JamJet engine via `Agent.run_durable(prompt)`.

Authoring the agent is the same as the in-process path. Calling `run_durable`
compiles it to an **agent-loop IR** (`model -> tools -> model`, statically
unrolled and bounded by `max_turns`) and drives a durable execution, so the run
gets the event log, replay, idempotency, park-on-429, and artifacts for free. The
model call still goes through the governed model seam, and the `@tool` functions
run on a separate `jamjet worker` process.

## Files

- `weather_agent.py` - the `@tool` functions (`get_weather`, `add`) and the
  `build_agent()` factory. Imported by both the runner and the worker.
- `main.py` - calls `await agent.run_durable(prompt)` and prints the answer.

## Why the tools live in their own module

The compiled IR records each tool as `{name: "weather_agent:<fn>"}` (module +
qualname). The `jamjet worker` resolves that reference by **importing
`weather_agent`**, so the tool functions must live in an importable module, not in
`__main__`. That is why `main.py` does `from weather_agent import build_agent`
instead of defining the tools inline.

## Prerequisites

- The `jamjet` Python package installed (`pip install jamjet`, or use the dev repo
  with `uv run` from `sdk/python/`).
- A model provider key for the sidecar, for example `ANTHROPIC_API_KEY`.
- The engine binary: `jamjet dev` downloads it on first run, or build it locally
  with `jamjet dev --build`.

## Run it (four terminals)

The engine routes every model call to the sidecar when `JAMJET_MODEL_SEAM_URL` is
set, and it probes the sidecar's `/health` at startup, so **start the sidecar
first**.

```bash
# Terminal 1 - model sidecar (the governed seam that calls the provider)
export ANTHROPIC_API_KEY=sk-ant-...
uvicorn jamjet.model.sidecar_server:app --host 127.0.0.1 --port 4280

# Terminal 2 - the durable engine on http://localhost:7700, routed to the sidecar
export JAMJET_MODEL_SEAM_URL=http://127.0.0.1:4280
jamjet dev                 # first run: `jamjet dev --build`

# Terminal 3 - the Python tool worker (drains the python_tool queue)
cd examples/react-agent-durable
PYTHONPATH=. jamjet worker \
  --runtime http://localhost:7700 \
  --queue python_tool \
  --modules weather_agent

# Terminal 4 - run the agent
cd examples/react-agent-durable
python main.py
```

`PYTHONPATH=.` puts this directory on the worker's import path so it can import
`weather_agent`; run the worker from this directory.

Expected output (Terminal 4): the model's final answer, followed by the tool calls
it made durably, for example:

```text
answer: It is sunny in Paris, and 19 + 23 = 42.

  tool get_weather({'city': 'Paris'}) -> sunny, 24C
  tool add({'a': 19, 'b': 23}) -> 42
```

In Terminal 3 you will see each `python_tool` work item claimed and completed; the
engine's event log records the `model` node's `tool_calls`, then the tool-dispatch
`NodeCompleted`, then the next `model` node.

## Without the sidecar

If you do not set `JAMJET_MODEL_SEAM_URL`, the engine calls the provider directly
through its native adapters (it still needs `ANTHROPIC_API_KEY` in the engine's
environment). The sidecar path is preferred because it runs the governed seam
middleware (allowlist, PII, metering) around the model call.

## In-process comparison (no services)

The same agent runs in process without any of the above:

```python
import asyncio
from weather_agent import build_agent

print(asyncio.run(build_agent().run("What's the weather in Paris?")).output)
```

`Agent.run()` (a pure in-process loop) and `Agent.run_durable()` (the engine)
produce the same answer shape. The SDK proves this with a parity test
(`tests/test_agent_durable_loop_parity.py`): it drives the **real** compiled IR
and the **real** tool-dispatch helper through the engine's control flow with a
deterministic mock model, and asserts the `model -> tool -> model` loop runs, the
tool is invoked, and the final answer matches the in-process run.

## Limitations (v1)

- **Bounded `max_turns`, no early exit yet.** `max_turns` bounds the static
  unroll (that many tool turns, then a final answer-only model turn) and the loop
  runs it to completion. The per-turn gate that would short-circuit as soon as the
  model returns a final answer is not yet wired into the engine
  (F-2j-dynamic-loop), so size `max_turns` to the conversation depth you expect.
  The extracted answer is that final model turn's output, which consumes the last
  tool results.
- **The model is re-invoked every remaining turn (cost).** Because there is no
  early exit, a real model is called once per turn for all `max_turns`, so cost
  scales with `max_turns` and the answer can drift across turns.
- **Tool-message fidelity is lossy across turns.** Tool results are threaded
  back to the model as conversation text; the Rust `ChatMessage` carries only
  `role` + `content`, so full assistant/tool message-role fidelity through the
  engine is a follow-up (F-2j-chatmessage-fidelity).
- **The tool-dispatch node owns the `messages` state-patch, not your `@tool`.**
  In this durable loop the `python_tool` node runs `dispatch_tool_calls` (not your
  `@tool` directly): it executes the model's requested calls, appends each tool's
  return value as a `role: tool` message, and returns `{"messages": [...]}`. That
  return dict is applied as the node's `state_patch` (top-level keys replace, the
  rest merge), which threads the accumulated conversation into the next turn. Your
  `@tool` functions just return a result (string or JSON) that becomes tool-message
  content — they never return the reserved loop keys (`messages`,
  `last_model_output`), which the dispatch node and model node manage
  (F-2j-statepatch-namespace).
