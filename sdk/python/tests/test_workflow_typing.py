from collections.abc import Awaitable, Callable
from pathlib import Path
from typing import assert_type

from mypy import api
from pydantic import BaseModel

from jamjet import Workflow


class State(BaseModel):
    value: int


workflow = Workflow("typing")


@workflow.step
def direct_sync(state: State) -> State:
    return state


@workflow.step
async def direct_async(state: State) -> State:
    return state


@workflow.step(name="configured_sync")
def configured_sync(state: State) -> State:
    return state


@workflow.step(name="configured_async")
async def configured_async(state: State) -> State:
    return state


typed_direct_sync: Callable[[State], State] = direct_sync
typed_direct_async: Callable[[State], Awaitable[State]] = direct_async
typed_configured_sync: Callable[[State], State] = configured_sync
typed_configured_async: Callable[[State], Awaitable[State]] = configured_async

assert_type(direct_sync(State(value=1)), State)
assert_type(configured_sync(State(value=1)), State)


async def check_async_returns() -> None:
    assert_type(await direct_async(State(value=1)), State)
    assert_type(await configured_async(State(value=1)), State)


def check_invalid_sync_calls() -> None:
    direct_sync("wrong")  # type: ignore[arg-type]
    configured_sync("wrong")  # type: ignore[arg-type]


async def check_invalid_async_calls() -> None:
    await direct_async("wrong")  # type: ignore[arg-type]
    await configured_async("wrong")  # type: ignore[arg-type]


def test_workflow_step_typing(tmp_path: Path) -> None:
    stdout, stderr, status = api.run(["--follow-imports=silent", "--cache-dir", str(tmp_path / "mypy"), __file__])
    assert status == 0, stdout + stderr
