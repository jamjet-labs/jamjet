"""Performance baseline: 100 sequential @DurableAgent runs, end to end.

This is an ORDER-OF-MAGNITUDE gate, not a benchmark. The budget is set well
above the observed time so it catches a real regression — an accidental O(n^2),
a per-run fsync, a lost cache — and not the ordinary variance of a shared CI
runner.

The previous 4.0s budget was tuned to an M2 laptop and sat close enough to the
CI runner's actual time to fail on noise: it tripped at 4.11s and 4.25s on
consecutive days, including on `main` with no relevant change. A perf gate that
cries wolf gets ignored or re-run until green, which is strictly worse than a
loose gate that only fires on something real.

Keep the budget generous. If a change makes this test fail, the change is very
probably at fault.
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


# Roughly 3x the time observed on CI, which is itself ~2x an M2 laptop. Large
# enough to absorb a slow shared runner, small enough that a 10x regression in
# per-run overhead still trips it.
BUDGET_S = 12.0


@pytest.mark.asyncio
async def test_100_sequential_runs_within_budget(tmp_path):
    """Crude end-to-end perf gate. Catches order-of-magnitude regressions."""
    t0 = time.perf_counter()
    for i in range(100):
        await run(_Bench, 1, execution_id=f"bench-{tmp_path.name}-{i}")
    elapsed = time.perf_counter() - t0
    assert elapsed < BUDGET_S, (
        f"100 runs took {elapsed:.2f}s; budget is {BUDGET_S}s. This gate is set "
        f"well above the normal time, so a failure here means a real regression "
        f"in run() overhead, not runner noise."
    )
