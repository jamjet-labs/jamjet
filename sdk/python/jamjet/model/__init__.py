"""The JamJet model seam: the single governed entry point for all model calls.

No module outside this package may import a provider SDK on the hot path.
"""

from jamjet.model.types import (
    ModelRef,
    ModelRequest,
    ModelResponse,
    StreamChunk,
    api_key_env_for,
    parse_model_ref,
    provider_literal_for,
)

__all__ = [
    "ModelRef",
    "ModelRequest",
    "ModelResponse",
    "StreamChunk",
    "api_key_env_for",
    "parse_model_ref",
    "provider_literal_for",
]
