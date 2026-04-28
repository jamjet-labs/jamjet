"""JamJet Cloud SDK — add governance to any AI agent in 2 lines."""

from __future__ import annotations

import threading
from typing import Any

from .agent import Agent, agent, get_current_agent, set_default_agent
from .approvals import request_approval as _request_approval
from .budget import set_budget as _set_budget
from .config import get_config, set_config
from .events import init_queue
from .patcher import patch_all, unpatch_all
from .policy import get_evaluator
from .propagation import extract_headers, inject_headers
from .redaction import redact
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
    "redact",
    "require_approval",
    "set_user_context",
    "trace",
    "unpatch_all",
    "user_context",
]


def configure(
    api_key: str,
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
    )
    init_queue(flush_interval=flush_interval, flush_size=flush_size)

    # Seed the default agent. Even users who don't call jamjet.agent() get
    # named attribution — every span belongs to either the configured default
    # name or the literal "default" agent.
    default_name = agent if agent and agent.strip() else "default"
    set_default_agent(Agent(name=default_name))

    # Process-wide context (environment, release_version) for every span.
    set_process_context(environment=environment, release_version=release_version)

    if redact:
        from .redaction import configure as _redact_cfg
        _redact_cfg(enabled=True, pii_types=redact_types)

    if auto_patch:
        patch_all()

    # Sync policies from server in background
    cfg = get_config()
    t = threading.Thread(target=_sync_policies, args=(cfg.api_key, cfg.api_url, project), daemon=True)
    t.start()


def _sync_policies(api_key: str | None, api_url: str, project: str) -> None:
    """Fetch policies from the server and load them into the local evaluator."""
    if not api_key:
        return
    try:
        import httpx

        resp = httpx.get(
            f"{api_url}/v1/projects/{project}/policies",
            headers={"Authorization": f"Bearer {api_key}"},
            timeout=10,
        )
        if resp.status_code == 200:
            evaluator = get_evaluator()
            for rule in resp.json().get("rules", []):
                evaluator.add(rule["action"], rule["pattern"])
    except Exception:
        pass  # Graceful degradation — local policies still work


def policy(action: str, tools: str) -> None:
    """Add a local policy rule.

    Args:
        action: 'block', 'allow', or 'require_approval'.
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
    timeout_seconds: float = 3600,
) -> str:
    """Request human-in-the-loop approval. Blocks until approved, rejected, or timeout.

    Returns the approval_id on success.
    """
    cfg = get_config()
    if not cfg.api_key:
        raise RuntimeError("JamJet Cloud not configured. Call jamjet.configure() first.")
    return _request_approval(
        api_key=cfg.api_key,
        api_url=cfg.api_url,
        action=action,
        context=context,
        timeout_seconds=timeout_seconds,
    )
