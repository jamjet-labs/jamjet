"""Per-action signed, hash-chained audit for governed agent runs (T3 / C1).

A governed ``Agent`` produces a tamper-evident audit record for *every action*
it takes (each tool call plus the model turn), not just for approvals. Each
:class:`AuditAction` is sealed into a hash chain (``prev_hash`` -> ``entry_hash``)
and signed with a keyed HMAC-SHA256, mirroring the Rust ``AuditLogEntry`` /
``verify_chain`` primitive (``runtime/audit/src``) so the SDK and engine layers
use the same construction.

This is the in-process, SDK-side audit emitted by ``Agent.run`` /
``Agent.run_durable`` (off any fenced commit path — there is no durability
transaction here, so emission is always best-effort safe). It is ON by default
(``GovernanceConfig.audit``); ``audit=False`` emits nothing. The chain is
attached to :class:`~jamjet.agents.agent.AgentResult.audit` and accepted by
:func:`verify_chain` under the same signing key.

Signing key (honest, fail-closed posture — mirrors the Rust signer, I4)
----------------------------------------------------------------------
The HMAC key is resolved at emission time:

* ``JAMJET_AUDIT_SIGNING_KEY`` set (non-empty)  -> sign with it (secure).
* else ``JAMJET_AUDIT_ALLOW_INSECURE_KEY`` set  -> sign with the *public*
  built-in dev key, with a loud warning. A deliberate, explicit opt-in to an
  insecure key — never a silent default.
* else (no key, no opt-in)                      -> entries are still chained
  (tamper-evident linkage) but left **UNSIGNED** (``signature is None``) with a
  one-time loud warning. :func:`verify_chain` reports such entries as
  ``Unsigned`` — the audit log never *pretends* to be securely signed.

So a signed-audit path with no real key fails loud rather than forging a
trustworthy-looking signature with a publicly-known key.
"""

from __future__ import annotations

import hashlib
import hmac
import json
import os
import uuid
import warnings
from collections.abc import Iterable
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any

# Field separator for canonical content hashing — a control byte that never
# appears in our textual field values, so concatenation is unambiguous (matches
# the Rust entry's FIELD_SEP = 0x1f).
_FIELD_SEP = "\x1f"

# The environment variables that govern audit signing.
SIGNING_KEY_ENV = "JAMJET_AUDIT_SIGNING_KEY"
ALLOW_INSECURE_KEY_ENV = "JAMJET_AUDIT_ALLOW_INSECURE_KEY"

# Public-by-design dev key. Signatures produced with it provide NO security and
# are only usable behind the explicit JAMJET_AUDIT_ALLOW_INSECURE_KEY opt-in.
_DEV_DEFAULT_KEY = b"jamjet-insecure-dev-audit-signing-key-set-JAMJET_AUDIT_SIGNING_KEY-in-prod"

# One-time warning guards so a long run / the whole test suite does not spam.
_warned_unsigned = False
_warned_insecure = False


class ChainError(Exception):
    """A sealed audit chain failed verification.

    ``index`` is the 0-based position of the offending entry; ``reason`` is one
    of ``"unsealed" | "unsigned" | "broken_link" | "hash_mismatch" |
    "bad_signature"`` (mirrors the Rust ``ChainError`` variants).
    """

    def __init__(self, index: int, reason: str) -> None:
        self.index = index
        self.reason = reason
        super().__init__(f"audit chain entry {index} failed: {reason}")


class AuditSigner:
    """Keyed HMAC-SHA256 signer for audit entries.

    Use :func:`resolve_signer` to build one from the environment with the honest
    key-source policy; construct directly with explicit ``key`` bytes in tests
    or callers that hold their own secret. ``key=None`` is the *unsigned* signer:
    :meth:`sign` returns ``None`` and :meth:`verify` returns ``False`` so it can
    never masquerade as secure.
    """

    def __init__(self, key: bytes | None, *, is_dev_key: bool = False) -> None:
        self._key = key
        self.is_dev_key = is_dev_key

    @property
    def can_sign(self) -> bool:
        return self._key is not None

    def sign(self, data: bytes) -> str | None:
        if self._key is None:
            return None
        return hmac.new(self._key, data, hashlib.sha256).hexdigest()

    def verify(self, data: bytes, signature_hex: str | None) -> bool:
        if self._key is None or signature_hex is None:
            return False
        expected = self.sign(data)
        if expected is None:
            return False
        return hmac.compare_digest(expected, signature_hex)


def resolve_signer() -> AuditSigner:
    """Resolve the audit signer from the environment (honest, fail-closed)."""
    global _warned_unsigned, _warned_insecure
    key = os.environ.get(SIGNING_KEY_ENV)
    if key:
        return AuditSigner(key.encode("utf-8"))
    if os.environ.get(ALLOW_INSECURE_KEY_ENV):
        if not _warned_insecure:
            _warned_insecure = True
            warnings.warn(
                f"{SIGNING_KEY_ENV} is not set; signing the agent audit log with the "
                f"BUILT-IN INSECURE DEV KEY because {ALLOW_INSECURE_KEY_ENV} is set. "
                "These signatures provide NO tamper-resistance against an attacker. "
                f"Provision {SIGNING_KEY_ENV} with a real secret in production.",
                UserWarning,
                stacklevel=2,
            )
        return AuditSigner(_DEV_DEFAULT_KEY, is_dev_key=True)
    if not _warned_unsigned:
        _warned_unsigned = True
        warnings.warn(
            f"{SIGNING_KEY_ENV} is not set; agent audit entries will be hash-chained "
            "but left UNSIGNED (verification reports them as unsigned). Set "
            f"{SIGNING_KEY_ENV} to a real secret, or {ALLOW_INSECURE_KEY_ENV}=1 to "
            "explicitly opt into the insecure built-in dev key.",
            UserWarning,
            stacklevel=2,
        )
    return AuditSigner(None)


