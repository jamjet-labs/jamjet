# Observability & Debugging

JamJet provides full execution traces, replay from any checkpoint, and structured event timelines.

---

## CLI Commands

```bash
# Full execution state and current node
jamjet inspect exec_01abc

# Event timeline (all events in order)
jamjet events exec_01abc

# Filter by event type
jamjet events exec_01abc --type node_completed

# Replay from the beginning (uses recorded model/tool outputs)
jamjet replay exec_01abc

# Replay from a specific node
jamjet replay exec_01abc --from-node summarize

# Worker status
jamjet workers list
```

---

## Execution Timeline

`jamjet events exec_01abc` output:

```
exec_01abc  customer_support_triage
──────────────────────────────────────────────────
00:00.000   workflow_started
00:00.012   node_scheduled      fetch_ticket
00:00.015   node_started        fetch_ticket         worker=w1
00:01.204   node_completed      fetch_ticket         1.2s
00:01.205   node_scheduled      classify
00:01.208   node_started        classify             worker=w1
00:03.311   node_completed      classify             2.1s   priority=high
00:03.312   node_scheduled      human_review
00:03.315   interrupt_raised    human_review         waiting for approval
...
04:22.001   approval_received   human_review         user=alice  decision=approved
04:22.003   node_completed      human_review
04:22.004   node_scheduled      auto_respond
...
```

---

## OpenTelemetry

JamJet exports traces, metrics, and logs via OpenTelemetry.

```yaml
# jamjet.yaml
observability:
  otel:
    enabled: true
    endpoint: http://localhost:4318  # OTLP HTTP
    service_name: jamjet-runtime
```

Spans emitted:
- `jamjet.workflow` — full workflow trace
- `jamjet.node` — per-node span with input/output metadata
- `jamjet.model_call` — LLM call span (tokens, latency, model)
- `jamjet.tool_call` — tool invocation span
- `jamjet.mcp_call` — MCP tool round-trip
- `jamjet.a2a_task` — A2A delegation span

---

## Replay

Replay re-executes a workflow using **recorded outputs** for all model and tool calls. It is:
- **Instant** — no real LLM or tool calls made
- **Deterministic** — same outputs, same path through the graph
- **Safe** — side-effect nodes are skipped by default in replay mode

```bash
jamjet replay exec_01abc --from-node classify
```

Useful for:
- Debugging a failed execution by replaying just the failing section
- Validating a bug fix without real model calls
- Testing routing logic changes against historical data

---

## Cost and Token Tracking

JamJet records token usage and cost (when available from the model provider) per execution:

```bash
jamjet inspect exec_01abc --costs
```

```
exec_01abc costs:
  classify:   1,204 tokens  $0.002
  summarize:  3,891 tokens  $0.006
  ─────────────────────────────────
  Total:      5,095 tokens  $0.008
```
