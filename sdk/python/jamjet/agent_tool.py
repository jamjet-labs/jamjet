"""Agent-tool configuration objects and their IR serialization."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any


@dataclass
class AgentToolDef:
    """Configuration for an agent-tool invocation, without executing the agent.

    Attributes:
        agent_uri: Explicit agent URI, or ``"auto"`` for an automatic target.
        mode: Invocation mode: ``"sync"``, ``"streaming"``, or ``"conversational"``.
        description: Human-readable description included in the IR fragment.
        budget: Optional ``max_cost_usd`` and ``max_tokens`` limits, passed through
            without validation. An empty mapping is omitted from the IR.
        max_turns: Conversational turn limit. Serialization uses five when this
            is ``None`` or zero; other modes do not serialize this field.
        timeout_ms: Optional timeout in milliseconds. ``None`` and zero are
            omitted from the IR.
    """

    agent_uri: str
    mode: str
    description: str
    budget: dict[str, Any] | None = None
    max_turns: int | None = None
    timeout_ms: int | None = None

    def to_ir_kind(self) -> dict[str, Any]:
        """Serialize the configuration as an agent-tool node-kind fragment.

        The target is encoded as ``{"auto": True}`` or ``{"explicit": uri}``.
        Conversational mode becomes a mapping containing its turn limit; sync
        and streaming modes remain strings. No agent is invoked or limits
        validated by this method.

        Returns:
            A dictionary containing the node type, target, mode, description,
            and any nonempty budget or nonzero timeout.
        """
        agent_target: dict[str, Any] = {"auto": True} if self.agent_uri == "auto" else {"explicit": self.agent_uri}
        ir: dict[str, Any] = {
            "type": "agent_tool",
            "agent": agent_target,
            "mode": self.mode,
            "description": self.description,
        }
        if self.budget:
            ir["budget"] = self.budget
        if self.timeout_ms:
            ir["timeout_ms"] = self.timeout_ms
        if self.mode == "conversational":
            ir["mode"] = {"conversational": {"max_turns": self.max_turns or 5}}
        return ir


def agent_tool(
    agent: str,
    description: str,
    mode: str = "sync",
    budget: dict[str, Any] | None = None,
    max_turns: int | None = None,
    timeout_ms: int | None = None,
) -> AgentToolDef:
    """Create an agent-tool definition for later IR serialization.

    This stores configuration without calling an agent or validating the URI,
    invocation mode, or limits.

    Args:
        agent: Agent URI, such as ``"jamjet://org/classifier"``, or ``"auto"``
            for an automatic target.
        description: Human-readable description of what this tool does.
        mode: Invocation mode: ``"sync"`` (default), ``"streaming"``, or
            ``"conversational"``.
        budget: Optional limits with keys ``max_cost_usd`` and ``max_tokens``.
            An empty mapping is omitted during serialization.
        max_turns: Turn limit for conversational mode only. ``None`` or zero
            serializes as five turns.
        timeout_ms: Optional timeout in milliseconds. ``None`` or zero is
            omitted during serialization.

    Returns:
        An ``AgentToolDef`` whose ``to_ir_kind()`` method produces a node-kind
        dictionary. The definition itself is not a callable agent.

    Examples:
        >>> tool = agent_tool("jamjet://org/classifier", "Classifies text")
        >>> tool.to_ir_kind()["agent"]
        {'explicit': 'jamjet://org/classifier'}
    """
    return AgentToolDef(
        agent_uri=agent,
        mode=mode,
        description=description,
        budget=budget,
        max_turns=max_turns,
        timeout_ms=timeout_ms,
    )
