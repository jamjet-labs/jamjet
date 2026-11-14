import sys

import pytest

from jamjet.decorators import DurableAgent
from jamjet.entry import run
from jamjet.runtime.local import LocalRuntime
from jamjet.runtime.types import Scope
from jamjet.spec import DurabilityConfig, MemoryConfig


@DurableAgent(stateless=True)
class MemorylessAgent:
    async def run(self, value: str) -> dict[str, str]:
        return {"value": value, "execution_id": self.workflow_id, "memory": type(self.memory).__name__}


@pytest.mark.parametrize("entrypoint", ["run", "execute"])
@pytest.mark.parametrize("memory", [None, MemoryConfig(enabled=False), MemoryConfig(backend="none")])
@pytest.mark.asyncio
async def test_scope_does_not_require_engram_when_memory_is_disabled(tmp_path, monkeypatch, entrypoint, memory):
    monkeypatch.setitem(sys.modules, "engram", None)
    spec = MemorylessAgent.__jamjet_spec__.model_copy(
        update={"memory": memory, "durability": DurabilityConfig(db_path=str(tmp_path / "steps.db"))}
    )
    kwargs = {"execution_id": "scoped-run", "scope": Scope(user_id="alice", org_id="acme")}

    if entrypoint == "run":
        result = await run(spec, "hello", **kwargs)
    else:
        result = await LocalRuntime().execute(spec, "hello", **kwargs)

    assert result.output == {"value": "hello", "execution_id": "scoped-run", "memory": "NoMemory"}
    assert len(result.steps) == 1
    assert result.steps[0].status == "completed"

    resumed = await LocalRuntime().resume(spec, result.execution_id)
    assert resumed.output == result.output


@pytest.mark.parametrize("scope", [None, Scope(user_id="alice", org_id="acme")])
@pytest.mark.asyncio
async def test_enabled_memory_still_requires_engram(tmp_path, monkeypatch, scope):
    monkeypatch.setitem(sys.modules, "engram", None)
    spec = MemorylessAgent.__jamjet_spec__.model_copy(
        update={
            "memory": MemoryConfig(db_path=str(tmp_path / "memory.db")),
            "durability": DurabilityConfig(db_path=str(tmp_path / "steps.db")),
        }
    )

    with pytest.raises(ImportError, match="pip install 'jamjet\\[memory\\]' "):
        await run(spec, "hello", scope=scope)
