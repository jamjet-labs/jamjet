import pytest

from jamjet.runtime.local.llm_adapters import get_adapter
from jamjet.runtime.local.llm_adapters.base import LLMAdapter
from jamjet.runtime.local.llm_adapters.openai import OpenAIAdapter
from jamjet.spec import LLMConfig


def test_openai_adapter_resolves():
    cfg = LLMConfig(provider="openai", model="gpt-4o")
    adapter = get_adapter(cfg)
    assert isinstance(adapter, LLMAdapter)
    assert isinstance(adapter, OpenAIAdapter)
    assert adapter.config.provider == "openai"


@pytest.mark.parametrize("provider", ["anthropic", "google", "ollama", "openai_compatible"])
def test_unsupported_provider_raises(provider):
    cfg = LLMConfig(provider=provider, model="any")
    with pytest.raises(NotImplementedError, match="Phase 3"):
        get_adapter(cfg)
