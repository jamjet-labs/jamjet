"""Verify jamjet-cloud audit export packages.

Used both as a library (``verify_package(...)``) and via the
``jamjet-cloud audit-verify`` CLI subcommand.
"""
from __future__ import annotations

import base64
import hashlib
import json
from dataclasses import dataclass
from pathlib import Path

import httpx


@dataclass
class VerifyResult:
    ok: bool
    digest: str
    reason: str = ""
    key_id: str | None = None


def verify_package(bundle: bytes, signature: bytes, public_key_bytes: bytes) -> VerifyResult:
    """Verify a bundle's Ed25519 signature against a 32-byte raw public key.

    The signed payload is ``sha256(bundle)``; this function recomputes the
    digest and validates the signature against it. Mismatches in length,
    key, or signature integrity all surface as ``ok=False``.
    """
    from cryptography.exceptions import InvalidSignature
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey

    digest_bytes = hashlib.sha256(bundle).digest()
    digest_hex = digest_bytes.hex()
    if len(signature) != 64:
        return VerifyResult(False, digest_hex, "signature wrong length (expected 64 bytes)")
    if len(public_key_bytes) != 32:
        return VerifyResult(False, digest_hex, "public key wrong length (expected 32 bytes)")
    try:
        pk = Ed25519PublicKey.from_public_bytes(public_key_bytes)
        pk.verify(signature, digest_bytes)
        return VerifyResult(True, digest_hex)
    except InvalidSignature:
        return VerifyResult(False, digest_hex, "signature did not match the public key")
    except Exception as e:  # pragma: no cover — defensive
        return VerifyResult(False, digest_hex, f"verify error: {e}")


def verify_from_files(
    package_path: Path,
    metadata_path: Path,
    *,
    api_url: str = "https://api.jamjet.dev",
) -> VerifyResult:
    """Read a package + its POST-response metadata, fetch the published
    public key, and verify.

    metadata_path JSON shape (from `POST /v1/audit/export`):
        {
          "id": "...",
          "sha256_digest": "...",
          "signature_b64": "...",
          "signing_key_id": "...",
          "expires_at": "...",
          "download_urls": {...}
        }
    """
    bundle = package_path.read_bytes()
    digest_hex = hashlib.sha256(bundle).hexdigest()

    try:
        meta = json.loads(metadata_path.read_text())
    except (OSError, json.JSONDecodeError) as e:
        return VerifyResult(False, digest_hex, f"could not read metadata: {e}")

    sig_b64 = meta.get("signature_b64")
    key_id = meta.get("signing_key_id")
    if not sig_b64 or not key_id:
        return VerifyResult(False, digest_hex, "metadata missing signature_b64 or signing_key_id")

    if meta.get("sha256_digest") and meta["sha256_digest"] != digest_hex:
        return VerifyResult(False, digest_hex,
                            f"bundle digest {digest_hex} does not match metadata.sha256_digest {meta['sha256_digest']}")

    project_id = json.loads(bundle).get("project", {}).get("id")
    if not project_id:
        return VerifyResult(False, digest_hex, "bundle missing project.id")

    keys = httpx.get(
        f"{api_url}/.well-known/jamjet-audit-key.json",
        params={"project_id": project_id},
        timeout=10,
    ).json()
    matching = [k for k in keys if k["key_id"] == key_id]
    if not matching:
        return VerifyResult(False, digest_hex, f"key_id {key_id} not in published keys")
    pk_bytes = base64.b64decode(matching[0]["public_key_b64"])
    sig = base64.b64decode(sig_b64)
    result = verify_package(bundle, sig, pk_bytes)
    result.key_id = key_id
    return result
