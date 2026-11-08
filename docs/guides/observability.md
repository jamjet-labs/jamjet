# Observability & Debugging

The runtime records every run as an ordered event log. The CLI reads it back,
and the server can export traces and metrics over OpenTelemetry.

---

## CLI commands

```bash
# Run state, and a per-node token breakdown
jamjet inspect exec_01abc

# Event timeline, in order
jamjet events exec_01abc

# One node's events, or only MCP or A2A events
jamjet events exec_01abc --node review
jamjet events exec_01abc --protocol mcp

# Start a new run of the same workflow with the same input
jamjet replay exec_01abc
```

Every command takes `--runtime <url>` (default `http://localhost:7700`) and
reads a token from `JAMJET_TOKEN` outside dev mode. `jamjet workers` calls
`GET /workers`, which lists each worker that holds a live lease with the items
it holds. The runtime has no worker registry, so an idle worker is not listed.

---

## Execution timeline

`jamjet events` prints one row per event. This run had a model step, a
`human_approval` step and a second model step:

```
┃ Seq ┃ Type                   ┃ Node   ┃ Created at                  ┃
│ 1   │ workflow_started       │ —      │ 2026-11-08T04:49:32.985567Z │
│ 2   │ node_scheduled         │ draft  │ 2026-11-08T04:49:32.985640Z │
│ 3   │ node_started           │ draft  │ 2026-11-08T04:49:58.680232Z │
│ 4   │ node_completed         │ draft  │ 2026-11-08T04:49:58.690766Z │
│ 5   │ node_scheduled         │ review │ 2026-11-08T04:49:59.181083Z │
│ 6   │ node_started           │ review │ 2026-11-08T04:49:59.690663Z │
│ 7   │ tool_approval_required │ review │ 2026-11-08T04:49:59.697179Z │
│ 8   │ approval_received      │ review │ 2026-11-08T04:50:46.360187Z │
│ 9   │ node_scheduled         │ review │ 2026-11-08T04:50:46.670902Z │
│ 10  │ node_started           │ review │ 2026-11-08T04:50:46.675640Z │
│ 11  │ node_completed         │ review │ 2026-11-08T04:50:46.679344Z │
│ 12  │ node_scheduled         │ send   │ 2026-11-08T04:50:47.174579Z │
│ 13  │ node_started           │ send   │ 2026-11-08T04:50:47.201141Z │
│ 14  │ node_completed         │ send   │ 2026-11-08T04:50:47.206485Z │
│ 15  │ workflow_completed     │ —      │ 2026-11-08T04:50:47.677390Z │
```

The table shows types only. The full events, with errors, policy decisions and
approval details, come from `GET /executions/:id/events`. It returns at most
10000 events per call; when a page is full, `next_after` in the response is the
`after` to pass for the next page (`?after=<sequence>&limit=<n>`), and `null`
otherwise. A node output larger than 256 KiB stays a
`{"$artifact": {"hash", "size", ...}}` reference, fetched with
`GET /artifacts/:hash`, and so do the rest of a page's outputs once it has
inlined 16 MiB. `?inline_artifacts=all` lifts the 256 KiB limit (the page limit
is then 64 MiB) and `none` inlines nothing. The CLI and the Python client follow
the pages for you. `GET /executions` returns at most 200 runs per call
(`limit`, `offset`). `GET /workers` lists at most 1000 held items.

---

## Replay

`jamjet replay <id>` starts a new execution of the same workflow version with
the original input; `--override-input '<json>'` merges fields into it. The new
run executes every step again, with real model and tool calls. It does not
reuse the recorded outputs. `--from-node` prints a note and starts from the
beginning. `jamjet fork <id> --override-input '<json>'` does the same with
changed input.

What the runtime does reuse is its own event log: after a crash or restart a
run continues where it stopped, and steps that already completed do not run
again.

---

## OpenTelemetry

Set `OTEL_EXPORTER_OTLP_ENDPOINT` on the server to export traces and metrics
over OTLP/gRPC (for example `http://localhost:4317`).

- Spans: one `jamjet.node` span per step the server's in-process workers run
  (`model`, `condition`, `human_approval`, `eval`, `limit_exceeded`), with
  `jamjet.execution.id`, `jamjet.node.id`, `jamjet.node.kind`,
  `jamjet.attempt`, `jamjet.worker.id`, `jamjet.workflow.id` and
  `jamjet.workflow.version`. Model steps add the OpenTelemetry GenAI
  attributes (`gen_ai.system`, `gen_ai.request.model`,
  `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens` and others).
- Metrics: the `gen_ai.client.token.usage` histogram, per model call.

Python and Java steps run in their own worker processes, and the server emits
no spans for them. Prompts and completions are not attached to spans.

---

## Cost and token tracking

`jamjet inspect` prints a per-node table of tokens, finish reason, cost and
duration (hide it with `--no-tokens`). Cost shows when the model's reply
carries one:

```
┃ Node  ┃ System    ┃ Model      ┃ Input Tokens ┃ Output Tokens ┃ Finish Reason ┃ Cost (USD) ┃ Duration (ms) ┃
│ draft │ anthropic │ claude-ha… │           10 │             8 │ stop          │          — │            10 │
│ send  │ anthropic │ claude-ha… │           10 │             8 │ stop          │          — │             5 │
  Totals: input=20 tokens, output=16 tokens
```
