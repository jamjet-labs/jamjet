"""Verify jamjet-cloud audit export packages.

Used both as a library (``verify_package(...)``) and via the
``jamjet-cloud audit-verify`` CLI subcommand.
"""
from __future__ import annotations

import base64
import binascii
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
    try:
        bundle = package_path.read_bytes()
    except OSError as e:
        return VerifyResult(False, "", f"could not read package: {e}")
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

    try:
        bundle_json = json.loads(bundle)
    except (json.JSONDecodeError, UnicodeDecodeError) as e:
        return VerifyResult(False, digest_hex, f"bundle is not valid JSON: {e}")
    if not isinstance(bundle_json, dict):
        return VerifyResult(False, digest_hex, "bundle must be a JSON object")
    project_id = bundle_json.get("project", {}).get("id")
    if not project_id:
        return VerifyResult(False, digest_hex, "bundle missing project.id")

    try:
        keys_resp = httpx.get(
            f"{api_url}/.well-known/jamjet-audit-key.json",
            params={"project_id": project_id},
            timeout=10,
        )
        keys_resp.raise_for_status()
        keys = keys_resp.json()
        if not isinstance(keys, list):
            return VerifyResult(False, digest_hex, f"well-known endpoint returned unexpected shape: {type(keys).__name__}")
    except httpx.HTTPError as e:
        return VerifyResult(False, digest_hex, f"could not fetch public key: {e}")
    except json.JSONDecodeError as e:
        return VerifyResult(False, digest_hex, f"well-known response not JSON: {e}")
    matching = [k for k in keys if isinstance(k, dict) and k.get("key_id") == key_id]
    if not matching:
        return VerifyResult(False, digest_hex, f"key_id {key_id} not in published keys")
    public_key_b64 = matching[0].get("public_key_b64")
    if not public_key_b64:
        return VerifyResult(False, digest_hex, f"key_id {key_id} missing public_key_b64")
    try:
        pk_bytes = base64.b64decode(public_key_b64, validate=True)
        sig = base64.b64decode(sig_b64, validate=True)
    except (binascii.Error, ValueError, TypeError) as e:
        return VerifyResult(False, digest_hex, f"invalid base64 encoding: {e}")
    result = verify_package(bundle, sig, pk_bytes)
    result.key_id = key_id
    return result
