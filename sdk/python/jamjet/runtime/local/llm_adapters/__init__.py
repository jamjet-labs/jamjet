"""LLM adapters. Every provider now routes through the governed Model seam."""

from jamjet.runtime.local.llm_adapters.base import LLMAdapter
from jamjet.runtime.local.llm_adapters.seam_adapter import SeamAdapter
from jamjet.spec import LLMConfig


def get_adapter(config: LLMConfig) -> LLMAdapter:
    """Return the seam-backed adapter for any provider (provider routing is
    parsed from ``config.model`` inside the seam)."""
    return SeamAdapter(config)


__all__ = ["LLMAdapter", "SeamAdapter", "get_adapter"]
