"""JamJet Cloud SDK — add governance to any AI agent in 2 lines."""

from __future__ import annotations

import threading
from typing import Any

from .agent import Agent, agent, get_current_agent, set_default_agent
from .approvals import request_approval as _request_approval
from .budget import set_budget as _set_budget
from .config import get_config, set_config
from .events import init_queue
from .outcomes import record_outcome as _record_outcome
from .patcher import patch_all, unpatch_all
from .policy import get_evaluator
from .propagation import extract_headers, inject_headers
from .redaction import redact
from .remote_policy import RemotePolicies
from .trace import trace
from .user_context import set_process_context, set_user_context, user_context

__all__ = [
    "Agent",
    "agent",
    "budget",
    "configure",
    "extract_headers",
    "get_current_agent",
    "inject_headers",
    "patch_all",
    "policy",
    "record_outcome",
    "redact",
    "refresh_policies",
    "require_approval",
    "set_user_context",
    "trace",
    "unpatch_all",
    "user_context",
]


def configure(
    api_key: str | None = None,
    project: str = "default",
    agent: str | None = None,
    environment: str | None = None,
    release_version: str | None = None,
    capture_io: bool = False,
    auto_patch: bool = True,
    flush_interval: float = 5.0,
    flush_size: int = 50,
    api_url: str = "https://api.jamjet.dev",
    redact: bool = False,
    redact_types: list[str] | None = None,
    policy_path: str | None = None,
    telemetry: bool = True,
    approval_timeout_seconds: float = 3600.0,
    remote_policies: bool | None = None,
    remote_policy_refresh_seconds: float = 60.0,
) -> None:
    """Initialize the JamJet Cloud SDK.

    Sets global config, starts the event queue, optionally monkey-patches the
    OpenAI / Anthropic SDKs, and seeds the default agent identity.

    Args:
        api_key: ``jj_...`` key from ``app.jamjet.dev``.
        project: logical grouping of traces. One project per service is typical.
        agent: optional default agent name for every span this process emits.
            If omitted, spans are tagged ``default`` until an explicit
            ``with jamjet.agent("name"):`` scope or ``@jamjet.agent("name")``
            decorator overrides.
        capture_io: when True, captures full prompt/response payloads (off by
            default for privacy).
        policy_path: optional path to a ``policy.yaml`` file. When provided the
            file is loaded and the Phase 1 middleware chain is built from any
            ``redact`` rules found in the policy. When omitted (or when the
            ``JAMJET_MIDDLEWARE_ENABLED`` env-var is not set to ``"1"``) the
            chain remains empty and the patcher is byte-identical to the
            pre-Phase-1 path.
        telemetry: set to ``False`` to suppress the background Cloud policy
            sync (see ``remote_policies``). Primarily useful in tests.
        approval_timeout_seconds: how long a tool call matched by a
            ``require_approval`` rule waits for a decision in JamJet Cloud
            before raising ``JamJetApprovalTimeout``. Sent to Cloud as the
            approval's deadline, clamped to 1 minute to 24 hours; Cloud
            deployments since jamjet-cloud #87 refuse decisions after it.
            Default 1 hour.
        remote_policies: apply the block and require_approval rules managed in
            JamJet Cloud, alongside local ``policy()`` rules (the stricter
            decision wins). ``True`` loads them before ``configure()`` returns
            and fails closed: if they cannot be loaded, the failure is logged
            and every tool call is blocked until a refresh succeeds. ``None``
            (default) loads them in the background when ``telemetry`` is on; a
            failure is logged as a warning and only local rules apply.
            ``False`` skips Cloud rules.
        remote_policy_refresh_seconds: how often Cloud rules are reloaded
            (plus or minus 10% jitter). A failed reload keeps the last rules
            that loaded. ``0`` turns the timer off; :func:`refresh_policies`
            reloads on demand.
    """
    set_config(
        api_key=api_key,
        project=project,
        capture_io=capture_io,
        auto_patch=auto_patch,
        flush_interval=flush_interval,
        flush_size=flush_size,
        api_url=api_url,
        enabled=True,
        approval_timeout_seconds=approval_timeout_seconds,
    )
    init_queue(flush_interval=flush_interval, flush_size=flush_size)

    # Seed the default agent. Even users who don't call jamjet.agent() get
    # named attribution — every span belongs to either the configured default
    # name or the literal "default" agent.
    default_name = agent if agent and agent.strip() else "default"
    set_default_agent(Agent(name=default_name))

    # Process-wide context (environment, release_version) for every span.
    set_process_context(environment=environment, release_version=release_version)

    from .redaction import configure as _redact_cfg

    _redact_cfg(enabled=redact, pii_types=redact_types)

    # Phase 1: load a local policy.yaml and build the middleware chain from it.
    # Empty when policy_path is absent, the feature flag is off, or no
    # middleware-eligible rules exist; in all cases the patcher behaviour is
    # byte-identical to the pre-Phase-1 path.
    if policy_path is not None:
        import yaml  # PyYAML is a direct dependency of jamjet; safe to import here

        with open(policy_path) as _f:
            _policy_dict: dict = yaml.safe_load(_f) or {}

        from .middleware import build_chain as _build_chain
        from .patcher import _runtime_state as _rs

        _rs().middleware_chain = _build_chain(_policy_dict)

    if auto_patch:
        patch_all()

    # Cloud-managed policies. See remote_policy.py for how they are applied.
    evaluator = get_evaluator()
    if evaluator.remote is not None:
        evaluator.remote.stop()
    evaluator.remote = None
    cfg = get_config()
    if remote_policies is True:
        if not cfg.api_key:
            raise ValueError("remote_policies=True needs an api_key to read policies from JamJet Cloud.")
        evaluator.remote = RemotePolicies(cfg.api_key, cfg.api_url, environment, required=True)
        evaluator.remote.load()
        evaluator.remote.start_refresh(remote_policy_refresh_seconds)
    elif remote_policies is None and telemetry and cfg.api_key:
        evaluator.remote = RemotePolicies(cfg.api_key, cfg.api_url, environment, required=False)
        remote = evaluator.remote

        def first_load_then_refresh() -> None:
            remote.load()
            remote.start_refresh(remote_policy_refresh_seconds)

        threading.Thread(target=first_load_then_refresh, daemon=True, name="jamjet-policy-sync").start()


