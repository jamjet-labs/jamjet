"""Verify the JamJet runtime's audit log offline, with only its public key.

The runtime seals every audit entry into a hash chain. In Ed25519 mode
(``JAMJET_AUDIT_SIGNING_MODE=ed25519``) it also signs each entry with a private
key only the runtime holds, so anyone with the public key can check an export
of ``GET /audit`` without being able to forge one. (In HMAC mode the same key
signs and verifies, so a verifier could also forge; this module does not
verify HMAC entries.)

Each entry is checked on its own: its ``entry_hash`` must be the SHA-256 the
runtime computes over its fields (``runtime/audit/src/entry.rs``,
``AuditLogEntry::content_hash``), and its ``signature`` must be a valid Ed25519
signature, by the given key, over the signing statement::

    JCS({"alg": "Ed25519", "entry_hash": <entry_hash>, "kid": <kid>,
         "spec": "jamjet.audit.entry/v1"})

``JCS`` is RFC 8785. Every member is an ASCII string, so the statement is the
members sorted by name with no whitespace. ``kid`` is the first 16 hex digits
of SHA-256 over the 32-byte public key. The stored ``signature`` is
``ed25519:<kid>:<128 hex digits>``.

Usage::

    import httpx
    from jamjet.runtime_audit import (
        chain_order, load_entries, public_key_from_did_document, verify_entries,
    )

    base = "http://localhost:7700"
    public_key = public_key_from_did_document(httpx.get(f"{base}/.well-known/did.json").json())
    text = httpx.get(f"{base}/audit", params={"limit": 200}, headers=auth).text
    entries = load_entries(text)
    verify_entries(chain_order(entries), public_key)  # raises ChainError

Pin the public key you trust out of band rather than fetching it from the
runtime you are checking: the DID document only tells you which key the
runtime says it uses.

``load_entries`` keeps every number as the runtime wrote it. The hash covers
the runtime's own JSON text of each event, and a float that is parsed into a
Python float and printed again can come back in a different spelling
(``1e16`` and ``1e+16``).
"""

from __future__ import annotations

import hashlib
import json
import re
import uuid
from collections.abc import Iterable, Mapping, Sequence
from typing import Any

SIG_SPEC = "jamjet.audit.entry/v1"
SIG_PREFIX = "ed25519:"
_FIELD_SEP = b"\x1f"
_P = 2**255 - 19
# Encodings of the points of order dividing 8, sign bit cleared (libsodium's
# blocklist). A key or an R among them proves nothing; the runtime refuses
# both (ed25519-dalek `verify_strict`), and so must this verifier, since
# OpenSSL accepts them.
_SMALL_ORDER = frozenset(
    bytes.fromhex(h)
    for h in (
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0100000000000000000000000000000000000000000000000000000000000000",
        "26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05",
        "c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a",
        "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
    )
)
_HEX128 = re.compile(r"[0-9a-f]{128}")


class ChainError(Exception):
    """An audit entry failed verification.

    ``index`` is the entry's position in the list passed in; ``reason`` is one of
    ``"unsealed"``, ``"unsigned"``, ``"broken_link"``, ``"hash_mismatch"``,
    ``"bad_signature"``, ``"wrong_key"`` and ``"malformed"`` (a field Rust would
    not read: wrong JSON type, or a UUID not in lowercase hyphenated form).
    """

    def __init__(self, index: int, reason: str, detail: str = "") -> None:
        self.index = index
        self.reason = reason
        message = f"audit entry {index} failed: {reason}"
        if detail:
            message += f" ({detail})"
        super().__init__(message)


class _Number(str):
    """A JSON number kept as the text it was written as."""


def load_entries(text: str | bytes) -> list[dict[str, Any]]:
    """Parse a ``GET /audit`` response (``{"items": [...]}``) or a JSON list of
    entries, keeping every number's original text."""
    data = json.loads(text, parse_float=_Number, parse_int=_Number)
    if isinstance(data, Mapping):
        data = data.get("items")
    if not isinstance(data, list):
        raise ValueError("expected a list of audit entries or an object with an 'items' list")
    return data


def _escape(s: str) -> str:
    # serde_json's escaping: quote, backslash, and control characters; the
    # five with short forms use them, the rest \u00XX in lowercase hex.
    out = []
    for ch in s:
        o = ord(ch)
        if ch == '"':
            out.append('\\"')
        elif ch == "\\":
            out.append("\\\\")
        elif o < 0x20:
            short = {0x08: "\\b", 0x09: "\\t", 0x0A: "\\n", 0x0C: "\\f", 0x0D: "\\r"}.get(o)
            out.append(short or f"\\u{o:04x}")
        else:
            out.append(ch)
    return '"' + "".join(out) + '"'


