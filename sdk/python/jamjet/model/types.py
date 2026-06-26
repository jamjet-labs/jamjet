"""Provider-agnostic value types and model-string parsing for the seam."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

_KNOWN_PROVIDER_LITERALS = {"openai", "anthropic", "google", "ollama"}
_GOOGLE_ALIASES = {"gemini", "vertex_ai", "vertexai"}
_API_KEY_ENVS = {
    "anthropic": "ANTHROPIC_API_KEY",
    "openai": "OPENAI_API_KEY",
    "google": "GEMINI_API_KEY",
    "gemini": "GEMINI_API_KEY",
    "ollama": "OLLAMA_API_KEY",
}


@dataclass(frozen=True)
class ModelRef:
    """A parsed provider-routed model reference.

    ``litellm_model`` is the exact string handed to LiteLLM.
    """

    provider: str
    model: str
    litellm_model: str


@dataclass
class ModelRequest:
    """One model call as it enters the seam."""

    ref: ModelRef
    messages: list[dict[str, Any]]
    tools: list[dict[str, Any]] | None = None
    temperature: float | None = None
    max_tokens: int | None = None
    stream: bool = False
    metadata: dict[str, Any] = field(default_factory=dict)


@dataclass
class ModelResponse:
    """A completed (non-streaming) model call leaving the seam.

    ``message`` is an OpenAI-shaped message (has ``.content`` and ``.tool_calls``)
    so existing strategy runners consume it unchanged.
    """

    message: Any
    input_tokens: int = 0
    output_tokens: int = 0
    cost_usd: float = 0.0
    raw: Any = None


@dataclass
class StreamChunk:
    """One incremental streaming delta."""

    delta: str
    raw: Any = None


def parse_model_ref(model: str) -> ModelRef:
    """Parse a provider-routed model string.

    ``"anthropic/claude-opus-4-8"`` -> provider ``anthropic``. A bare string
    with no ``provider/`` prefix defaults to provider ``openai`` (matching the
    historical default).
    """
    raw = model.strip()
    if "/" in raw:
        provider, _, rest = raw.partition("/")
        provider = provider.strip().lower()
        normalized_litellm = f"{provider}/{rest}"
        return ModelRef(provider=provider, model=rest, litellm_model=normalized_litellm)
    return ModelRef(provider="openai", model=raw, litellm_model=raw)


def provider_literal_for(model: str) -> str:
    """Map a model string to a valid ``LLMConfig.provider`` Literal value."""
    ref = parse_model_ref(model)
    if ref.provider in _KNOWN_PROVIDER_LITERALS:
        return ref.provider
    if ref.provider in _GOOGLE_ALIASES:
        return "google"
    return "openai_compatible"


def api_key_env_for(provider: str) -> str:
    """Best-effort env-var name for a provider's API key.

    Informational only on the seam path: LiteLLM reads provider keys from the
    environment itself. Kept for non-seam back-compat.
    """
    return _API_KEY_ENVS.get(provider.lower(), "OPENAI_API_KEY")
