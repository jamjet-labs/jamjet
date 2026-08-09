"""Binds the ADK tool-dispatch coordinates across the Python/Rust boundary.

The Python compiler stamps every tool-dispatch node with
``_DISPATCH_MODULE`` / ``_DISPATCH_FUNCTION``
(:mod:`jamjet.compiler.agent_ir`). Rust's IR validation re-derives "this node is
an ADK dispatch node" from exactly that pair, in
``runtime/ir/src/validate.rs``, because it is the only signal that survives
serialization: the compiler nests its ``tools`` resolver map inside the *kind*
dict, ``NodeKind::PythonFn`` has no such field, and serde drops unknown keys.

Nothing mechanically ties the two copies, and the drift **fails open**: rename
the coroutine on the Python side and the Rust pass silently matches nothing, so
an unmarked dispatch node sails through registration and runs a whole turn's
model-chosen tool calls with no tool policy — the precise hole that pass exists
to close. Comments on both sides do not fail builds. This test does.
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest

from jamjet.compiler.agent_ir import _DISPATCH_FUNCTION, _DISPATCH_MODULE

_REPO_ROOT = Path(__file__).resolve().parents[3]
_RUST_VALIDATE = _REPO_ROOT / "runtime" / "ir" / "src" / "validate.rs"


def _rust_const(text: str, name: str) -> str:
    """Return the string literal bound to a Rust `const NAME: &str = "...";`.

    Missing constant is a hard failure, never a skip: a rename on the Rust side
    is exactly the drift this test exists to catch.
    """
    match = re.search(rf'const\s+{name}\s*:\s*&str\s*=\s*"([^"]*)"\s*;', text)
    assert match is not None, (
        f"{name} not found in {_RUST_VALIDATE}. If it was renamed or removed, the "
        "Rust dispatch-marker validation no longer keys on the Python compiler's "
        "coordinates and fails OPEN — unmarked ADK dispatch nodes would register "
        "and run unpoliced."
    )
    return match.group(1)


def _rust_source() -> str:
    if not _RUST_VALIDATE.exists():
        pytest.skip(f"runtime workspace not present at {_RUST_VALIDATE}")
    return _RUST_VALIDATE.read_text()


def test_dispatch_module_matches_rust() -> None:
    assert _rust_const(_rust_source(), "ADK_DISPATCH_MODULE") == _DISPATCH_MODULE


def test_dispatch_function_matches_rust() -> None:
    assert _rust_const(_rust_source(), "ADK_DISPATCH_FUNCTION") == _DISPATCH_FUNCTION


def test_python_constants_are_the_ones_the_compiler_emits() -> None:
    """Guards the other half of the binding.

    Matching Rust against the Python *constants* proves nothing if the compiler
    stopped using those constants and hard-coded something else. This pins the
    constants to the node the compiler actually emits.
    """
    from jamjet.agents.agent import Agent
    from jamjet.compiler.agent_ir import compile_agent_to_ir
    from jamjet.tools.decorators import tool

    @tool
    async def echo(text: str) -> str:
        """Echo the input."""
        return text

    agent = Agent("a", model="anthropic/claude-sonnet-4-6", tools=[echo], strategy="react")
    ir = compile_agent_to_ir(agent, "hi", max_turns=2)
    dispatch = [
        node["kind"]
        for node in ir["nodes"].values()
        if node["kind"].get("agent_tool_dispatch") is True
    ]
    assert dispatch, "the compiler must emit at least one marked dispatch node"
    for kind in dispatch:
        assert kind["module"] == _DISPATCH_MODULE
        assert kind["function"] == _DISPATCH_FUNCTION
