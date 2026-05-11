"""End-to-end durable @DurableAgent — execute, then resume via execution_id short-circuits."""

import pytest

from jamjet import resume, run
from jamjet.decorators import DurableAgent, task
from jamjet.spec import DurabilityConfig, MemoryConfig

# Agent classes must be at module level so the executor can resolve them via
# importlib.import_module + getattr (class_ref = "<module>:<classname>").


@DurableAgent(
    memory=MemoryConfig(backend="none"),
    durability=DurabilityConfig(checkpoint_every_step=True),
)
class _Pipeline:
    @task(entry=True)
    async def run(self, n: int) -> int:
        return n * 2


@DurableAgent(
    memory=MemoryConfig(backend="none"),
    durability=DurabilityConfig(checkpoint_every_step=True),
)
class _Boomer:
    @task(entry=True)
    async def run(self, _: int) -> int:
        raise RuntimeError("boom")


_counter_call_count: dict[str, int] = {"n": 0}


@DurableAgent(
    memory=MemoryConfig(backend="none"),
    durability=DurabilityConfig(checkpoint_every_step=True),
)
class _Counter:
    @task(entry=True)
    async def run(self, x: int) -> int:
        _counter_call_count["n"] += 1
        return x + 100


def _spec_with_db(base_spec, db_path):
    """Return a copy of the spec with db_path set to the given tmp path."""
    return base_spec.model_copy(
        update={"durability": DurabilityConfig(db_path=str(db_path), checkpoint_every_step=True)}
    )


@pytest.mark.asyncio
async def test_run_then_resume_returns_same_output(tmp_path):
    db_path = tmp_path / "ckpt.db"
    spec = _spec_with_db(_Pipeline.__jamjet_spec__, db_path)

    out1 = await run(spec, 21, execution_id="recovery-test-1")
    assert out1.output == 42
    assert db_path.exists()

    out2 = await resume(spec, "recovery-test-1")
    assert out2.output == 42


@pytest.mark.asyncio
async def test_failed_step_recorded(tmp_path):
    """When the entry method raises, the step status is recorded as 'failed'."""
    db_path = tmp_path / "fail.db"
    spec = _spec_with_db(_Boomer.__jamjet_spec__, db_path)

    with pytest.raises(RuntimeError, match="boom"):
        await run(spec, 1, execution_id="fail-test-1")

    # The DB should exist with the step recorded as failed
    from jamjet.runtime.local.checkpoint import CheckpointStore

    store = CheckpointStore(db_path, ir_version="1.0", spec_hash="todo")
    incomplete = await store.list_incomplete_steps()
    assert len(incomplete) == 1
    assert incomplete[0].status == "failed"
    assert incomplete[0].error == "boom"


@pytest.mark.asyncio
async def test_resume_picks_up_existing_state(tmp_path):
    """If a fresh run completes, resume returns the cached output without re-executing."""
    db_path = tmp_path / "cache.db"
    spec = _spec_with_db(_Counter.__jamjet_spec__, db_path)

    _counter_call_count["n"] = 0

    eid = "cache-test-1"
    out1 = await run(spec, 5, execution_id=eid)
    assert out1.output == 105
    assert _counter_call_count["n"] == 1

    # Resume: should NOT re-call the method
    out2 = await resume(spec, eid)
    assert out2.output == 105
    assert _counter_call_count["n"] == 1, "resume should short-circuit via checkpoint, not re-run"
