"""
JamJet Python SDK

The agent-native runtime — built for performance, designed for interoperability,
reliable enough for production.

Quick start:

    from jamjet import Workflow, tool
    from pydantic import BaseModel

    @tool
    async def my_tool(query: str) -> str:
        return f"Result for: {query}"

    workflow = Workflow("my_workflow")

    @workflow.state
    class State(BaseModel):
        query: str
        result: str | None = None

    @workflow.step
    async def run(state: State) -> State:
        r = await my_tool(query=state.query)
        return state.model_copy(update={"result": r})
"""

# isort: off
# ruff: noqa: I001
# Import order below is load-bearing: `from jamjet.decorators import workflow`
# MUST come after `from jamjet.workflow.workflow import Workflow` so the
# `workflow` name in this namespace binds to the decorator, not the subpackage.
from jamjet.agents.agent import Agent, AgentResult
from jamjet.agents.artifacts import ArtifactStore
from jamjet.agents.session import Session, SessionStore
from jamjet.agents.task import task
from jamjet.client import ArtifactRef, JamjetClient
from jamjet.durable import (
    durable,
    durable_run,
    reset_execution_context,
    set_execution_context,
)
from jamjet.entry import deploy, resume, run
from jamjet.eval.registry import scorer
from jamjet.protocols.adapter import ProtocolAdapter
from jamjet.protocols.registry import ProtocolRegistry
from jamjet.runtime import Runtime, RuntimeEvent, RuntimeResult  # noqa: F401
from jamjet.runtime.local import LocalRuntime  # noqa: F401

# Phase 1+2 additions
from jamjet.spec import (  # noqa: F401
    IR_VERSION,
    AgentSpec,
    DurabilityConfig,
    DurableAgentSpec,
    LLMConfig,
    MemoryConfig,
    ToolSpec,
    WorkflowSpec,
)
from jamjet.team import Loop, Parallel, Sequential, Team, TeamResult
from jamjet.tools.decorators import tool
from jamjet.workflow.workflow import Workflow
from jamjet.decorators import DurableAgent, workflow  # noqa: F401, E402
from jamjet.gate import PolicyDeniedError, gate, stderr_emitter  # noqa: F401, E402
# isort: on

__all__ = [
    "Agent",
    "AgentResult",
    "AgentMemory",
    "AgentSpec",
    "ArtifactRef",
    "ArtifactStore",
    "Session",
    "SessionStore",
    "DurabilityConfig",
    "DurableAgent",
    "DurableAgentSpec",
    "IR_VERSION",
    "JamjetClient",
    "LLMConfig",
    "LocalRuntime",
    "MemoryConfig",
    "ProtocolAdapter",
    "ProtocolRegistry",
    "Loop",
    "Parallel",
    "Runtime",
    "RuntimeEvent",
    "RuntimeResult",
    "Scope",
    "Sequential",
    "Team",
    "TeamResult",
    "ToolSpec",
    "Workflow",
    "WorkflowSpec",
    "PolicyDeniedError",
    "deploy",
    "durable",
    "durable_run",
    "gate",
    "reset_execution_context",
    "resume",
    "run",
    "scorer",
    "set_execution_context",
    "stderr_emitter",
    "task",
    "tool",
    "workflow",
]


def __getattr__(name: str):  # noqa: ANN001, ANN202
    if name in ("AgentMemory", "Scope"):
        from jamjet import memory

        return getattr(memory, name)  # raises a clear ImportError if engram missing
    raise AttributeError(f"module 'jamjet' has no attribute {name!r}")


def __dir__() -> list[str]:  # noqa: ANN201
    return list(set(globals()) | set(__all__))


__version__ = "0.10.2"
