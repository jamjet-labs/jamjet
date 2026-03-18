from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass
class AgentToolDef:
    """A wrapped agent that can be used as a tool by other agents."""
    agent_uri: str
    mode: str
    description: str
    budget: dict[str, Any] | None = None
    max_turns: int | None = None
    timeout_ms: int | None = None

    def to_ir_kind(self) -> dict[str, Any]:
        """Compile to IR node kind definition."""
        agent_target = (
            {"auto": True} if self.agent_uri == "auto"
            else {"explicit": self.agent_uri}
        )
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
        if self.mode == "conversational" and self.max_turns:
            ir["mode"] = {"conversational": {"max_turns": self.max_turns}}
        return ir


def agent_tool(
    agent: str,
    description: str,
    mode: str = "sync",
    budget: dict[str, Any] | None = None,
    max_turns: int | None = None,
    timeout_ms: int | None = None,
) -> AgentToolDef:
    """Wrap an agent as a callable tool.

    Args:
        agent: Agent URI (e.g., "jamjet://org/classifier") or "auto" for coordinator routing.
        description: Human-readable description of what this tool does.
        mode: Invocation mode — "sync", "streaming", or "conversational".
        budget: Budget constraints (max_cost_usd, max_tokens).
        max_turns: Max turns for conversational mode.
        timeout_ms: Timeout in milliseconds.
    """
    return AgentToolDef(
        agent_uri=agent,
        mode=mode,
        description=description,
        budget=budget,
        max_turns=max_turns,
        timeout_ms=timeout_ms,
    )
