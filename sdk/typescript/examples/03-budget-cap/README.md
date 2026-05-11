# 03 — Budget cap

Show JamJet stopping a runaway agent loop at a hard dollar cap.

## Run

```bash
pnpm install
pnpm start
```

## Expected output

```
Step 1: search.web $0.02  ALLOWED
Step 2: search.web $0.02  ALLOWED
Step 3: search.web $0.02  BUDGET_EXCEEDED
Spent: $0.04 of $0.05 cap
Decision: BUDGET_EXCEEDED
The model is mocked. The enforcement path is real.
```

## What this proves

A three-step `search.web` loop runs against a `BudgetManager` capped at $0.05.
The third call would push spend over the cap, so it throws
`JamjetBudgetExceeded` before the cost is recorded. No API key, no network —
this exercises the budget enforcement path directly.

In a live `init()`-backed app the same `BudgetManager` is wired to the
auto-patcher; every OpenAI / Anthropic call's cost is recorded automatically
and the budget short-circuits before the model is hit.
