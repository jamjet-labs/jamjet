"""JamJet memory: NoMemory is always available; AgentMemory/Scope need the 'memory' extra (engram)."""

from jamjet.memory.nomemory import NoMemory

__all__ = ["AgentMemory", "NoMemory", "Scope"]


def __getattr__(name: str):  # noqa: ANN001, ANN202
    if name == "Scope":
        try:
            from engram import Scope
        except ImportError as e:
            raise ImportError(
                "jamjet.memory.Scope requires the 'memory' extra. Install with: pip install 'jamjet[memory]'"
            ) from e
        return Scope
    if name == "AgentMemory":
        try:
            from jamjet.memory.engram_bridge import AgentMemory
        except ImportError as e:
            raise ImportError(
                "jamjet.memory.AgentMemory requires the 'memory' extra. Install with: pip install 'jamjet[memory]'"
            ) from e
        return AgentMemory
    raise AttributeError(f"module 'jamjet.memory' has no attribute {name!r}")
