"""LLM adapters. Every provider now routes through the governed Model seam."""

from __future__ import annotations

from typing import TYPE_CHECKING

from jamjet.runtime.local.llm_adapters.base import LLMAdapter
from jamjet.runtime.local.llm_adapters.seam_adapter import SeamAdapter
from jamjet.spec import LLMConfig

if TYPE_CHECKING:
    from jamjet.agents.governance import GovernanceConfig


def get_adapter(config: LLMConfig, governance: GovernanceConfig | None = None) -> LLMAdapter:
    """Return the seam-backed adapter for any provider (provider routing is
    parsed from ``config.model`` inside the seam).

    ``governance`` (T3-7) threads the agent's :class:`GovernanceConfig` into the
    seam middleware chain so budget / allowlist / PII enforce on the in-process
    ``agent.run()`` path.  ``None`` keeps the Track-1 default (allow-all, no
    budget, PII on).
    """
    return SeamAdapter(config, governance=governance)


__all__ = ["LLMAdapter", "SeamAdapter", "get_adapter"]
