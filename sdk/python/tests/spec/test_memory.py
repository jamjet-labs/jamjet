import pytest
from pydantic import ValidationError

from jamjet.spec import LLMConfig, MemoryConfig


def test_zero_arg_default_works():
    cfg = MemoryConfig()
    assert cfg.enabled is True
    assert cfg.backend == "engram_embedded"
    assert cfg.default_mode == "context"
    assert cfg.use_classifier is True
    assert cfg.default_role_filter is None
    assert cfg.default_token_budget is None
    assert cfg.decompose is False
    assert cfg.db_path is None
    assert cfg.remote_url is None
    assert cfg.llm is None


def test_role_filter_tuple():
    cfg = MemoryConfig(default_role_filter=("user",))
    assert cfg.default_role_filter == ("user",)


def test_disabled_backend():
    cfg = MemoryConfig(enabled=False)
    assert cfg.enabled is False


def test_invalid_mode_rejected():
    with pytest.raises(ValidationError):
        MemoryConfig(default_mode="invalid")  # type: ignore[arg-type]


def test_with_llm_override():
    cfg = MemoryConfig(llm=LLMConfig(provider="openai", model="gpt-4o-mini"))
    assert cfg.llm is not None
    assert cfg.llm.model == "gpt-4o-mini"


def test_round_trip_json():
    cfg = MemoryConfig(default_role_filter=("user", "assistant"), decompose=True)
    assert MemoryConfig.model_validate_json(cfg.model_dump_json()) == cfg