def refresh_policies() -> bool:
    """Reload the Cloud-managed policies (see ``configure(remote_policies=...)``).

    Returns True when they loaded. A failure is logged; with
    ``remote_policies=True`` every tool call stays blocked until a refresh
    succeeds. Returns False when remote policies are not configured.
    """
    remote = get_evaluator().remote
    if remote is None:
        return False
    return remote.load()


def policy(action: str, tools: str) -> None:
    """Add a local policy rule.

    ``block`` strips matching tools from requests and raises
    ``JamJetPolicyBlocked`` if the model calls one anyway. ``require_approval``
    leaves the tool available; when the model calls it, the SDK opens an
    approval in JamJet Cloud (tool, call id, arguments, model, rule) and waits
    for the decision before returning the response. A rejection raises
    ``JamJetApprovalRejected``; no decision within ``approval_timeout_seconds``,
    or no way to reach Cloud, raises ``JamJetApprovalTimeout``.

    Args:
        action: 'block', 'allow', 'require_approval' or 'audit'.
        tools: Glob pattern for tool names (e.g. 'payments.*').
    """
    evaluator = get_evaluator()
    evaluator.add(action, tools)


def budget(max_cost_usd: float) -> None:
    """Set a cost ceiling for the current process."""
    _set_budget(max_cost_usd)


def require_approval(
    action: str,
    context: dict[str, Any] | None = None,
    timeout_seconds: float | None = None,
) -> str:
    """Request human-in-the-loop approval. Blocks until approved, rejected, or timeout.

    ``timeout_seconds`` defaults to the ``approval_timeout_seconds`` given to
    :func:`configure` (1 hour unless set). Returns the approval_id on success.
    """
    cfg = get_config()
    if not cfg.api_key:
        raise RuntimeError("JamJet Cloud not configured. Call jamjet.configure() first.")
    return _request_approval(
        api_key=cfg.api_key,
        api_url=cfg.api_url,
        action=action,
        context=context,
        timeout_seconds=cfg.approval_timeout_seconds if timeout_seconds is None else timeout_seconds,
        poll_interval=cfg.approval_poll_interval,
    )


def record_outcome(
    trace_id: str,
    outcome: str,
    score: float | None = None,
    metadata: dict[str, Any] | None = None,
) -> None:
    """Record the outcome of a completed trace run.

    Requires the SDK to be configured via :func:`configure`.

    Args:
        trace_id: ID of the trace whose outcome is being recorded.
        outcome:  One of ``'success'``, ``'failure'``, ``'approved'``,
                  ``'rejected'``, ``'resolved'``, or ``'unresolved'``.
        score:    Optional float in [0, 1] representing a quality score.
        metadata: Optional free-form dict attached to the record.

    Raises:
        RuntimeError: When the SDK has not been configured.
        ValueError: When *outcome* is invalid or *score* is out of range.
        httpx.HTTPStatusError: When the API returns a non-2xx response.
    """
    cfg = get_config()
    if not cfg.api_key:
        raise RuntimeError("JamJet Cloud not configured. Call jamjet.configure() first.")
    _record_outcome(
        api_key=cfg.api_key,
        api_url=cfg.api_url,
        trace_id=trace_id,
        outcome=outcome,
        score=score,
        metadata=metadata,
    )