def _serde_json(value: Any) -> str:
    """``serde_json::to_string`` of a ``serde_json::Value``: compact, object
    members sorted by name."""
    if value is None:
        return "null"
    if value is True:
        return "true"
    if value is False:
        return "false"
    if isinstance(value, _Number):
        return str.__str__(value)
    if isinstance(value, bool):  # pragma: no cover - handled above
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        raise ValueError("floats must be loaded with load_entries so their spelling is kept")
    if isinstance(value, str):
        return _escape(value)
    if isinstance(value, Mapping):
        return "{" + ",".join(f"{_escape(k)}:{_serde_json(value[k])}" for k in sorted(value)) + "}"
    if isinstance(value, (list, tuple)):
        return "[" + ",".join(_serde_json(v) for v in value) + "]"
    raise ValueError(f"not a JSON value: {type(value).__name__}")


def _rfc3339(ts: str) -> str:
    # The hash covers chrono's `to_rfc3339()`, which writes UTC as `+00:00`;
    # serialized entries carry the same instant with a `Z`.
    return ts[:-1] + "+00:00" if ts.endswith("Z") else ts


def _weak_point(b: bytes) -> bool:
    """A small-order point, or an encoding whose y is not reduced mod p."""
    cleared = b[:31] + bytes([b[31] & 0x7F])
    return cleared in _SMALL_ORDER or int.from_bytes(cleared, "little") >= _P


def _check_public_key(public_key: bytes) -> None:
    if len(public_key) != 32:
        raise ValueError("an Ed25519 public key is 32 bytes")
    if _weak_point(public_key):
        raise ValueError("not a usable Ed25519 public key (small order or non-canonical)")


def _uuid(entry: Mapping[str, Any], key: str) -> bytes:
    value = entry[key]
    if not isinstance(value, str) or str(uuid.UUID(value)) != value:
        raise ValueError(f"{key} is not a lowercase hyphenated UUID")
    return uuid.UUID(value).bytes


def _int(entry: Mapping[str, Any], key: str) -> int:
    value = entry[key]
    # Numbers arrive as _Number (their JSON text); an integer has digits only.
    if not isinstance(value, _Number) or not re.fullmatch(r"-?[0-9]+", value):
        raise ValueError(f"{key} is not a JSON integer")
    number = int(value)
    # Rust reads these as i64; a value outside it is not an entry Rust wrote.
    if not -(2**63) <= number < 2**63:
        raise ValueError(f"{key} is outside the i64 range")
    return number


def _text(entry: Mapping[str, Any], key: str, *, optional: bool = True) -> str:
    """A string field; an absent optional one is "". Anything that is not a
    JSON string (a number, a list, a null where Rust needs a string) is
    malformed."""
    if key not in entry or (optional and entry[key] is None):
        if optional:
            return ""
        raise ValueError(f"{key} is missing")
    v = entry[key]
    if not isinstance(v, str) or isinstance(v, _Number):
        raise ValueError(f"{key} is not a string")
    return v


def entry_content_hash(entry: Mapping[str, Any], prev_hash: str | None) -> str:
    """The runtime's ``AuditLogEntry::content_hash`` of a serialized entry."""
    h = hashlib.sha256()

    def field(b: bytes) -> None:
        h.update(b)
        h.update(_FIELD_SEP)

    field(_uuid(entry, "id"))
    field(_uuid(entry, "event_id"))
    field(_text(entry, "execution_id", optional=False).encode())
    field(_int(entry, "sequence").to_bytes(8, "little", signed=True))
    field(_text(entry, "event_type", optional=False).encode())
    field(_text(entry, "actor_id", optional=False).encode())
    field(_text(entry, "actor_type", optional=False).encode())
    for key in ("tool_call_hash", "policy_decision", "http_request_id", "http_method", "http_path", "ip_address"):
        field(_text(entry, key).encode())
    field(_rfc3339(_text(entry, "created_at", optional=False)).encode())
    field(_serde_json(entry["raw_event"]).encode())
    # Rust defaults a missing tenant_id to "default"; a null is not a string.
    if "tenant_id" in entry:
        field(_text(entry, "tenant_id", optional=False).encode())
    else:
        field(b"default")
    expires = _text(entry, "expires_at")
    field(_rfc3339(expires).encode() if expires else b"")
    redacted = entry.get("redacted", False)
    if not isinstance(redacted, bool):
        raise ValueError("redacted is not a boolean")
    field(b"\x01" if redacted else b"\x00")
    field((prev_hash or "").encode())
    return h.hexdigest()


def key_id(public_key: bytes) -> str:
    """The ``kid`` the runtime writes for ``public_key`` (32 raw bytes)."""
    return hashlib.sha256(public_key).hexdigest()[:16]


