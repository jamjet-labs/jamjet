"""Inject runtime-managed attributes onto a @DurableAgent instance."""
from __future__ import annotations

from pathlib import Path

from engram import Engram
from engram import Scope as EngramScope

from jamjet.memory import AgentMemory, NoMemory
from jamjet.runtime.local.llm_adapters import get_adapter
from jamjet.runtime.local.seed import SeededClock, SeededRandom, SeededUuidGen
from jamjet.spec import DurableAgentSpec


async def inject_runtime_attributes(
    instance: object,
    *,
    spec: DurableAgentSpec,
    execution_id: str,
    scope: EngramScope | None = None,
) -> None:
    instance.workflow_id = execution_id  # type: ignore[attr-defined]
    instance.random = SeededRandom(execution_id)  # type: ignore[attr-defined]
    instance.uuid_gen = SeededUuidGen(execution_id)  # type: ignore[attr-defined]
    instance.now = SeededClock()  # type: ignore[attr-defined]

    s = scope or EngramScope(user_id="default", org_id="default")

    if spec.memory is None or not spec.memory.enabled or spec.memory.backend == "none":
        instance.memory = NoMemory()  # type: ignore[attr-defined]
    else:
        db_path = (
            Path(spec.memory.db_path) if spec.memory.db_path
            else Path.home() / ".jamjet" / "engram.db"
        )
        db_path.parent.mkdir(parents=True, exist_ok=True)
        engram = await Engram.open(path=str(db_path))
        instance.memory = AgentMemory(  # type: ignore[attr-defined]
            engram, scope=s, config=spec.memory, session_id=execution_id,
        )
        instance._jamjet_engram = engram  # type: ignore[attr-defined]  # caller must await engram.close()

    instance.llm = get_adapter(spec.llm)  # type: ignore[attr-defined]
