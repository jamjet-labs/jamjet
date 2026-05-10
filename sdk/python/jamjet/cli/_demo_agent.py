"""
DeterministicDemoAgent — a non-intelligent stub used by jamjet demo commands.

This is NOT an LLM. It returns canned tool plans for each demo scenario so that
the safety enforcement path can run with no API key, no network, no randomness.
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass
class ToolCallPlan:
    tool: str
    arguments: dict[str, object]
    estimated_cost_usd: float = 0.0


class DeterministicDemoAgent:
    """Mock agent. Plans are hardcoded per scenario."""

    def __init__(self, scenario: str) -> None:
        self.scenario = scenario

    def name(self) -> str:
        return "DeterministicDemoAgent (mocked — no real model)"

    def plan_tool_calls(self) -> list[ToolCallPlan]:
        if self.scenario == "unsafe-tool-call":
            return [
                ToolCallPlan(
                    tool="database.delete_all_customers",
                    arguments={"reason": "cleanup old records"},
                )
            ]
        if self.scenario == "approval":
            return [
                ToolCallPlan(
                    tool="payments.refund",
                    arguments={"customer_id": "cust_123", "amount_usd": 499.00},
                )
            ]
        if self.scenario == "budget-cap":
            return [
                ToolCallPlan(tool="search.web", arguments={"q": "step-1"}, estimated_cost_usd=0.02),
                ToolCallPlan(tool="search.web", arguments={"q": "step-2"}, estimated_cost_usd=0.02),
                ToolCallPlan(tool="search.web", arguments={"q": "step-3"}, estimated_cost_usd=0.02),
            ]
        if self.scenario == "mcp-tool-policy":
            return [
                ToolCallPlan(
                    tool="postgres/database.delete_all_customers",
                    arguments={"server": "postgres-mcp", "confirm": True},
                )
            ]
        raise ValueError(f"unknown scenario: {self.scenario}")
