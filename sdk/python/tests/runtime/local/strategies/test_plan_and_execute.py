from copy import deepcopy
from dataclasses import dataclass

import pytest

from jamjet.runtime.local.strategies import get_strategy_runner
from jamjet.spec import AgentSpec, LLMConfig


@dataclass
class Message:
    content: str
    tool_calls: list | None = None


class RecordingAdapter:
    def __init__(self):
        self.config = LLMConfig(provider="openai", model="offline")
        self.calls = []
        self.responses = iter(
            [
                "1. Find the invoice ID\n2. Look up its total\n3. Write a report",
                "Invoice ID: INV-7429",
                "Invoice total: USD 318.25",
                "Report prepared.",
                "Final report.",
            ]
        )

    async def generate(self, messages, *, tools=None):
        self.calls.append(deepcopy(messages))
        return Message(next(self.responses))


@pytest.mark.parametrize("with_history", [False, True])
async def test_each_plan_step_receives_completed_step_results(with_history):
    adapter = RecordingAdapter()
    spec = AgentSpec(name="invoice-assistant", llm=adapter.config)
    history = (
        [
            {"role": "system", "content": "Use the billing system."},
            {"role": "user", "content": "Work with the current account."},
            {"role": "assistant", "content": "Account selected."},
            {"role": "user", "content": "Prepare an invoice report."},
        ]
        if with_history
        else None
    )
    original_history = deepcopy(history)

    result = await get_strategy_runner("plan-and-execute")(
        adapter=adapter,
        spec=spec,
        prompt="Prepare an invoice report.",
        tools=[],
        tool_calls_log=[],
        initial_messages=history,
    )

    assert result == "Final report."
    assert len(adapter.calls) == 5
    assert history == original_history

    first_step = str(adapter.calls[1])
    second_step = str(adapter.calls[2])
    third_step = str(adapter.calls[3])
    synthesis = str(adapter.calls[4])
    assert "INV-7429" not in first_step
    assert "INV-7429" in second_step
    assert "318.25" not in second_step
    assert "INV-7429" in third_step
    assert "318.25" in third_step
    assert "Report prepared." not in third_step
    assert all(text in synthesis for text in ["INV-7429", "318.25", "Report prepared."])
    if with_history:
        assert all("Account selected." in str(call) for call in adapter.calls)
