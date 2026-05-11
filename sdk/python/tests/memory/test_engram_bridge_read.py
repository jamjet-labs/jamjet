import pytest
from engram import Engram
from engram import Scope as EngramScope

from jamjet.memory.engram_bridge import AgentMemory
from jamjet.spec import MemoryConfig


@pytest.fixture
async def bridge_with_data(tmp_path):
    engram = await Engram.open(path=str(tmp_path / "e.db"))
    scope = EngramScope(org_id="o1", user_id="u1")
    am = AgentMemory(engram, scope=scope, config=MemoryConfig(), session_id="s1")
    await am.record("user likes pizza", role="user")
    await am.record("the meeting is at 3pm", role="user")
    await am.record("agent said the answer is 42", role="assistant")
    yield am


@pytest.mark.asyncio
async def test_recall_returns_scored_facts(bridge_with_data):
    facts = await bridge_with_data.recall("food preference", top_k=5)
    assert isinstance(facts, list)
    assert len(facts) > 0
    # Each item should have .fact attribute
    assert all(hasattr(sf, "fact") for sf in facts)


@pytest.mark.asyncio
async def test_context_returns_string(bridge_with_data):
    s = await bridge_with_data.context("food preference", token_budget=200)
    assert isinstance(s, str)


@pytest.mark.asyncio
async def test_role_filter_excludes_other_role(bridge_with_data):
    facts = await bridge_with_data.recall("any", role_filter=("assistant",))
    for sf in facts:
        assert sf.fact.metadata.get("role") == "assistant"


@pytest.mark.asyncio
async def test_ask_default_mode_context_returns_str(bridge_with_data):
    out = await bridge_with_data.ask("food preference")
    assert isinstance(out, str)


@pytest.mark.asyncio
async def test_ask_recall_mode_returns_list(bridge_with_data):
    out = await bridge_with_data.ask("food preference", mode="recall")
    assert isinstance(out, list)


@pytest.mark.asyncio
async def test_synthesize_without_llm_raises(bridge_with_data):
    """Synthesize requires MemoryConfig.llm to be set; otherwise raises clearly."""
    with pytest.raises(RuntimeError, match="synthesize"):
        await bridge_with_data.synthesize("food preference")
