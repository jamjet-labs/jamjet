"""Performance baseline: 100 sequential @DurableAgent runs in under 4s on M2.

Stricter <2s budget needs intra-method @task checkpointing which is a
follow-up; for now we measure run() invocation overhead end-to-end.
"""
import time

import pytest

from jamjet import DurableAgent, run
from jamjet.decorators import task


@DurableAgent(memory=None)
class _Bench:
    @task(entry=True)
    async def run(self, x: int) -> int:
        return x + 1


@pytest.mark.asyncio
async def test_100_sequential_runs_under_4s(tmp_path):
    """Crude end-to-end perf gate. Catches order-of-magnitude regressions."""
    t0 = time.perf_counter()
    for i in range(100):
        await run(_Bench, 1, execution_id=f"bench-{tmp_path.name}-{i}")
    elapsed = time.perf_counter() - t0
    assert elapsed < 4.0, f"100 runs took {elapsed:.2f}s; budget is 4.0s"
