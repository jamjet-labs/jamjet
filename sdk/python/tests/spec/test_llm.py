import pytest
from pydantic import ValidationError

from jamjet.spec import LLMConfig


def test_minimal_config():
    cfg = LLMConfig(provider="openai", model="gpt-4o")
    assert cfg.provider == "openai"
    assert cfg.model == "gpt-4o"
    assert cfg.api_key_env == "OPENAI_API_KEY"
    assert cfg.base_url is None
    assert cfg.temperature is None


def test_invalid_provider_rejected():
    with pytest.raises(ValidationError):
        LLMConfig(provider="hf-inference", model="any")


def test_extra_fields_rejected():
    with pytest.raises(ValidationError):
        LLMConfig(provider="openai", model="gpt-4o", typo_field=1)


def test_frozen_immutable():
    cfg = LLMConfig(provider="openai", model="gpt-4o")
    with pytest.raises(ValidationError):
        cfg.model = "gpt-4o-mini"  # type: ignore[misc]


def test_round_trip_json():
    cfg = LLMConfig(provider="anthropic", model="claude-sonnet-4-6", temperature=0.2)
    assert LLMConfig.model_validate_json(cfg.model_dump_json()) == cfg
