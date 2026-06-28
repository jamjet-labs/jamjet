"""Run the multi-agent team example.

Two patterns over the same two specialists:

1. A coordinator ``Team``: a router picks ONE specialist (researcher or writer)
   to handle each request.
2. A ``Sequential`` pipeline: the researcher's output feeds the writer.

Each sub-agent runs as its own execution (Path A). This file uses the in-process
``.run()`` path, which needs only a model provider key (for example
``ANTHROPIC_API_KEY``). For the durable path, swap ``.run(...)`` for
``.run_durable(...)`` and start the engine + a worker + the model sidecar (see
README.md); the API is identical.

    python main.py
"""

from __future__ import annotations

import asyncio

from specialists import build_desk, build_pipeline


async def main() -> None:
    # ── Pattern 1: coordinator routing ────────────────────────────────────────
    desk = build_desk()

    # A timeless explainer (not a freshness query): the example's web_search tool
    # is a canned stub, so the prompt asks for a definition that does not change
    # rather than implying live, up-to-the-minute data.
    fact_task = "Explain what a durable agent runtime is and why it matters."
    routed = await desk.run(fact_task)
    print(f"[desk] {fact_task}")
    print(f"  routed to: {', '.join(k for k in routed.per_agent if k != 'router')}")
    print(f"  answer:    {routed.output}\n")

    write_task = "Write a one-line teaser for our new agent runtime."
    routed2 = await desk.run(write_task)
    print(f"[desk] {write_task}")
    print(f"  routed to: {', '.join(k for k in routed2.per_agent if k != 'router')}")
    print(f"  answer:    {routed2.output}\n")

    # ── Pattern 2: sequential pipeline ────────────────────────────────────────
    pipeline = build_pipeline()
    result = await pipeline.run("agent runtimes")
    print("[pipeline] research-then-write")
    print(f"  steps:  {' -> '.join(result.per_agent)}")
    print(f"  output: {result.output}")


if __name__ == "__main__":
    asyncio.run(main())