def signing_statement(entry_hash: str, kid: str) -> bytes:
    """The bytes an entry's Ed25519 signature covers (JCS, RFC 8785)."""
    stmt = {"alg": "Ed25519", "entry_hash": entry_hash, "kid": kid, "spec": SIG_SPEC}
    return json.dumps(stmt, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()


def _verify_signature(public_key: bytes, kid: str, entry_hash: str, signature: str) -> str | None:
    """``None`` if valid, else the failure reason."""
    from cryptography.exceptions import InvalidSignature
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey

    if not signature.startswith(SIG_PREFIX):
        return "bad_signature"
    parts = signature[len(SIG_PREFIX) :].split(":")
    if len(parts) != 2:
        return "bad_signature"
    sig_kid, sig_hex = parts
    if sig_kid != kid:
        return "wrong_key"
    if not _HEX128.fullmatch(sig_hex):
        return "bad_signature"
    raw = bytes.fromhex(sig_hex)
    if _weak_point(raw[:32]):
        return "bad_signature"
    try:
        Ed25519PublicKey.from_public_bytes(public_key).verify(raw, signing_statement(entry_hash, kid))
    except InvalidSignature:
        return "bad_signature"
    return None


def verify_entries(entries: Sequence[Mapping[str, Any]], public_key: bytes, *, linked: bool = True) -> None:
    """Verify every entry's hash and Ed25519 signature; raise :class:`ChainError`
    at the first failure.

    ``linked=True`` also checks that each entry's ``prev_hash`` is the previous
    entry's ``entry_hash`` (pass the entries oldest first, see
    :func:`chain_order`). The chain spans every tenant, so a single tenant's
    slice of a shared log is not contiguous: verify it with ``linked=False``.
    """
    _check_public_key(public_key)
    kid = key_id(public_key)
    prev: str | None = None
    for index, entry in enumerate(entries):
        entry_hash = entry.get("entry_hash")
        if not entry_hash:
            raise ChainError(index, "unsealed")
        signature = entry.get("signature")
        if not signature:
            raise ChainError(index, "unsigned")
        for key in ("entry_hash", "signature", "prev_hash"):
            value = entry.get(key)
            if value is not None and not isinstance(value, str):
                raise ChainError(index, "malformed", f"{key} is not a string")
        if linked and index > 0 and entry.get("prev_hash") != prev:
            raise ChainError(index, "broken_link")
        try:
            recomputed = entry_content_hash(entry, entry.get("prev_hash"))
        except (KeyError, ValueError, TypeError) as e:
            raise ChainError(index, "malformed", str(e)) from e
        if recomputed != entry_hash:
            raise ChainError(index, "hash_mismatch")
        reason = _verify_signature(public_key, kid, entry_hash, signature)
        if reason:
            raise ChainError(index, reason)
        prev = entry_hash


def chain_order(entries: Iterable[Mapping[str, Any]]) -> list[Mapping[str, Any]]:
    """Order entries oldest first by following their ``prev_hash`` links.

    Raises :class:`ChainError` (``broken_link``) unless the entries form one
    unbroken run: one entry whose predecessor is not in the set, and every
    other entry's predecessor exactly once.
    """
    items = list(entries)
    by_prev: dict[str | None, Mapping[str, Any]] = {}
    hashes = {e.get("entry_hash") for e in items}
    for i, e in enumerate(items):
        p = e.get("prev_hash")
        if p in by_prev:
            raise ChainError(i, "broken_link", "two entries name the same predecessor")
        by_prev[p] = e
    starts = [e for e in items if e.get("prev_hash") not in hashes]
    if len(starts) != 1:
        raise ChainError(0, "broken_link", f"{len(starts)} chain starts in the set")
    ordered = [starts[0]]
    while len(ordered) < len(items):
        nxt = by_prev.get(ordered[-1].get("entry_hash"))
        if nxt is None:
            raise ChainError(len(ordered), "broken_link")
        ordered.append(nxt)
    return ordered


def public_key_from_did_document(doc: Mapping[str, Any]) -> bytes:
    """The audit signing key a runtime publishes in ``/.well-known/did.json``
    (its ``#audit-key`` verification method, a JWK with ``crv: Ed25519``)."""
    import base64

    for vm in doc.get("verificationMethod") or []:
        if not str(vm.get("id", "")).endswith("#audit-key"):
            continue
        jwk = vm.get("publicKeyJwk") or {}
        if jwk.get("kty") != "OKP" or jwk.get("crv") != "Ed25519" or "x" not in jwk:
            raise ValueError("the audit key is not an Ed25519 JWK")
        x = jwk["x"]
        raw = base64.urlsafe_b64decode(x + "=" * (-len(x) % 4))
        _check_public_key(raw)
        return raw
    raise ValueError("the DID document publishes no audit key (is the runtime in ed25519 mode?)")
