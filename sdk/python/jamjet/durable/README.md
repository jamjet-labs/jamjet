# jamjet.durable

Exactly-once tool execution across agent frameworks.

## What it does

`@durable` caches the result of a function against an idempotency key derived
from `(execution_id, function_qualname, args, kwargs)`. The next call with
the same key returns the cached result without re-executing the function.

This means that if your agent crashes mid-tool-call and restarts:

- **Without `@durable`:** the agent's loop replays from the last user message.
  Side-effecting tools (charge a card, send an email, book a flight) get
  called *again*, causing duplicate charges, duplicate emails, double bookings.
- **With `@durable`:** the cached result is returned on replay. The side
  effect happens exactly once.

## How to use it

```python
from jamjet import durable, durable_run

@durable
def charge_card(amount: float) -> dict:
    return stripe.charges.create(amount=amount)

with durable_run("my-agent-run-id"):
    charge_card(100.0)  # first call: hits Stripe, caches result
    charge_card(100.0)  # second call (within same run): returns cache
```

## Cache backend

By default, results are stored in a SQLite database at
`~/.jamjet/durable/cache.db` (override with the `JAMJET_DURABLE_DIR` env var
or by passing `cache=` to the decorator).

## What gets cached

Any picklable return value: dicts, lists, dataclasses, pydantic models,
primitives, nested combinations of those. Lambdas, file handles, sockets,
and other unpicklable types raise `TypeError` at call time.

## Determinism guarantees

- Same `(execution_id, fn_qualname, args, kwargs)` → same key → cache hit.
- Different `execution_id` → different key → re-execution.
- `kwargs` order does not affect the key (`{a:1, b:2}` and `{b:2, a:1}` are equivalent).
- Pydantic models are dumped via `model_dump(mode="json")` for stable hashing.

## What this doesn't cover

- LLM "thinking" steps that aren't wrapped in `@durable`. If your agent calls
  an LLM and decides to take action B based on the response, then crashes
  before action B, on replay the LLM may decide on action C. JamJet's native
  runtime checkpoints LLM responses too; the framework shims do not.
- Tools whose underlying side effects don't have idempotency (e.g., a payment
  API that ignores duplicate idempotency keys). Use `@durable` only on tools
  whose external systems honor at-least-once semantics.
