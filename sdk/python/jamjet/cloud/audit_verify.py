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
    pdf_path: Path | None = None,
    otlp_path: Path | None = None,
    siem_splunk_path: Path | None = None,
    siem_datadog_path: Path | None = None,
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

    if not result.ok:
        return result

    expected_digest = digest_hex
    for path, kind, fn in (
        (pdf_path, "pdf",
         lambda p: cross_check_pdf(p, expected_digest)),
        (otlp_path, "otlp",
         lambda p: cross_check_otlp(p, expected_digest)),
        (siem_splunk_path, "siem_splunk",
         lambda p: cross_check_siem_jsonl(p, expected_digest, splunk=True)),
        (siem_datadog_path, "siem_datadog",
         lambda p: cross_check_siem_jsonl(p, expected_digest, splunk=False)),
    ):
        if path is not None:
            err = fn(path)
            if err is not None:
                return VerifyResult(False, expected_digest, err, key_id)

    return result


import re


def cross_check_pdf(pdf_path: Path, expected_bundle_sha256: str) -> str | None:
    """If `pdf_path` is given, extract the embedded bundle_sha256 from the
    PDF cover page (which prints it as text) and assert equality.
    Returns None if OK, or a failure-reason string."""
    try:
        raw = pdf_path.read_bytes()
    except OSError as e:
        return f"could not read pdf: {e}"
    if not raw.startswith(b"%PDF-"):
        return f"{pdf_path} does not look like a PDF (missing %PDF- magic)"
    # The sha256 (64 hex chars) appears in a content stream. typst-pdf
    # may compress streams; decompress to find it.
    if expected_bundle_sha256.encode() in raw:
        return None
    # Try zlib-decompressing each /FlateDecode object naively.
    import zlib
    for chunk in re.findall(rb"stream\r?\n(.*?)\r?\nendstream", raw, re.DOTALL):
        try:
            decompressed = zlib.decompress(chunk)
        except zlib.error:
            continue
        if expected_bundle_sha256.encode() in decompressed:
            return None
    return (f"pdf metadata sha256 not found in {pdf_path.name} "
            f"(expected {expected_bundle_sha256[:16]}…); "
            f"pdf may have been re-rendered or tampered")


def cross_check_otlp(otlp_path: Path, expected_bundle_sha256: str) -> str | None:
    try:
        doc = json.loads(otlp_path.read_text())
    except (OSError, json.JSONDecodeError) as e:
        return f"could not read otlp: {e}"
    if not isinstance(doc, dict):
        return "otlp file is not a JSON object"
    audit = doc.get("_jamjet_audit", {})
    actual = audit.get("bundle_sha256")
    if actual != expected_bundle_sha256:
        return (f"otlp _jamjet_audit.bundle_sha256 = {actual!r} "
                f"does not match canonical bundle digest {expected_bundle_sha256!r}")
    return None


def cross_check_siem_jsonl(siem_path: Path, expected_bundle_sha256: str, *, splunk: bool) -> str | None:
    try:
        raw = siem_path.read_text()
    except OSError as e:
        return f"could not read siem jsonl: {e}"
    field_name = "jj_audit_bundle_sha256"
    for i, line in enumerate(l for l in raw.splitlines() if l.strip()):
        try:
            rec = json.loads(line)
        except json.JSONDecodeError as e:
            return f"siem line {i} not valid JSON: {e}"
        if splunk:
            actual = rec.get("fields", {}).get(field_name)
        else:
            actual = rec.get(field_name)
        if actual != expected_bundle_sha256:
            return (f"siem line {i} {field_name} = {actual!r} "
                    f"does not match canonical bundle digest {expected_bundle_sha256!r}")
    return None
