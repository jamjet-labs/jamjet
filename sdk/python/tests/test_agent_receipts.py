"""AgentBoundary receipts on by default for governed agent turns (T3-4).

A plain governed :class:`Agent` produces a valid, conformant Action Receipt
for every turn without the developer opting in. ``receipts=False`` turns it
off. The receipt is bound to the action (provenance) via ``arguments_hash``.
"""

import pytest
from agentboundary import check_conformance, validate_receipt
from agentboundary.hashing import compute_arguments_hash

from jamjet import Agent
from jamjet.agents.receipts import agent_action_arguments
from jamjet.runtime.types import RuntimeResult

_PROMPT = "summarize the latest trends in agent runtimes"
_OUTPUT = "the answer is 42"


class FakeLocalRuntime:
    """Returns a fixed result so run() never touches a real model."""

    name = "local"
    supported_ir_versions = ("1.0",)

    async def execute(self, spec, input, *, execution_id=None, scope=None, on_event=None):
        return RuntimeResult(
            output=_OUTPUT,
            execution_id="ex1",
            duration_ms=1.0,
            steps=[],
            tool_calls=[],
            llm_calls=[],
        )

    async def resume(self, spec, execution_id):
        raise NotImplementedError


def _fake_runtime(monkeypatch):
    monkeypatch.setattr("jamjet.agents.agent.LocalRuntime", FakeLocalRuntime)


@pytest.mark.asyncio
async def test_governed_turn_emits_a_valid_conformant_receipt(monkeypatch):
    _fake_runtime(monkeypatch)
    agent = Agent("researcher", model="gpt-4o", tools=[], strategy="react")

    result = await agent.run(_PROMPT)

    receipt = result.receipt
    assert receipt is not None, "receipts are ON by default"
    # Schema valid.
    assert validate_receipt(receipt) == []
    # Level 3 (Portable Proof) conformance: no fail-severity checks. Passing the
    # original arguments exercises the arguments_hash recompute too.
    args = agent_action_arguments(agent_name="researcher", model="gpt-4o", prompt=_PROMPT)
    fails = [c for c in check_conformance(receipt, level=3, arguments=args) if c.severity == "fail"]
    assert fails == [], f"conformance failures: {fails}"
    # The decision is recorded (Level 2 policy-bound).
    assert receipt["policy"]["decision"] == "allow"
    assert receipt["execution"]["status"] == "success"


@pytest.mark.asyncio
async def test_default_agent_has_receipts_on(monkeypatch):
    _fake_runtime(monkeypatch)
    # No governance kwargs at all -> still governed, still mints.
    agent = Agent("plain", model="gpt-4o", tools=[])
    assert agent.governance.receipts is True

    result = await agent.run("hi")

    assert result.receipt is not None
    assert validate_receipt(result.receipt) == []


@pytest.mark.asyncio
async def test_receipts_false_produces_no_receipt(monkeypatch):
    _fake_runtime(monkeypatch)
    agent = Agent("researcher", model="gpt-4o", tools=[], receipts=False)

    result = await agent.run(_PROMPT)

    assert result.receipt is None


@pytest.mark.asyncio
async def test_receipt_links_to_the_action_provenance(monkeypatch):
    _fake_runtime(monkeypatch)
    agent = Agent("researcher", model="gpt-4o", tools=[])

    result = await agent.run(_PROMPT)
    receipt = result.receipt

    # arguments_hash binds the receipt to THIS prompt/agent/model.
    expected = compute_arguments_hash(agent_action_arguments(agent_name="researcher", model="gpt-4o", prompt=_PROMPT))
    assert receipt["arguments_hash"] == expected
    # A different prompt would hash differently -> the receipt is not generic.
    other = compute_arguments_hash(
        agent_action_arguments(agent_name="researcher", model="gpt-4o", prompt="something else")
    )
    assert receipt["arguments_hash"] != other
    # The capability identifies the agent that acted.
    assert receipt["tool"]["capability"] == "agent.researcher"
    assert receipt["actor"]["type"] == "agent"


@pytest.mark.asyncio
async def test_receipt_emitter_ships_the_receipt(monkeypatch):
    _fake_runtime(monkeypatch)
    shipped: list[dict] = []
    agent = Agent("researcher", model="gpt-4o", tools=[], receipt_emitter=shipped.append)

    result = await agent.run(_PROMPT)

    assert len(shipped) == 1
    assert shipped[0] == result.receipt
    assert validate_receipt(shipped[0]) == []


@pytest.mark.asyncio
async def test_emitter_not_called_when_receipts_off(monkeypatch):
    _fake_runtime(monkeypatch)
    shipped: list[dict] = []
    agent = Agent("researcher", model="gpt-4o", tools=[], receipts=False, receipt_emitter=shipped.append)

    await agent.run(_PROMPT)

    assert shipped == []
