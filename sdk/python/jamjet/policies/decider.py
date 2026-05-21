"""Policy deciders for :func:`jamjet.gate`.

A *decider* is a callable that takes the call-site arguments and returns a
:class:`PolicyOutcome`. The receipt emitter binds the outcome into the
AgentBoundary v0.1 Action Receipt verbatim.

This module ships one trivial decider (:func:`static_decider`). Real policy
configuration — YAML files, hosted policy via JamJet Cloud — is added in
follow-up work; the decorator's design lets users plug their own decider
without touching the SDK.
"""

from __future__ import annotations

from collections.abc import Callable, Mapping
from dataclasses import dataclass
from typing import Any, Literal

PolicyDecision = Literal["allow", "require-approval", "deny", "escalate"]


@dataclass(frozen=True)
class PolicyOutcome:
    """The result of a policy check.

    Fields mirror the AgentBoundary v0.1 ``policy`` block:
    :attr:`name` and :attr:`version` end up in the emitted receipt; :attr:`reason`
    is logged but not put on the wire.
    """

    decision: PolicyDecision
    name: str = "jamjet.gate.default"
    version: str = "1"
    reason: str = ""


PolicyDecider = Callable[[Mapping[str, Any]], PolicyOutcome]
"""A policy decider takes the bound-args mapping and returns an outcome."""


def static_decider(
    decision: PolicyDecision,
    *,
    name: str = "jamjet.gate.default",
    version: str = "1",
    reason: str = "",
) -> PolicyDecider:
    """Return a decider that always emits the same outcome.

    Useful for testing and for callers who want the simplest possible
    auto-allow behaviour: ``static_decider("allow")``.
    """

    outcome = PolicyOutcome(decision=decision, name=name, version=version, reason=reason)

    def _decide(_args: Mapping[str, Any]) -> PolicyOutcome:
        return outcome

    return _decide
