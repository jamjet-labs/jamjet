from jamjet.model.types import (
    ModelRef,
    api_key_env_for,
    parse_model_ref,
    provider_literal_for,
)


def test_parses_provider_routed_string():
    ref = parse_model_ref("anthropic/claude-opus-4-8")
    assert ref == ModelRef(
        provider="anthropic",
        model="claude-opus-4-8",
        litellm_model="anthropic/claude-opus-4-8",
    )


def test_bare_string_defaults_to_openai():
    ref = parse_model_ref("gpt-4o")
    assert ref.provider == "openai"
    assert ref.model == "gpt-4o"
    assert ref.litellm_model == "gpt-4o"


def test_provider_is_lowercased_and_trimmed():
    ref = parse_model_ref("  Anthropic/claude-opus-4-8 ")
    assert ref.provider == "anthropic"
    assert ref.litellm_model == "anthropic/claude-opus-4-8"


def test_provider_literal_maps_known_and_unknown():
    assert provider_literal_for("anthropic/claude-opus-4-8") == "anthropic"
    assert provider_literal_for("gpt-4o") == "openai"
    assert provider_literal_for("gemini/gemini-1.5-pro") == "google"
    assert provider_literal_for("bedrock/anthropic.claude-v2") == "openai_compatible"


def test_api_key_env_for_known_providers():
    assert api_key_env_for("anthropic") == "ANTHROPIC_API_KEY"
    assert api_key_env_for("openai") == "OPENAI_API_KEY"
    assert api_key_env_for("gemini") == "GEMINI_API_KEY"
    assert api_key_env_for("something-else") == "OPENAI_API_KEY"
