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

from jamjet.agents.agent import Agent, AgentResult
from jamjet.agents.task import task
from jamjet.client import JamjetClient
from jamjet.durable import (
    durable,
    durable_run,
    reset_execution_context,
    set_execution_context,
)
from jamjet.entry import deploy, resume, run
from jamjet.eval.registry import scorer
from jamjet.memory import AgentMemory, Scope  # noqa: F401
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
from jamjet.tools.decorators import tool
from jamjet.workflow.workflow import Workflow

# Decorators imported LAST so the `workflow` name binds to the decorator
# rather than the subpackage module (which Python's import machinery sets
# on the parent namespace when `jamjet.workflow.workflow` is imported above).
from jamjet.decorators import DurableAgent, workflow  # noqa: F401, E402

__all__ = [
    "Agent",
    "AgentResult",
    "AgentMemory",
    "AgentSpec",
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
    "Runtime",
    "RuntimeEvent",
    "RuntimeResult",
    "Scope",
    "ToolSpec",
    "Workflow",
    "WorkflowSpec",
    "deploy",
    "durable",
    "durable_run",
    "reset_execution_context",
    "resume",
    "run",
    "scorer",
    "set_execution_context",
    "task",
    "tool",
    "workflow",
]
__version__ = "0.8.0"
