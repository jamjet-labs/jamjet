import pytest

from jamjet.runtime.local.llm_adapters import get_adapter
from jamjet.runtime.local.llm_adapters.base import LLMAdapter
from jamjet.runtime.local.llm_adapters.seam_adapter import SeamAdapter
from jamjet.spec import LLMConfig


def test_openai_adapter_resolves():
    cfg = LLMConfig(provider="openai", model="gpt-4o")
    adapter = get_adapter(cfg)
    assert isinstance(adapter, LLMAdapter)
    assert isinstance(adapter, SeamAdapter)
    assert adapter.config.provider == "openai"


@pytest.mark.parametrize("provider", ["anthropic", "google", "ollama", "openai_compatible"])
def test_any_provider_returns_seam_adapter(provider):
    cfg = LLMConfig(provider=provider, model="any")
    adapter = get_adapter(cfg)
    assert isinstance(adapter, SeamAdapter)
