from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field

from jamjet.spec.durability import DurabilityConfig
from jamjet.spec.version import IR_VERSION


class NodeSpec(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")
    id: str
    handler_ref: str
    config: dict[str, Any] = Field(default_factory=dict)


class EdgeSpec(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")
    from_node: str
    to_node: str
    condition: str | None = None


class WorkflowSpec(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")

    ir_version: str = IR_VERSION
    kind: Literal["workflow"] = "workflow"
    name: str
    nodes: list[NodeSpec]
    edges: list[EdgeSpec]
    entry_node: str
    durability: DurabilityConfig = Field(default_factory=DurabilityConfig)
