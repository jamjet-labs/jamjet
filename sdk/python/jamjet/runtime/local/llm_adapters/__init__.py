"""LLM adapters keyed by provider. Phase 3 adds Anthropic, Google, Ollama, openai_compatible."""
from jamjet.runtime.local.llm_adapters.base import LLMAdapter
from jamjet.runtime.local.llm_adapters.openai import OpenAIAdapter
from jamjet.spec import LLMConfig


def get_adapter(config: LLMConfig) -> LLMAdapter:
    if config.provider == "openai":
        return OpenAIAdapter(config)
    raise NotImplementedError(
        f"Provider {config.provider!r} not implemented; lands in Phase 3. Use 'openai' for now."
    )


__all__ = ["LLMAdapter", "OpenAIAdapter", "get_adapter"]
