"""Top-level entry points: run() / resume() / deploy()."""

from __future__ import annotations

import os
from typing import Any

from jamjet.runtime.local import LocalRuntime
from jamjet.runtime.types import Runtime, RuntimeResult, Scope
from jamjet.spec import AgentSpec, WorkflowSpec


def _resolve_spec(target: Any) -> AgentSpec | WorkflowSpec:
    spec: Any = getattr(target, "__jamjet_spec__", None)
    if spec is not None:
        if not isinstance(spec, (AgentSpec, WorkflowSpec)):
            raise TypeError(f"__jamjet_spec__ on {target!r} is not an AgentSpec or WorkflowSpec.")
        return spec
    if isinstance(target, (AgentSpec, WorkflowSpec)):
        return target
    raise TypeError(
        f"Cannot resolve spec from {target!r}. Pass a @DurableAgent class, "
        "a @workflow function, or an AgentSpec/WorkflowSpec directly."
    )


def _select_runtime(target_runtime: str | None = None) -> Runtime:
    name = target_runtime or os.environ.get("JAMJET_RUNTIME", "local")
    if name == "local":
        return LocalRuntime()
    from jamjet.runtime.stub import CloudRuntime, JavaRuntime, RustRuntime

    runtimes: dict[str, Runtime] = {
        "cloud": CloudRuntime(),
        "java": JavaRuntime(),
        "rust": RustRuntime(),
    }
    rt = runtimes.get(name)
    if rt is None:
        raise ValueError(f"Unknown runtime {name!r}. Valid: local, cloud, java, rust.")
    return rt


async def run(
    target: Any,
    input: Any = None,
    *,
    execution_id: str | None = None,
    scope: Scope | None = None,
    target_runtime: str | None = None,
) -> RuntimeResult:
    """Execute a @DurableAgent class, @workflow function, or AgentSpec."""
    spec = _resolve_spec(target)
    rt = _select_runtime(target_runtime)
    return await rt.execute(spec, input, execution_id=execution_id, scope=scope)


async def resume(
    target: Any,
    execution_id: str,
    *,
    governance: Any | None = None,
    target_runtime: str | None = None,
) -> RuntimeResult:
    """Resume a durable execution, keeping its governance enforced (M6 parity).

    Pass ``governance`` (e.g. ``agent.governance``) so a resumed in-process run
    keeps the SAME budget / allowlist / PII enforcement as the original run
    instead of silently resuming ungoverned.
    """
    spec = _resolve_spec(target)
    rt = _select_runtime(target_runtime)
    return await rt.resume(spec, execution_id, governance=governance)


async def deploy(target: Any, *, runtime: str | None = None, **kwargs: Any) -> Any:
    """Deprecated — use :meth:`jamjet.Agent.deploy` / :meth:`jamjet.Team.deploy`.

    The old top-level ``deploy()`` was a Phase-5 ``NotImplementedError`` stub. The
    real deploy surface now lives on the ADK objects: ``Agent.deploy(runtime=...)``
    and ``Team.deploy(...)`` ship the compiled IR to a ``jamjet-server`` engine
    (local / self-host / cloud) over the working ``JamjetClient`` path (Track 7a).

    For backward compatibility this delegates to ``target.deploy(...)`` when
    *target* is an :class:`~jamjet.agents.agent.Agent` or a team (anything with a
    ``deploy`` method), emitting a :class:`DeprecationWarning`. For any other
    target (a ``@DurableAgent`` class / ``@workflow`` function / spec, which never
    had a working deploy) it raises a clear error pointing at the new API — it
    never silently no-ops and never raises the old "lands in Phase 5" stub.
    """
    import warnings

    deploy_method = getattr(target, "deploy", None)
    if callable(deploy_method):
        warnings.warn(
            "jamjet.deploy(target) is deprecated; call target.deploy(runtime=...) "
            "directly (Agent.deploy / Team.deploy).",
            DeprecationWarning,
            stacklevel=2,
        )
        return await deploy_method(runtime=runtime, **kwargs)
    raise TypeError(
        f"Cannot deploy {target!r}: the top-level deploy() stub is removed. Use "
        "Agent.deploy(runtime=...) on a jamjet.Agent (or Team.deploy(...)). 'cloud' "
        "ships the IR to YOUR hosted jamjet-server engine plus JamJet Cloud "
        "governance; it is not a managed execution cell."
    )
