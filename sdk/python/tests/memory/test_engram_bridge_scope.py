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
async def test_as_scope_yields_scoped_view_without_mutating_base(bridge):
    """I3: ``as_scope`` yields a per-run scoped VIEW; it does not mutate the base.

    A record made THROUGH the yielded view lands under the override scope, while
    the base instance's scope is never touched — so concurrent runs that each
    scope the one cached bridge by their own ``session.id`` cannot cross.  (The
    pre-fix API mutated the shared scope around the await; recording on the base
    instance inside the block leaked across sessions.)
    """
    # Base-scope (u1) record.
    await bridge.record("alice's secret", role="user")

    # A record through the SCOPED VIEW lands under the override scope ("other"),
    # and the base instance's scope is never mutated.
    with bridge.as_scope(user_id="other") as scoped:
        assert scoped is not bridge, "as_scope must yield a distinct per-run view"
        await scoped.record("bob's secret", role="user")
    assert bridge._scope.user_id == "u1", "base scope must not be mutated by as_scope"

    # Recall under the base scope (u1) sees alice but NOT bob (bob is under "other").
    facts_u1 = await bridge.recall("secret")
    texts = [sf.fact.text for sf in facts_u1]
    assert any("alice" in t for t in texts)
    assert not any("bob" in t for t in texts)

    # The scoped view retrieves bob under "other" (isolation holds both ways).
    with bridge.as_scope(user_id="other") as scoped:
        facts_other = await scoped.recall("secret")
    texts_other = [sf.fact.text for sf in facts_other]
    assert any("bob" in t for t in texts_other)
