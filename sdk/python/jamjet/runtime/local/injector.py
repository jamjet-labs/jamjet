"""Inject runtime-managed attributes onto a @DurableAgent instance."""

from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from engram import Scope as EngramScope

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

    if spec.memory is None or not spec.memory.enabled or spec.memory.backend == "none":
        from jamjet.memory.nomemory import NoMemory

        instance.memory = NoMemory()  # type: ignore[attr-defined]
    else:
        try:
            from engram import Engram
            from engram import Scope as EngramScope  # noqa: F811
        except ImportError as e:
            raise ImportError(
                "Agent memory requires the 'memory' extra. Install with: pip install 'jamjet[memory]'"
            ) from e
        from jamjet.memory.engram_bridge import AgentMemory

        s = scope or EngramScope(user_id="default", org_id="default")
        db_path = Path(spec.memory.db_path) if spec.memory.db_path else Path.home() / ".jamjet" / "engram.db"
        db_path.parent.mkdir(parents=True, exist_ok=True)
        engram = await Engram.open(path=str(db_path))
        instance.memory = AgentMemory(  # type: ignore[attr-defined]
            engram,
            scope=s,
            config=spec.memory,
            session_id=execution_id,
        )
        instance._jamjet_engram = engram  # type: ignore[attr-defined]  # caller must await engram.close()

    instance.llm = get_adapter(spec.llm)  # type: ignore[attr-defined]
