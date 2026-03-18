"""Node builder classes for the graph builder API."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass
class ModelNode:
    model: str = "default_chat"
    prompt: str | None = None
    output_schema: str | None = None
    system_prompt: str | None = None
    retry_policy: str | None = None
    timeout: str | None = None

    def to_ir_kind(self) -> dict[str, Any]:
        return {
            "type": "model",
            "model_ref": self.model,
            "prompt_ref": self.prompt or "",
            "output_schema": self.output_schema or "",
            "system_prompt": self.system_prompt,
        }


@dataclass
class ToolNode:
    tool_ref: str = ""
    input_mapping: dict[str, str] = field(default_factory=dict)
    output_schema: str | None = None
    retry_policy: str | None = None
    timeout: str | None = None

    def to_ir_kind(self) -> dict[str, Any]:
        return {
            "type": "tool",
            "tool_ref": self.tool_ref,
            "input_mapping": self.input_mapping,
            "output_schema": self.output_schema or "",
        }


@dataclass
class ConditionNode:
    branches: list[dict[str, Any]] = field(default_factory=list)

    def to_ir_kind(self) -> dict[str, Any]:
        return {"type": "condition", "branches": self.branches}


@dataclass
class HumanApprovalNode:
    description: str = "Approval required"
    timeout: str | None = None
    fallback: str | None = None

    def to_ir_kind(self) -> dict[str, Any]:
        from jamjet.workflow.ir_compiler import _parse_timeout

        return {
            "type": "human_approval",
            "description": self.description,
            "timeout_secs": _parse_timeout(self.timeout),
            "fallback_node": self.fallback,
        }


@dataclass
class EvalNode:
    scorers: list[dict[str, Any]] = field(default_factory=list)
    on_fail: str = "halt"
    max_retries: int = 0
    input_expr: str | None = None

    def to_ir_kind(self) -> dict[str, Any]:
        return {
            "type": "eval",
            "scorers": _compile_scorers(self.scorers),
            "on_fail": self.on_fail,
            "max_retries": self.max_retries,
            "input_expr": self.input_expr,
        }


@dataclass
class CoordinatorNode:
    """Dynamic agent routing with structured scoring + LLM tiebreaker."""
    task: str = ""
    required_skills: list[str] = field(default_factory=list)
    output_key: str = "result"
    preferred_skills: list[str] = field(default_factory=list)
    trust_domain: str | None = None
    budget: dict | None = None
    tiebreaker: dict | None = None
    strategy: str = "default"
    weights: dict | None = None
    input_mapping: dict | None = None

    def to_ir_kind(self) -> dict[str, Any]:
        ir = {
            "type": "coordinator",
            "task": self.task,
            "required_skills": self.required_skills,
            "preferred_skills": self.preferred_skills,
            "output_key": self.output_key,
            "strategy": self.strategy,
            "weights": self.weights or {},
            "input_mapping": self.input_mapping or {},
        }
        if self.trust_domain:
            ir["trust_domain"] = self.trust_domain
        if self.budget:
            ir["budget"] = self.budget
        if self.tiebreaker:
            ir["tiebreaker"] = self.tiebreaker
        return ir


@dataclass
class AgentToolNode:
    """Invoke a registered agent as a callable tool."""
    agent: str = ""
    output_key: str = "result"
    mode: str = "sync"
    input_mapping: dict | None = None
    timeout_ms: int | None = None
    budget: dict | None = None

    def to_ir_kind(self) -> dict[str, Any]:
        agent_target = (
            {"auto": True} if self.agent == "auto"
            else {"explicit": self.agent}
        )
        ir = {
            "type": "agent_tool",
            "agent": agent_target,
            "mode": self.mode,
            "output_key": self.output_key,
            "input_mapping": self.input_mapping or {},
        }
        if self.timeout_ms:
            ir["timeout_ms"] = self.timeout_ms
        if self.budget:
            ir["budget"] = self.budget
        return ir


def _compile_scorers(scorers: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Convert SDK scorer dicts to the IR format expected by the Rust executor.

    When a scorer dict has no ``type`` but carries a ``name`` that matches
    a registered custom scorer in the :class:`~jamjet.eval.registry.ScorerRegistry`,
    it is compiled as ``type: "custom"`` with a ``scorer_ref`` pointing to the
    registered name.
    """
    from jamjet.eval.registry import get_scorer_registry

    registry = get_scorer_registry()
    compiled = []
    for s in scorers:
        scorer_type = s.get("type", "")
        if scorer_type == "llm_judge":
            compiled.append(
                {
                    "type": "llm_judge",
                    "model": s.get("model", "default_chat"),
                    "rubric": s.get("rubric", ""),
                    "min_score": s.get("min_score", 3),
                }
            )
        elif scorer_type == "assertion":
            compiled.append(
                {
                    "type": "assertion",
                    "checks": s.get("checks", []),
                }
            )
        elif scorer_type == "latency":
            compiled.append(
                {
                    "type": "latency",
                    "threshold_ms": s.get("threshold_ms", 5000),
                }
            )
        elif scorer_type == "cost":
            compiled.append(
                {
                    "type": "cost",
                    "threshold_usd": s.get("threshold_usd", 1.0),
                }
            )
        elif scorer_type == "custom":
            compiled.append(
                {
                    "type": "custom",
                    "module": s.get("module", ""),
                    "scorer_ref": s.get("scorer_ref", s.get("name", "")),
                    "kwargs": s.get("kwargs", {}),
                }
            )
        else:
            # Check if the scorer name matches a registered custom scorer.
            scorer_name = s.get("name", "")
            if scorer_name and registry.get(scorer_name) is not None:
                compiled.append(
                    {
                        "type": "custom",
                        "scorer_ref": scorer_name,
                        "kwargs": {k: v for k, v in s.items() if k != "name"},
                    }
                )
            else:
                compiled.append(s)
    return compiled
