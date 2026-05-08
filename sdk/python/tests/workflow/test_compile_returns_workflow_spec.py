from pydantic import BaseModel

from jamjet import Workflow
from jamjet.spec import EdgeSpec, WorkflowSpec


def test_workflow_compile_returns_workflow_spec():
    w = Workflow(workflow_id="trip", version="1.0")

    @w.state
    class TripState(BaseModel):
        query: str = ""
        plan: str = ""

    @w.step
    async def search(state):
        state.query = "x"
        return state

    @w.step
    async def plan_it(state):
        state.plan = "y"
        return state

    spec = w.compile()
    assert isinstance(spec, WorkflowSpec)
    assert spec.name == "trip"
    assert len(spec.nodes) == 2
    assert {n.id for n in spec.nodes} == {"search", "plan_it"}
    assert all(isinstance(e, EdgeSpec) for e in spec.edges)
