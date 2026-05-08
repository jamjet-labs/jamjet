"""Run twice with the same execution_id. The second call short-circuits via replay."""
import asyncio
import time

from jamjet import DurableAgent, resume, run
from jamjet.spec import DurabilityConfig


@DurableAgent(
    memory=None,
    durability=DurabilityConfig(checkpoint_every_step=True),
    model="gpt-4o",
)
class SlowMath:
    async def run(self, n: int) -> int:
        time.sleep(2)
        return n * 7


async def main() -> None:
    eid = "crash-demo-1"
    t0 = time.time()
    out1 = await run(SlowMath, 6, execution_id=eid)
    print(f"First run: {out1.output} in {time.time() - t0:.1f}s")

    t0 = time.time()
    out2 = await resume(SlowMath.__jamjet_spec__, eid)
    print(f"Resume:    {out2.output} in {time.time() - t0:.3f}s (cached)")


if __name__ == "__main__":
    asyncio.run(main())
