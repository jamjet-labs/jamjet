# 01 — Block an unsafe tool call

Show JamJet's policy enforcement blocking a destructive tool call before execution.

## Run

    python main.py

## Expected output

See `expected-output.txt`.

## What this proves

The agent (mocked) plans `database.delete_all_customers`. The runtime policy
matches `*delete*` and blocks the call. Nothing executes. No model API key
needed — this exercises the enforcement path directly.

## Same enforcement, different surfaces

The same `PolicyEvaluator` runs inside `jamjet.cloud` auto-instrumentation —
when configured, every OpenAI / Anthropic call in your process is filtered by
this policy.
