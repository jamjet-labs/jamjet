import pytest

from jamjet.runtime.local import LocalRuntime
from jamjet.runtime.types import RuntimeResult
from jamjet.spec import AgentSpec, AgentStrategy, LLMConfig


@pytest.mark.asyncio
async def test_local_runtime_executes_agent_spec(monkeypatch):
    """AgentSpec → strategy executor path. Uses fake LLM via monkeypatch."""

    async def fake_runner(*, adapter, spec, prompt, tools, tool_calls_log, initial_messages=None):
        return f"echo: {prompt}"

    monkeypatch.setattr(
        "jamjet.runtime.local.executor.get_strategy_runner",
        lambda name: fake_runner,
    )

    rt = LocalRuntime()
    spec = AgentSpec(
        name="x",
        llm=LLMConfig(provider="openai", model="fake"),
        strategy=AgentStrategy(name="react"),
    )
    result = await rt.execute(spec, input="hello")
    assert isinstance(result, RuntimeResult)
    assert result.output == "echo: hello"
    assert result.execution_id


def test_local_runtime_supported_ir_versions():
    rt = LocalRuntime()
    assert "1.0" in rt.supported_ir_versions
    assert rt.name == "local"
