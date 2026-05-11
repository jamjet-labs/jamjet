# 01 — Block an unsafe tool call

Show JamJet's policy enforcement blocking a destructive tool call before execution.

## Run

```bash
pnpm install
pnpm start
```

## Expected output

```text
Tool: database.delete_all_customers
Policy: block '*delete*'
Decision: BLOCKED
Executed: false
The model is mocked. The enforcement path is real.
```

## What this proves

The agent (mocked) plans `database.delete_all_customers`. The runtime policy
matches `*delete*` and blocks the call. Nothing executes. No API key, no
network — this exercises the enforcement path directly.

## Same enforcement, different surfaces

The same `PolicyEvaluator` runs inside `@jamjet/cloud` auto-instrumentation —
when `init()` is configured, every OpenAI / Anthropic call in your process is
filtered by this policy.
