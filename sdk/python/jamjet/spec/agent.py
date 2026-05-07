from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field

from jamjet.spec.llm import LLMConfig
from jamjet.spec.memory import MemoryConfig
from jamjet.spec.tool import ToolSpec
from jamjet.spec.version import IR_VERSION


class AgentStrategy(BaseModel):
    """Selects one of the six built-in reasoning strategies (or 'custom')."""

    model_config = ConfigDict(frozen=True, extra="forbid")

    name: Literal[
        "plan-and-execute", "react", "critic",
        "reflection", "consensus", "debate", "custom",
    ]
    config: dict[str, Any] = Field(default_factory=dict)


class AgentSpec(BaseModel):
    """An imperative-construction agent (jamjet.Agent) compiled to IR.

    @DurableAgent classes produce DurableAgentSpec, which extends this.
    """

    model_config = ConfigDict(frozen=True, extra="forbid")

    ir_version: str = IR_VERSION
    kind: Literal["agent"] = "agent"
    name: str
    instructions: str = ""
    llm: LLMConfig
    tools: list[ToolSpec] = Field(default_factory=list)
    memory: MemoryConfig | None = None
    strategy: AgentStrategy = Field(default_factory=lambda: AgentStrategy(name="plan-and-execute"))
    limits: dict[str, Any] = Field(default_factory=dict)
