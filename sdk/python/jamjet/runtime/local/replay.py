"""Deterministic step_id derivation + input hashing for replay."""
from __future__ import annotations

import hashlib
import json
from typing import Any


def derive_step_id(*, parent_step_id: str | None, call_site: str, invocation_index: int) -> str:
    h = hashlib.sha256()
    h.update((parent_step_id or "").encode())
    h.update(b"\x00")
    h.update(call_site.encode())
    h.update(b"\x00")
    h.update(str(invocation_index).encode())
    return h.hexdigest()[:16]


def compute_input_hash(value: Any) -> str:
    """Stable hash of a JSON-serializable value. Dict order independent."""
    payload = json.dumps(value, sort_keys=True, default=str)
    return hashlib.sha256(payload.encode()).hexdigest()[:16]
