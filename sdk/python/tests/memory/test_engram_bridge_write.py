import pytest
from engram import Engram
from engram import Scope as EngramScope

from jamjet.memory.engram_bridge import AgentMemory
from jamjet.spec import MemoryConfig


@pytest.fixture
async def bridge(tmp_path):
    engram = await Engram.open(path=str(tmp_path / "e.db"))
    scope = EngramScope(org_id="o1", user_id="u1")
    yield AgentMemory(engram, scope=scope, config=MemoryConfig(), session_id="sess1")


@pytest.mark.asyncio
async def test_record_returns_fact(bridge):
    fact = await bridge.record("the sky is blue")
    assert fact.text == "the sky is blue"
    assert fact.scope.user_id == "u1"
    assert fact.scope.org_id == "o1"


@pytest.mark.asyncio
async def test_record_with_role_metadata(bridge):
    fact = await bridge.record("user said hi", role="user")
    assert fact.metadata.get("role") == "user"


@pytest.mark.asyncio
async def test_record_message_stores_chat(bridge):
    msg = await bridge.record_message("hello there", role="user")
    assert msg.role == "user"
    assert msg.content == "hello there"


@pytest.mark.asyncio
async def test_record_with_metadata(bridge):
    fact = await bridge.record("x", metadata={"source": "test"})
    assert fact.metadata.get("source") == "test"
