"""The JamJet model seam: the single governed entry point for all model calls.

No module outside this package may import a provider SDK on the hot path.
"""

from jamjet.model.middleware import (
    BaseModelMiddleware,
    ModelAllowlistMiddleware,
    ModelDeniedError,
    ModelMiddleware,
)
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
    "BaseModelMiddleware",
    "ModelAllowlistMiddleware",
    "ModelDeniedError",
    "ModelMiddleware",
    "ModelRef",
    "ModelRequest",
    "ModelResponse",
    "StreamChunk",
    "api_key_env_for",
    "parse_model_ref",
    "provider_literal_for",
]