@dataclass
class AuditAction:
    """One immutable, sealed audit record for a single governed action."""

    execution_id: str
    sequence: int
    action_type: str  # "tool_call" | "agent_turn"
    actor_id: str
    agent: str
    model: str
    decision: str = "allow"
    actor_type: str = "agent"
    tool: str | None = None
    arguments_hash: str | None = None
    result_hash: str | None = None
    id: str = field(default_factory=lambda: str(uuid.uuid4()))
    created_at: str = field(default_factory=lambda: datetime.now(UTC).isoformat())
    prev_hash: str | None = None
    entry_hash: str | None = None
    signature: str | None = None

    def content_hash(self, prev_hash: str | None) -> str:
        """SHA-256 over the immutable content fields chained onto ``prev_hash``.

        Covers every field except the derived ``entry_hash`` / ``signature``,
        plus ``prev_hash`` so altering any past entry (or re-linking the chain)
        breaks verification from that point forward.
        """
        parts = [
            self.id,
            self.execution_id,
            str(self.sequence),
            self.action_type,
            self.actor_id,
            self.actor_type,
            self.agent,
            self.model,
            self.decision,
            self.tool or "",
            self.arguments_hash or "",
            self.result_hash or "",
            self.created_at,
            prev_hash or "",
        ]
        # Coerce defensively to str so a stray non-string field (e.g. a mocked
        # execution_id) hashes deterministically instead of raising.
        digest = hashlib.sha256(_FIELD_SEP.join(str(p) for p in parts).encode("utf-8")).hexdigest()
        return digest

    def seal(self, prev_hash: str | None, signer: AuditSigner) -> None:
        """Record ``prev_hash``, compute ``entry_hash``, then sign it.

        With an unsigned signer (no key configured) ``signature`` stays ``None``
        — the entry is chained but honestly marked unsigned.
        """
        h = self.content_hash(prev_hash)
        self.entry_hash = h
        self.prev_hash = prev_hash
        self.signature = signer.sign(h.encode("utf-8"))


def _hash_value(value: Any) -> str:
    """A stable SHA-256 of an arbitrary JSON-able value (for args/result refs)."""
    try:
        blob = json.dumps(value, sort_keys=True, default=str)
    except (TypeError, ValueError):
        blob = repr(value)
    return hashlib.sha256(blob.encode("utf-8")).hexdigest()


def build_action_chain(
    *,
    agent_name: str,
    model: str,
    execution_id: str,
    prompt: str,
    output: Any,
    tool_calls: Iterable[dict[str, Any]],
    signer: AuditSigner | None = None,
) -> list[AuditAction]:
    """Build a sealed, chained audit record for one governed agent run.

    Emits one ``tool_call`` action per tool the run invoked, in order, followed
    by one ``agent_turn`` action for the model turn that produced ``output`` —
    so every action the agent took is audited, not just approvals. The chain is
    sealed and signed with ``signer`` (defaults to :func:`resolve_signer`).
    """
    if signer is None:
        signer = resolve_signer()

    actor_id = _default_actor_id()
    actions: list[AuditAction] = []
    seq = 0
    for tc in tool_calls:
        actions.append(
            AuditAction(
                execution_id=execution_id,
                sequence=seq,
                action_type="tool_call",
                actor_id=actor_id,
                agent=agent_name,
                model=model,
                tool=tc.get("tool"),
                arguments_hash=_hash_value(tc.get("input")),
                result_hash=_hash_value(tc.get("output")),
            )
        )
        seq += 1
    actions.append(
        AuditAction(
            execution_id=execution_id,
            sequence=seq,
            action_type="agent_turn",
            actor_id=actor_id,
            agent=agent_name,
            model=model,
            arguments_hash=_hash_value(prompt),
            result_hash=_hash_value("" if output is None else output),
        )
    )

    prev: str | None = None
    for action in actions:
        action.seal(prev, signer)
        prev = action.entry_hash
    return actions


def verify_chain(entries: list[AuditAction], signer: AuditSigner) -> None:
    """Re-walk a sealed audit chain; raise :class:`ChainError` on any deviation.

    Checks, for each entry in order: it is sealed + signed, its ``prev_hash``
    links to the previous entry's ``entry_hash`` (``None`` at genesis), its
    recomputed content hash matches the recorded ``entry_hash`` (tamper check),
    and its signature verifies under ``signer`` (forgery check). Mirrors the
    Rust ``verify_chain``.
    """
    prev: str | None = None
    for index, entry in enumerate(entries):
        if entry.entry_hash is None:
            raise ChainError(index, "unsealed")
        if entry.signature is None:
            raise ChainError(index, "unsigned")
        if entry.prev_hash != prev:
            raise ChainError(index, "broken_link")
        if entry.content_hash(entry.prev_hash) != entry.entry_hash:
            raise ChainError(index, "hash_mismatch")
        if not signer.verify(entry.entry_hash.encode("utf-8"), entry.signature):
            raise ChainError(index, "bad_signature")
        prev = entry.entry_hash


def _default_actor_id() -> str:
    return f"agent:jamjet:pid/{os.getpid()}"


__all__ = [
    "AuditAction",
    "AuditSigner",
    "ChainError",
    "build_action_chain",
    "resolve_signer",
    "verify_chain",
]
