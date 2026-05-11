import pytest
from engram import Engram
from engram import Scope as EngramScope

from jamjet.memory import AgentMemory, Scope
from jamjet.spec import MemoryConfig


@pytest.fixture
async def bridge(tmp_path):
    engram = await Engram.open(path=str(tmp_path / "e.db"))
    scope = EngramScope(org_id="o1", user_id="u1")
    yield AgentMemory(engram, scope=scope, config=MemoryConfig(), session_id="s1")


def test_scope_re_export():
    s = Scope(user_id="u9", org_id="o9")
    assert s.user_id == "u9"


@pytest.mark.asyncio
async def test_as_scope_overrides_then_restores(bridge):
    await bridge.record("alice's secret", role="user")
    with bridge.as_scope(user_id="other"):
        await bridge.record("bob's secret", role="user")
    facts_u1 = await bridge.recall("secret")
    texts = [sf.fact.text for sf in facts_u1]
    assert any("alice" in t for t in texts)
    assert not any("bob" in t for t in texts)
