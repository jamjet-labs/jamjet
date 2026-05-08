import pytest

from jamjet import Agent
from jamjet.runtime.types import RuntimeResult


@pytest.mark.asyncio
async def test_agent_run_dispatches_to_local_runtime(monkeypatch):
    seen = {}

    class FakeLocalRuntime:
        name = "local"
        supported_ir_versions = ("1.0",)

        async def execute(self, spec, input, *, execution_id=None, scope=None, on_event=None):
            seen["spec"] = spec
            seen["input"] = input
            return RuntimeResult(
                output="fake-output",
                execution_id="ex1",
                duration_ms=1.0,
                steps=[],
                tool_calls=[],
                llm_calls=[],
            )

        async def resume(self, spec, execution_id):
            raise NotImplementedError

    monkeypatch.setattr("jamjet.agents.agent.LocalRuntime", FakeLocalRuntime)

    a = Agent("x", model="gpt-4o", tools=[], strategy="react")
    result = await a.run("hello")
    assert result.output == "fake-output"
    assert seen["input"] == "hello"
    assert seen["spec"].name == "x"
