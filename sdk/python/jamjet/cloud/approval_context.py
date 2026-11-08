"""What a reviewer sees in an approval request, and how the request is bound
to the call it approves. Mirrors sdk/typescript/packages/cloud/src/approval-context.ts.
"""

from __future__ import annotations

import hashlib
import json
import re
from typing import Any

from .redaction import _redact_regex

# Fields whose value is a credential, whatever it looks like. An explicit
# list: a name pattern such as `*_token` would also hide business fields
# (`payee_token`); credentials under other names are caught by shape below.
_SECRET_KEY = re.compile(
    r"^(password|passwd|passphrase|secret|client_secret|api_key|apikey|x-api-key|authorization"
    r"|access_token|refresh_token|id_token|private_key)$",
    re.IGNORECASE,
)

# Credentials recognisable by shape inside any string.
_SECRET_VALUES = [
    re.compile(r"\bBearer\s+[A-Za-z0-9._~+/=-]+", re.IGNORECASE),
    re.compile(r"\bBasic\s+[A-Za-z0-9+/=]{8,}"),
    re.compile(r"\bsk-[A-Za-z0-9_-]{8,}"),
    re.compile(r"\bjj_[A-Za-z0-9_-]{8,}"),
    re.compile(r"\bAKIA[0-9A-Z]{16}\b"),
    re.compile(r"\bgh[pousr]_[A-Za-z0-9]{20,}"),
    re.compile(r"\bxox[abprs]-[A-Za-z0-9-]{10,}"),
    re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----"),
    re.compile(r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+"),  # JWTs
]

# Long high-entropy runs (32+ characters mixing upper case, lower case and
# digits) are treated as keys too. Hex digests and plain numbers do not mix
# cases, so they are kept.
_HIGH_ENTROPY = re.compile(r"[A-Za-z0-9+/_=-]{32,}")


def _looks_random(run: str) -> bool:
    return any(c.isupper() for c in run) and any(c.islower() for c in run) and any(c.isdigit() for c in run)


# With redaction on: these are masked outright...
_PII_MASKED = ["EMAIL_ADDRESS", "US_SSN", "PHONE_NUMBER", "IP_ADDRESS"]
# ...and account numbers keep their last 4, which a payment reviewer needs.
_ACCOUNT_NUMBERS = [re.compile(r"\b[A-Z]{2}\d{2}[A-Z0-9]{11,30}\b"), re.compile(r"\b(?:\d[\s-]?){12,18}\d\b")]

SECRET = "[SECRET]"


def _mask_string(text: str, pii: bool) -> str:
    for pattern in _SECRET_VALUES:
        text = pattern.sub(SECRET, text)
    text = _HIGH_ENTROPY.sub(lambda m: SECRET if _looks_random(m.group(0)) else m.group(0), text)
    if not pii:
        return text
    text = _redact_regex(text, _PII_MASKED)
    for pattern in _ACCOUNT_NUMBERS:
        text = pattern.sub(lambda m: "****" + re.sub(r"[\s-]", "", m.group(0))[-4:], text)
    return text


def mask_for_approval(value: Any, pii: bool) -> Any:
    """The approval context as sent to Cloud. Credentials are always masked (by
    field name and by shape). With ``pii`` on (redaction enabled), emails,
    SSNs, phone numbers and IPs are masked too, and IBANs and card numbers keep
    their last 4. Amounts, payees and other business fields are kept."""
    if isinstance(value, str):
        return _mask_string(value, pii)
    if isinstance(value, list):
        return [mask_for_approval(v, pii) for v in value]
    if isinstance(value, dict):
        return {
            k: SECRET if _SECRET_KEY.match(str(k)) and v not in (None, "") else mask_for_approval(v, pii)
            for k, v in value.items()
        }
    return value


def parsed_arguments(arguments: Any) -> Any:
    """Tool arguments as an object when they are a JSON string (OpenAI), else as given."""
    if not isinstance(arguments, str):
        return arguments
    try:
        return json.loads(arguments)
    except ValueError:
        return arguments


def arguments_digest(arguments: Any) -> str:
    """SHA-256 (hex) of the canonical JSON of the arguments: keys sorted, no
    whitespace, a JSON string argument parsed first. Sent with the approval and
    re-checked before the approved call is released. Same digest as the
    TypeScript SDK for JSON objects of strings, integers, booleans and nulls."""
    text = json.dumps(
        parsed_arguments(arguments), sort_keys=True, separators=(",", ":"), ensure_ascii=False, default=str
    )
    return hashlib.sha256(text.encode("utf-8")).hexdigest()
