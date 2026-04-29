"""
Deterministic idempotency-key generation.

A key is a SHA-256 hex string of the canonicalized triple
(execution_id, fn_qualname, args_fingerprint). Keys must be:

  * Stable across process restarts — same inputs → same key
  * Order-independent for kwargs — {"a":1,"b":2} == {"b":2,"a":1}
  * Reject inputs that can't round-trip safely

Args that are pydantic models are dumped via `.model_dump()`.
Other args must be JSON-serializable.
"""

from __future__ import annotations

import hashlib
import json
from typing import Any

try:
    from pydantic import BaseModel
except ImportError:  # pydantic is a hard dep but be defensive
    BaseModel = None  # type: ignore


def _to_canonical(value: Any) -> Any:
    """Convert value into a JSON-canonicalizable form."""
    if BaseModel is not None and isinstance(value, BaseModel):
        return value.model_dump(mode="json")
    if isinstance(value, dict):
        return {k: _to_canonical(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [_to_canonical(v) for v in value]
    if isinstance(value, (str, int, float, bool)) or value is None:
        return value
    # Fall through — let json.dumps raise the TypeError.
    return value


def args_fingerprint(args: tuple, kwargs: dict) -> str:
    """Compute a stable fingerprint over (args, kwargs)."""
    canonical = {
        "args": [_to_canonical(a) for a in args],
        "kwargs": {k: _to_canonical(v) for k, v in kwargs.items()},
    }
    try:
        # sort_keys ensures kwargs ordering doesn't matter.
        encoded = json.dumps(canonical, sort_keys=True, separators=(",", ":"))
    except TypeError as e:
        raise TypeError(
            f"@durable arguments are not JSON-serializable: {e}. Use primitive types, dicts, lists, or pydantic models."
        ) from e
    return hashlib.sha256(encoded.encode("utf-8")).hexdigest()


def generate_key(
    execution_id: str,
    fn_qualname: str,
    args: tuple,
    kwargs: dict,
) -> str:
    """
    Compose a deterministic SHA-256 key from
      (execution_id, fn_qualname, args_fingerprint(args, kwargs)).
    """
    fp = args_fingerprint(args, kwargs)
    payload = f"{execution_id}\x00{fn_qualname}\x00{fp}"
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()
