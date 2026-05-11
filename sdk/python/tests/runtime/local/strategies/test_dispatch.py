"""Strategy dispatcher uses a FakeLLMAdapter so we don't hit real APIs."""

from dataclasses import dataclass

import pytest

from jamjet.runtime.local.strategies import get_strategy_runner
from jamjet.spec import AgentSpec, AgentStrategy, LLMConfig


@dataclass
class _FakeMsg:
    content: str | None = ""
    tool_calls: list | None = None


class FakeLLMAdapter:
    def __init__(self, responses: list[str]) -> None:
        self.config = LLMConfig(provider="openai", model="fake")
        self._responses = list(responses)

    async def generate(self, messages, *, tools=None):
        if not self._responses:
            return _FakeMsg(content="", tool_calls=None)
        return _FakeMsg(content=self._responses.pop(0), tool_calls=None)


@pytest.mark.parametrize(
    "strategy_name",
    [
        "plan-and-execute",
        "react",
        "critic",
        "reflection",
        "consensus",
        "debate",
    ],
)
async def test_dispatch_returns_str(strategy_name):
    runner = get_strategy_runner(strategy_name)
    adapter = FakeLLMAdapter(["1. step\n2. step", "result", "PASS", "SATISFIED"] * 20)
    spec = AgentSpec(
        name="x",
        llm=LLMConfig(provider="openai", model="fake"),
        strategy=AgentStrategy(name=strategy_name),
    )
    out = await runner(adapter=adapter, spec=spec, prompt="say hi", tools=[], tool_calls_log=[])
    assert isinstance(out, str)


def test_unknown_strategy_raises():
    with pytest.raises(ValueError, match="Unknown strategy"):
        get_strategy_runner("imaginary")
