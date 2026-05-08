"""@DurableAgent class decorator. Three forms: bare, parameterized, stateless."""
from __future__ import annotations

import inspect
from typing import Any, overload

from jamjet.spec import (
    DurabilityConfig,
    DurableAgentSpec,
    LLMConfig,
    MemoryConfig,
    MethodSpec,
    ToolSpec,
)

_DEFAULT_LLM = LLMConfig(provider="openai", model="gpt-4o")
_INJECTED_ATTRS = ("memory", "llm", "workflow_id", "random", "uuid_gen", "now")


def _build_method_specs(cls: type) -> list[MethodSpec]:
    raw_methods: list[tuple[str, dict[str, Any] | None]] = []
    explicit_entry: str | None = None
    has_run = False

    for name, attr in cls.__dict__.items():
        if not callable(attr) or name.startswith("_"):
            continue
        if not inspect.iscoroutinefunction(attr):
            continue
        meta = getattr(attr, "__jamjet_task__", None)
        if meta is not None and meta.get("is_entrypoint"):
            explicit_entry = name
        if name == "run":
            has_run = True
        raw_methods.append((name, meta))

    if not raw_methods:
        return []

    if explicit_entry is None:
        explicit_entry = "run" if has_run else raw_methods[0][0]

    return [
        MethodSpec(
            name=name,
            is_step=meta["is_step"] if meta else True,
            is_entrypoint=(name == explicit_entry),
        )
        for name, meta in raw_methods
    ]


class _NotInjected:
    """Class-level descriptor that raises until the runtime injects via instance __dict__."""

    def __set_name__(self, owner: type, name: str) -> None:
        self._name = name

    def __get__(self, instance: Any, owner: type) -> Any:
        if instance is None:
            return self
        raise RuntimeError(
            f"self.{self._name} is not running inside a JamJet runtime. "
            f"Invoke this @DurableAgent via `await run(cls, ...)` or "
            f"`LocalRuntime.execute(spec, input)`."
        )


def _ban_attribute_access_outside_runtime(cls: type) -> None:
    for attr in _INJECTED_ATTRS:
        if attr not in cls.__dict__:
            descriptor = _NotInjected()
            descriptor._name = attr  # __set_name__ not called when using setattr post-hoc
            setattr(cls, attr, descriptor)


def _tool_def_to_spec(defn: Any) -> ToolSpec:
    return ToolSpec(
        name=defn.name,
        description=defn.description,
        input_schema=defn.input_schema,
        handler_ref=f"{defn.fn.__module__}:{defn.fn.__qualname__}",
    )


@overload
def DurableAgent(cls: type) -> type: ...  # noqa: N802
@overload
def DurableAgent(  # noqa: N802
    *,
    model: str | None = ...,
    instructions: str = ...,
    tools: list[Any] | None = ...,
    memory: MemoryConfig | None = ...,
    durability: DurabilityConfig | None = ...,
    stateless: bool = ...,
    llm: LLMConfig | None = ...,
) -> Any: ...


def DurableAgent(  # noqa: N802
    cls: type | None = None,
    *,
    model: str | None = None,
    instructions: str = "",
    tools: list[Any] | None = None,
    memory: MemoryConfig | None = None,
    durability: DurabilityConfig | None = None,
    stateless: bool = False,
    llm: LLMConfig | None = None,
) -> Any:
    """Class decorator that compiles a class into a DurableAgentSpec.

    Three forms:
        @DurableAgent                  # bare, all defaults
        @DurableAgent(model=..., ...)  # parameterized
        @DurableAgent(stateless=True)  # disable memory + durability
    """
    if stateless and (memory is not None or durability is not None):
        raise ValueError("stateless=True conflicts with explicit memory= or durability=")

    def _decorate(c: type) -> type:
        resolved_llm = llm or (
            LLMConfig(provider="openai", model=model) if model else _DEFAULT_LLM
        )
        resolved_memory: MemoryConfig | None
        resolved_durability: DurabilityConfig
        if stateless:
            resolved_memory = None
            resolved_durability = DurabilityConfig(checkpoint_every_step=False)
        else:
            resolved_memory = memory if memory is not None else MemoryConfig()
            resolved_durability = durability if durability is not None else DurabilityConfig()

        tool_specs: list[ToolSpec] = []
        for t in tools or []:
            defn = getattr(t, "_jamjet_tool", None)
            if defn is None:
                raise TypeError(f"{t!r} is not a @tool-decorated function.")
            tool_specs.append(_tool_def_to_spec(defn))

        spec = DurableAgentSpec(
            name=c.__name__,
            instructions=instructions,
            llm=resolved_llm,
            tools=tool_specs,
            memory=resolved_memory,
            durability=resolved_durability,
            class_ref=f"{c.__module__}:{c.__name__}",
            methods=_build_method_specs(c),
        )

        c.__jamjet_spec__ = spec  # type: ignore[attr-defined]
        _ban_attribute_access_outside_runtime(c)
        return c

    if cls is not None and isinstance(cls, type):
        return _decorate(cls)
    return _decorate
