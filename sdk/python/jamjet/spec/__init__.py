"""JamJet agent IR — Pydantic spec models that runtimes consume."""
from jamjet.spec.durability import DurabilityConfig
from jamjet.spec.llm import LLMConfig
from jamjet.spec.tool import ToolSpec
from jamjet.spec.version import IR_VERSION

__all__ = ["DurabilityConfig", "IR_VERSION", "LLMConfig", "ToolSpec"]
