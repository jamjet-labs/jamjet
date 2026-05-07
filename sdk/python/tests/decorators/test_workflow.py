import asyncio

from jamjet.decorators import workflow
from jamjet.spec import WorkflowSpec


def test_workflow_decorator_attaches_spec():
    @workflow
    async def trip(q: str) -> str:
        return q

    assert hasattr(trip, "__jamjet_spec__")
    spec = trip.__jamjet_spec__
    assert isinstance(spec, WorkflowSpec)
    assert spec.name == "trip"
    assert spec.entry_node == "trip"
    assert len(spec.nodes) == 1
    assert spec.nodes[0].id == "trip"


def test_workflow_callable_unchanged():
    @workflow
    async def f(x: str) -> str:
        return x.upper()

    assert asyncio.run(f("hi")) == "HI"
