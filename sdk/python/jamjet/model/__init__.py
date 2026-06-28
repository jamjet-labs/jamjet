"""The JamJet model seam: the single governed entry point for all model calls.

No module outside this package may import a provider SDK on the hot path.
"""

from jamjet.model.budget import BudgetMiddleware
from jamjet.model.defaults import default_model_middleware
from jamjet.model.metering import MeteringMiddleware, ModelCallRecord
from jamjet.model.middleware import (
    BaseModelMiddleware,
    BudgetExceededError,
    ModelAllowlistMiddleware,
    ModelDeniedError,
    ModelMiddleware,
)
from jamjet.model.seam import Model
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
    "BudgetExceededError",
    "BudgetMiddleware",
    "MeteringMiddleware",
    "Model",
    "ModelAllowlistMiddleware",
    "ModelCallRecord",
    "ModelDeniedError",
    "ModelMiddleware",
    "ModelRef",
    "ModelRequest",
    "ModelResponse",
    "StreamChunk",
    "api_key_env_for",
    "default_model_middleware",
    "parse_model_ref",
    "provider_literal_for",
]
