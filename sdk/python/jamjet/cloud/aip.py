"""AIP (Agent Identity Protocol) support — Plan 5 Phase 1.6.

OPT-IN. Requires the ``aip`` extra: ``pip install jamjet[aip]``. Without it,
calling these helpers raises ImportError pointing at the install command.

Wire format (compact mode, single-hop):
    at:<base64url(header)>.<base64url(payload)>.<base64url(sig)>
where header = {"alg":"EdDSA","typ":"AIP"} and payload carries:
    iss        — agent name (token issuer)
    aud        — project_id (UUID with dashes)
    iat / exp  — unix seconds
    delegated_tools (optional) — glob list, narrows allow set on the receiver
    cost_max_usd (optional)   — hard cost cap on any single tool call
    parent_agent (optional)   — upstream agent in a single-hop delegation

Multi-hop chains (Biscuit-based) are not in this build — single-hop covers
the launch demo and adds the verifiable-identity badge on the dashboard.
"""

from __future__ import annotations

import base64
import json
import threading
import time
from dataclasses import dataclass
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from cryptography.hazmat.primitives.asymmetric.ed25519 import (
        Ed25519PrivateKey,
        Ed25519PublicKey,
    )

_INSTALL_HINT = (
    "jamjet.cloud.aip requires the 'aip' extra. Install with: pip install jamjet[aip]"
)


def _require_crypto():
    try:
        from cryptography.hazmat.primitives.asymmetric.ed25519 import (
            Ed25519PrivateKey,  # noqa: F401
        )
    except ImportError as e:
        raise ImportError(_INSTALL_HINT) from e


