"""JamJet Cloud SDK — add governance to any AI agent in 2 lines."""

from __future__ import annotations

import threading
from typing import Any

from .approvals import request_approval as _request_approval
from .budget import set_budget as _set_budget
from .config import get_config, set_config
from .events import init_queue
from .patcher import patch_all, unpatch_all
from .policy import get_evaluator
from .trace import trace

__all__ = [
    "configure",
    "policy",
    "budget",
    "require_approval",
    "trace",
]


def configure(
    api_key: str,
    project: str = "default",
    capture_io: bool = False,
    auto_patch: bool = True,
    flush_interval: float = 5.0,
    flush_size: int = 50,
    api_url: str = "https://api.jamjet.dev",
) -> None:
    """Initialize the JamJet Cloud SDK.

    Sets global config, starts the event queue, and optionally monkey-patches
    OpenAI and Anthropic SDKs for automatic capture.
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
