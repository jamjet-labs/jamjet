"""JamJet agent IR — Pydantic spec models that runtimes consume."""
from jamjet.spec.llm import LLMConfig
from jamjet.spec.version import IR_VERSION

__all__ = ["IR_VERSION", "LLMConfig"]
