import pytest

from jamjet.decorators import DurableAgent
from jamjet.runtime.local.injector import inject_runtime_attributes
from jamjet.spec import MemoryConfig


@pytest.mark.asyncio
async def test_injector_wires_memory(tmp_path):
    @DurableAgent(memory=MemoryConfig(db_path=str(tmp_path / "engram.db")))
    class A:
        async def run(self, q: str) -> str:
            return q

    instance = A()
    await inject_runtime_attributes(
        instance,
        spec=A.__jamjet_spec__,
        execution_id="ex1",
    )
    assert instance.memory is not None
    fact = await instance.memory.record("test")
    assert fact.text == "test"
    # Cleanup
    engram = getattr(instance, "_jamjet_engram", None)
    if engram is not None:
        await engram.close()


@pytest.mark.asyncio
async def test_injector_uses_no_memory_when_disabled():
    @DurableAgent(stateless=True)
    class A:
        async def run(self, q: str) -> str:
            return q

    instance = A()
    await inject_runtime_attributes(
        instance,
        spec=A.__jamjet_spec__,
        execution_id="ex1",
    )
    from jamjet.memory import NoMemory

    assert isinstance(instance.memory, NoMemory)


@pytest.mark.asyncio
async def test_injector_sets_seeded_helpers():
    @DurableAgent(stateless=True)
    class A:
        async def run(self, q: str) -> str:
            return q

    instance = A()
    await inject_runtime_attributes(
        instance,
        spec=A.__jamjet_spec__,
        execution_id="ex-seed-test",
    )
    assert instance.workflow_id == "ex-seed-test"
    # Seeded random produces deterministic output for same execution_id
    a = instance.random.random()
    instance2 = A()
    await inject_runtime_attributes(
        instance2,
        spec=A.__jamjet_spec__,
        execution_id="ex-seed-test",
    )
    b = instance2.random.random()
    assert a == b
