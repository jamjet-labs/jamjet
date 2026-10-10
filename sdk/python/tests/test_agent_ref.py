"""``agent_ref``: the SDK names the registered agent a workflow or node acts
for, so the engine holds it to that Agent Card's constraints.

The engine side of the contract is tested in Rust
(``runtime/api/tests/agent_allowed_tools.rs::the_sdk_compiled_agent_is_held_to_its_card``)
against the fixture this module writes, so the two sides read the same IR.
"""

from __future__ import annotations

import json
import os
from pathlib import Path

import pytest

from jamjet.agents.agent import Agent
from jamjet.compiler.agent_ir import build_initial_state, compile_agent_to_ir
from jamjet.tools.decorators import tool
from jamjet.workflow.bundle import compile_bundle
from jamjet.workflow.ir_compiler import compile_yaml

AGENT = "jamjet://acme/researcher"
FIXTURE = Path(__file__).parent / "fixtures" / "agent_ref_agent_ir.json"


@tool
async def search_web(query: str) -> str:
    """Search the web."""
    return query


@tool
async def send_wire(to: str) -> str:
    """Send money."""
    return to


def _agent(**kw) -> Agent:
    return Agent("researcher", model="anthropic/claude-sonnet-4-6", tools=[search_web, send_wire], **kw)


def test_the_agent_compiler_emits_agent_ref():
    ir = compile_agent_to_ir(_agent(agent_ref=AGENT), "hi", max_turns=1)
    assert ir["agent_ref"] == AGENT


def test_no_agent_ref_emits_nothing_and_keeps_the_version():
    plain = compile_agent_to_ir(_agent(), "hi", max_turns=1)
    assert "agent_ref" not in plain
    # The key is part of the content version: a graph compiled for one agent
    # must not be reused for another.
    attributed = compile_agent_to_ir(_agent(agent_ref=AGENT), "hi", max_turns=1)
    assert attributed["version"] != plain["version"]


@pytest.mark.parametrize("bad", ["", "  ", 42])
def test_a_bad_agent_ref_is_refused(bad):
    with pytest.raises((ValueError, TypeError)):
        _agent(agent_ref=bad)


def test_yaml_workflow_and_node_attribution():
    ir = compile_yaml(
        f"""
workflow:
  id: wf
  version: 0.1.0
  agent_ref: {AGENT}
nodes:
  search:
    type: tool
    tool_ref: search_web
    acts_for: jamjet://acme/payer
    next: end
"""
    )
    assert ir["agent_ref"] == AGENT
    assert ir["nodes"]["search"]["agent_ref"] == "jamjet://acme/payer"


def test_yaml_without_attribution_emits_nothing():
    ir = compile_yaml(
        """
workflow:
  id: wf
  version: 0.1.0
nodes:
  search:
    type: tool
    tool_ref: search_web
    next: end
"""
    )
    assert "agent_ref" not in ir
    assert "agent_ref" not in ir["nodes"]["search"]


def test_yaml_refuses_a_non_string_agent_ref():
    with pytest.raises(ValueError):
        compile_yaml(
            """
workflow:
  id: wf
  agent_ref: [a]
nodes:
  s: {type: tool, tool_ref: x, next: end}
"""
        )


def test_bundle_workflow_units_carry_agent_ref():
    bundle = compile_bundle(
        {
            "workflows": {
                "wf": {
                    "agent_ref": AGENT,
                    "nodes": {"s": {"type": "tool", "tool_ref": "x", "acts_for": AGENT, "next": "end"}},
                }
            }
        }
    )
    ir = bundle.workflows[0]
    assert ir["agent_ref"] == AGENT
    assert ir["nodes"]["s"]["agent_ref"] == AGENT


def test_the_engine_fixture_is_current():
    """Writes (``JAMJET_REGEN_AGENT_REF_IR=1``) or checks the IR and dispatch
    state the Rust engine test registers and runs."""
    agent = _agent(agent_ref=AGENT)
    ir = compile_agent_to_ir(agent, "", max_turns=1)
    state = build_initial_state(agent, "hi")
    doc = {"ir": ir, "tools": state["tools"]}
    text = json.dumps(doc, indent=2, sort_keys=True) + "\n"
    if os.environ.get("JAMJET_REGEN_AGENT_REF_IR"):
        FIXTURE.write_text(text)
    assert FIXTURE.read_text() == text, "stale fixture; regenerate with JAMJET_REGEN_AGENT_REF_IR=1"


def test_agent_first_yaml_and_bundle_agent_units_carry_agent_ref():
    ir = compile_yaml(
        f"""
agent:
  id: researcher
  strategy: react
  goal: research
  model: anthropic/claude-sonnet-4-6
  agent_ref: {AGENT}
  limits: {{max_iterations: 3, max_cost_usd: 0.5, timeout_seconds: 60}}
"""
    )
    assert ir["agent_ref"] == AGENT
    bundle = compile_bundle(
        {
            "agents": {
                "researcher": {
                    "strategy": "react",
                    "goal": "research",
                    "model": "anthropic/claude-sonnet-4-6",
                    "agent_ref": AGENT,
                    "limits": {"max_iterations": 3, "max_cost_usd": 0.5, "timeout_seconds": 60},
                }
            }
        }
    )
    assert bundle.workflows[0]["agent_ref"] == AGENT


def test_yaml_agent_ref_on_a_non_agent_node_is_refused():
    """It would read as attribution and be silently dropped (review 2)."""
    with pytest.raises(ValueError, match="acts_for"):
        compile_yaml(
            f"""
workflow:
  id: wf
nodes:
  s:
    type: tool
    tool_ref: x
    agent_ref: {AGENT}
    next: end
"""
        )
