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


async def deploy(target: Any, *, runtime: str = "cloud") -> None:
    """Phase 5 — dispatch to remote runtime. Stub for now."""
    raise NotImplementedError("deploy() lands in Phase 5.")
