"""
The Workflow decorator API — the primary way to define JamJet workflows in Python.

Usage:

    workflow = Workflow("my_workflow")

    @workflow.state
    class State(BaseModel):
        question: str
        answer: str | None = None

    @workflow.step
    async def step_one(state: State) -> State:
        ...

    @workflow.step(next={"step_two": lambda s: True})
    async def step_two(state: State) -> State:
        ...
"""

from __future__ import annotations

from collections.abc import Callable
from typing import Any, TypeVar

from pydantic import BaseModel

from jamjet.workflow.ir_compiler import compile_to_ir
from jamjet.workflow.types import StepDef, WorkflowDef

T = TypeVar("T", bound=BaseModel)
F = TypeVar("F", bound=Callable[..., Any])


class Workflow:
    """
    A JamJet workflow definition.

    Provides decorators for defining workflow state, steps, and routing.
    Compiles to the canonical IR for submission to the runtime.
    """

    def __init__(self, workflow_id: str, version: str = "0.1.0") -> None:
        self.workflow_id = workflow_id
        self.version = version
        self._state_class: type[BaseModel] | None = None
        self._steps: list[StepDef] = []
        self._start: str | None = None

    def state(self, cls: type[T]) -> type[T]:
        """
        Mark a Pydantic model as the workflow's shared state.

        Example::

            @workflow.state
            class MyState(BaseModel):
                input: str
                result: str | None = None
        """
        if not issubclass(cls, BaseModel):
            raise TypeError(f"@workflow.state requires a Pydantic BaseModel, got {cls}")
        self._state_class = cls
        return cls

    def step(
        self,
        func: F | None = None,
        *,
        name: str | None = None,
        next: dict[str, Callable[..., bool]] | None = None,
        human_approval: bool = False,
        timeout: str | None = None,
        retry_policy: str | None = None,
        model: str | None = None,
    ) -> Any:
        """
        Register a function as a workflow step (node).

        Can be used with or without arguments:

            @workflow.step
            async def my_step(state: State) -> State: ...

            @workflow.step(next={"other": lambda s: s.flag}, timeout="30s")
            async def branching_step(state: State) -> State: ...
        """

        def decorator(fn: F) -> F:
            step_name = name or fn.__name__
            if self._start is None:
                self._start = step_name
            self._steps.append(
                StepDef(
                    name=step_name,
                    fn=fn,
                    next=next or {},
                    human_approval=human_approval,
                    timeout=timeout,
                    retry_policy=retry_policy,
                    model=model,
                )
            )
            return fn

        if func is not None:
            # Used as @workflow.step (no arguments)
            return decorator(func)
        # Used as @workflow.step(...) (with arguments)
        return decorator

    def compile(self) -> dict[str, Any]:
        """
        Compile this workflow to the canonical IR (dict).

        Raises ValueError if the workflow is not valid.
        """
        if self._state_class is None:
            raise ValueError(f"Workflow '{self.workflow_id}' has no @workflow.state defined")
        if not self._steps:
            raise ValueError(f"Workflow '{self.workflow_id}' has no @workflow.step definitions")

        defn = WorkflowDef(
            workflow_id=self.workflow_id,
            version=self.version,
            state_schema=f"{self._state_class.__module__}.{self._state_class.__name__}",
            start_node=self._start or self._steps[0].name,
            steps=self._steps,
        )
        return compile_to_ir(defn)

    def __repr__(self) -> str:
        return f"Workflow(id={self.workflow_id!r}, steps={[s.name for s in self._steps]})"
