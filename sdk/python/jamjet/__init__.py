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
from jamjet.tools.decorators import tool
from jamjet.workflow.workflow import Workflow

__all__ = ["Agent", "AgentResult", "task", "Workflow", "tool", "JamjetClient"]
__version__ = "0.1.0"
