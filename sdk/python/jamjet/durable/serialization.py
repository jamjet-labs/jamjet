"""
Pickle-based safe round-trip for cached values.

The @durable decorator stores function results in the cache. We use pickle
because it round-trips arbitrary Python objects, but we wrap it so that
unpicklable values fail loudly at @durable call time (not silently when the
cache is later read).
"""

from __future__ import annotations

import pickle
from typing import Any


def dumps(value: Any) -> bytes:
    """Serialize a value. Raises TypeError if the value is unpicklable."""
    try:
        return pickle.dumps(value, protocol=pickle.HIGHEST_PROTOCOL)
    except (pickle.PicklingError, AttributeError, TypeError) as e:
        raise TypeError(
            f"@durable result is not picklable: {e}. "
            "Return primitive types, dicts, lists, dataclasses, or pydantic models."
        ) from e


def loads(blob: bytes) -> Any:
    """Deserialize a value from bytes. Trusts the input — only call on cache reads."""
    return pickle.loads(blob)