def _b64url(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode("ascii")


def _b64url_decode(s: str) -> bytes:
    pad = "=" * (-len(s) % 4)
    return base64.urlsafe_b64decode(s + pad)


# ---------------------------------------------------------------------------
# Process-wide AIP state
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class AipKeypair:
    """Holds the agent's signing key + the public key the cloud verifies against.

    Most agents only need one keypair per process. The Span emitter pulls it
    from this slot when minting tokens for the active agent.
    """

    public_key_b64: str
    _private: "Ed25519PrivateKey"


_state_lock = threading.Lock()
_keypair: AipKeypair | None = None


@dataclass(frozen=True)
class _EmitterCfg:
    """Tells the event emitter to attach a freshly-minted AIP token to each
    outgoing event so the cloud can stamp ``events.aip_verified=true``.

    Populated by :func:`register_with_cloud` (since registration is what gives
    us a verifiable identity in the first place). Cleared by
    :func:`disable_emitter_attach` if the caller wants to stop attaching
    without unloading the keypair.
    """

    agent_name: str
    project_id: str
    ttl_seconds: int = 300


_emitter: _EmitterCfg | None = None
_emitter_attach_enabled: bool = True


def generate_keypair() -> AipKeypair:
    """Generate a fresh Ed25519 keypair. Use during dev / on first boot.

    Production agents typically load a long-lived keypair from KMS / disk via
    :func:`use_keypair` instead — generating per process means tokens can't be
    re-verified after a restart.
    """
    _require_crypto()
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

    private = Ed25519PrivateKey.generate()
    pub_bytes = private.public_key().public_bytes_raw()
    kp = AipKeypair(public_key_b64=_b64url(pub_bytes), _private=private)
    set_keypair(kp)
    return kp


def use_keypair(*, private_key_pem: bytes | None = None, private_key_raw: bytes | None = None) -> AipKeypair:
    """Load an existing Ed25519 keypair from PEM or raw 32 bytes."""
    _require_crypto()
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

    if private_key_pem:
        private = serialization.load_pem_private_key(private_key_pem, password=None)
        if not isinstance(private, Ed25519PrivateKey):
            raise ValueError("PEM did not decode to an Ed25519PrivateKey")
    elif private_key_raw:
        if len(private_key_raw) != 32:
            raise ValueError(f"Ed25519 raw private key must be 32 bytes; got {len(private_key_raw)}")
        private = Ed25519PrivateKey.from_private_bytes(private_key_raw)
    else:
        raise ValueError("Pass private_key_pem= or private_key_raw=")

    pub_bytes = private.public_key().public_bytes_raw()
    kp = AipKeypair(public_key_b64=_b64url(pub_bytes), _private=private)
    set_keypair(kp)
    return kp


def set_keypair(kp: AipKeypair) -> None:
    global _keypair
    with _state_lock:
        _keypair = kp


def get_keypair() -> AipKeypair | None:
    return _keypair


# ---------------------------------------------------------------------------
# Token issuance
# ---------------------------------------------------------------------------


def mint_token(
    *,
    issuer: str,
    project_id: str,
    ttl_seconds: int = 300,
    delegated_tools: list[str] | None = None,
    cost_max_usd: float | None = None,
    parent_agent: str | None = None,
) -> str:
    """Mint a signed AIP compact token. Caller is responsible for delivering
    it to the receiving agent (HTTP header, tracestate, etc.)."""
    _require_crypto()
    kp = get_keypair()
    if kp is None:
        raise RuntimeError(
            "no AIP keypair set; call jamjet.cloud.aip.generate_keypair() or use_keypair() first"
        )

    now = int(time.time())
    header = {"alg": "EdDSA", "typ": "AIP"}
    payload: dict = {"iss": issuer, "aud": project_id, "iat": now, "exp": now + ttl_seconds}
    if delegated_tools is not None:
        payload["delegated_tools"] = list(delegated_tools)
    if cost_max_usd is not None:
        payload["cost_max_usd"] = cost_max_usd
    if parent_agent is not None:
        payload["parent_agent"] = parent_agent

    h = _b64url(json.dumps(header, separators=(",", ":")).encode("utf-8"))
    p = _b64url(json.dumps(payload, separators=(",", ":"), sort_keys=True).encode("utf-8"))
    signing_input = f"{h}.{p}".encode("ascii")
    sig = kp._private.sign(signing_input)
    return f"{h}.{p}.{_b64url(sig)}"


def mint_for_event() -> str | None:
    """Mint a fresh AIP token for the configured emitter agent, or return None
    if AIP auto-attach is not enabled in this process. Called from the event
    queue's send path; kept lightweight (Ed25519 sign is microseconds)."""
    cfg = _emitter
    if cfg is None or not _emitter_attach_enabled or _keypair is None:
        return None
    try:
        return mint_token(
            issuer=cfg.agent_name,
            project_id=cfg.project_id,
            ttl_seconds=cfg.ttl_seconds,
        )
    except Exception:
        # Never let an AIP minting bug break event ingest.
        return None


def disable_emitter_attach() -> None:
    """Stop attaching AIP tokens to outgoing events. The keypair stays loaded
    so :func:`mint_token` keeps working for explicit delegation flows."""
    global _emitter_attach_enabled
    _emitter_attach_enabled = False


def enable_emitter_attach() -> None:
    """Resume attaching AIP tokens to outgoing events (default state)."""
    global _emitter_attach_enabled
    _emitter_attach_enabled = True


def peek_claims(token: str) -> dict:
    """Decode a token's claims without verifying. Useful for client-side display
    of who delegated what (the cloud still verifies on receipt)."""
    parts = token.split(".")
    if len(parts) != 3:
        raise ValueError("malformed AIP token")
    return json.loads(_b64url_decode(parts[1]))


# ---------------------------------------------------------------------------
# Public-key registration with the cloud
# ---------------------------------------------------------------------------


def register_with_cloud(
    *,
    agent_name: str,
    api_key: str,
    api_url: str = "https://api.jamjet.dev",
    card_uri: str | None = None,
    description: str | None = None,
) -> dict:
    """POST the keypair's public half to the cloud so it can verify tokens
    minted by this process. Idempotent — safe to call on every boot.

    Returns the cloud's response, including ``project_id`` so the caller can
    use it as the ``aud`` claim when minting tokens without a separate lookup.
    """
    _require_crypto()
    import httpx

    kp = get_keypair()
    if kp is None:
        raise RuntimeError("no AIP keypair set; call generate_keypair() first")

    body: dict = {"aip_public_key": kp.public_key_b64}
    if card_uri:
        body["card_uri"] = card_uri
    if description:
        body["description"] = description
    resp = httpx.post(
        f"{api_url}/v1/agents/{agent_name}/aip-key",
        json=body,
        headers={"Authorization": f"Bearer {api_key}", "Content-Type": "application/json"},
        timeout=10,
    )
    resp.raise_for_status()
    data = resp.json()

    # Wire the emitter so subsequent events get an AIP token attached.
    project_id = data.get("project_id")
    if project_id:
        global _emitter
        _emitter = _EmitterCfg(agent_name=agent_name, project_id=project_id)

    return data
