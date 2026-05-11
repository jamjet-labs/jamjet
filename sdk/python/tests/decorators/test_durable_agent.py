import pytest

from jamjet.decorators import DurableAgent, task
from jamjet.spec import DurableAgentSpec, MemoryConfig


def test_bare_form_uses_defaults():
    @DurableAgent
    class A:
        async def run(self, q: str) -> str:
            return q

    spec = A.__jamjet_spec__
    assert isinstance(spec, DurableAgentSpec)
    assert spec.kind == "durable_agent"
    assert spec.class_ref.endswith(":A")
    assert spec.memory is not None  # default MemoryConfig() applied


def test_parameterized_form():
    @DurableAgent(model="gpt-4o", instructions="hi")
    class A:
        @task(entry=True)
        async def plan(self, q: str) -> str:
            return q

    spec = A.__jamjet_spec__
    assert spec.llm.model == "gpt-4o"
    assert spec.instructions == "hi"
    assert any(m.is_entrypoint for m in spec.methods)


def test_stateless_shortcut():
    @DurableAgent(stateless=True)
    class A:
        async def run(self, q: str) -> str:
            return q

    spec = A.__jamjet_spec__
    assert spec.memory is None
    assert spec.durability.checkpoint_every_step is False


def test_stateless_conflicts_with_explicit_memory():
    with pytest.raises(ValueError, match="stateless"):

        @DurableAgent(stateless=True, memory=MemoryConfig())
        class A:
            async def run(self, q: str) -> str:
                return q


def test_entrypoint_inference_named_run():
    @DurableAgent
    class A:
        async def helper(self, x):
            return x

        async def run(self, q):
            return q

    spec = A.__jamjet_spec__
    entries = [m for m in spec.methods if m.is_entrypoint]
    assert len(entries) == 1
    assert entries[0].name == "run"


def test_explicit_entry_wins_over_run():
    @DurableAgent
    class A:
        @task(entry=True)
        async def go(self, q):
            return q

        async def run(self, q):
            return q

    spec = A.__jamjet_spec__
    entries = [m for m in spec.methods if m.is_entrypoint]
    assert len(entries) == 1
    assert entries[0].name == "go"


def test_class_returned_unmodified_attribute_access_outside_runtime_raises():
    @DurableAgent
    class A:
        async def run(self, q: str) -> str:
            return q

    a = A()
    with pytest.raises(RuntimeError, match="not running inside a JamJet runtime"):
        _ = a.memory  # noqa: F841

    with pytest.raises(RuntimeError, match="not running inside a JamJet runtime"):
        _ = a.llm  # noqa: F841


def test_runtime_can_inject_attributes():
    """After runtime injection (via instance __dict__), attribute access works."""

    @DurableAgent
    class A:
        async def run(self, q: str) -> str:
            return q

    a = A()
    a.memory = "fake-memory"
    a.llm = "fake-llm"
    assert a.memory == "fake-memory"
    assert a.llm == "fake-llm"
