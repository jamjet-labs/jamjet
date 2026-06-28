"""Policy resolver: map a PolicyRef to the model allowlist (and other rules).

T3-6: replaces the static ``ModelAllowlistMiddleware(None)`` allow-all with a
real policy-derived set fed into the seam middleware chain.

Design rules
------------
* ``policy=None``   -> allow-all (``None`` allowlist).  The documented v1 default.
  The *other* governance defaults (audit, PII, budget) are still active; the
  model layer is simply uncapped.
* ``policy`` is a dict -> read ``model_allowlist`` directly; unknown keys are
  passed through (the IR compiler validates them separately).
* ``policy`` is a str -> resolved against :data:`BUILT_IN_POLICIES`.  An
  **unknown** named policy raises ``ValueError`` — it never silently allows
  everything, because a misconfigured policy name that degrades to allow-all is
  worse than the hard error that surfaces the typo immediately.

The full jamjet-policy DSL (YAML/JSON loaded from a registry or URL) is a
follow-up (F-t3-policy-dsl).  Built-in named policies cover the common cases so
the string form is immediately useful without that follow-up.

Built-in named policies
-----------------------
``"strict"``
    Anthropic-only model calls.  A conservative starting point for agents that
    should never route to third-party providers.  The allowlist contains the
    ``"anthropic"`` provider string so it works for every Anthropic model
    (checked against ``ModelRef.provider`` in ``ModelAllowlistMiddleware``).

``"open"``
    Explicit alias for allow-all.  Equivalent to omitting ``policy=`` but makes
    the intent legible in code.
"""

from __future__ import annotations

from typing import Any

# ---------------------------------------------------------------------------
# Built-in named-policy table
# ---------------------------------------------------------------------------

# Each entry is the *rules dict* for that policy — same shape as the inline
# dict a developer can pass to ``Agent(policy={...})``.  The IR compiler and the
# allowlist resolver both read from here so the string and dict forms produce
# identical results.
BUILT_IN_POLICIES: dict[str, dict[str, Any]] = {
    "strict": {
        # Only Anthropic provider models allowed.  Checked against
        # ModelRef.provider (e.g. "anthropic") so all Anthropic model names pass.
        "model_allowlist": ["anthropic"],
        "blocked_tools": [],
        "require_approval_for": [],
    },
    "open": {
        # Explicit allow-all — every model allowed, no tools blocked.
        "model_allowlist": [],
        "blocked_tools": [],
        "require_approval_for": [],
    },
}


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------


def resolve_named_policy(name: str) -> dict[str, Any]:
    """Resolve a named policy string to its rules dict.

    Raises ``ValueError`` for unknown names — never silently falls back to
    allow-all, because a mis-typed policy name degrades to no governance.
    """
    try:
        return BUILT_IN_POLICIES[name]
    except KeyError:
        known = sorted(BUILT_IN_POLICIES)
        raise ValueError(
            f"Unknown named policy {name!r}. "
            f"Built-in policies: {known!r}. "
            "Register custom policies via the jamjet-policy package "
            "(follow-up F-t3-policy-dsl)."
        ) from None


def resolve_policy_allowlist(policy: str | dict | None) -> set[str] | None:
    """Return the model allowlist derived from *policy*, or ``None`` for allow-all.

    Parameters
    ----------
    policy:
        ``None``   -> ``None`` (allow-all; the documented v1 default).
        ``dict``   -> read ``"model_allowlist"`` key; empty/absent -> ``None``.
        ``str``    -> resolve from :data:`BUILT_IN_POLICIES`; raises on unknown.

    Returns
    -------
    ``set[str] | None``
        The set fed to ``ModelAllowlistMiddleware``; ``None`` means allow-all.
        Each element can be a provider string (``"anthropic"``) or a full litellm
        model string (``"anthropic/claude-sonnet-4-6"``).

    Raises
    ------
    ``ValueError``
        When *policy* is a string that is not in :data:`BUILT_IN_POLICIES` and
        no external registry resolves it.  This is a configuration error — it
        never silently degrades to allow-all.
    """
    if policy is None:
        return None

    if isinstance(policy, dict):
        al = policy.get("model_allowlist")
        if not al:
            return None  # dict policy with no model_allowlist -> allow-all for models
        return set(al)

    if isinstance(policy, str):
        rules = resolve_named_policy(policy)  # raises ValueError for unknown names
        al = rules.get("model_allowlist")
        if not al:
            return None
        return set(al)

    raise TypeError(f"policy must be str, dict, or None — got {type(policy).__name__!r}")
