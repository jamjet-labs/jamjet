"""Snapshot tests for execution traces. Run with --snapshot-update to regenerate."""

from pydantic import BaseModel

from jamjet import Workflow

wf = Workflow("snapshot-test", version="1.0.0")


@wf.state
class State(BaseModel):
    value: int = 0
    doubled: int = 0


@wf.step(name="add_step", next={"double_step": lambda s: True})
async def add_step(state: State) -> State:
    return state.model_copy(update={"value": 3 + 4})


@wf.step(name="double_step")
async def double_step(state: State) -> State:
    return state.model_copy(update={"doubled": state.value * 2})


def test_linear_workflow_trace(snapshot):
    result = wf.run_sync(State())
    assert result.to_snapshot() == snapshot


def test_linear_workflow_state(snapshot):
    result = wf.run_sync(State())
    assert result.state.model_dump() == snapshot


def test_linear_workflow_events_structure(snapshot):
    result = wf.run_sync(State())
    event_kinds = [{"step": e.step, "status": e.status} for e in result.events]
    assert event_kinds == snapshot
