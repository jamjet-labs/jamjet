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
