"""The YAML compiler refuses what it would otherwise drop.

Before this, a document with its nodes in the wrong place compiled to zero
nodes and `jamjet validate` printed "Valid.", and a misspelled or unsupported
node key compiled to an IR that silently ignored it.
"""

from __future__ import annotations

import warnings
from pathlib import Path

import pytest
from typer.testing import CliRunner

from jamjet.cli.main import app
from jamjet.templates import AVAILABLE_TEMPLATES, render_template
from jamjet.workflow import ir_compiler as irc

runner = CliRunner()

HEADER = "workflow:\n  id: w\n  version: 1.0.0\n  start: a\n"

VALID_GRAPH = """
workflow:
  id: full
  version: 1.0.0
  name: Full
  description: every key the graph compiler reads
  state_schema: { type: object }
  start: fetch
  labels: { team: core }
policy:
  blocked_tools: [rm_rf]
retry_policies: {}
timeouts: {}
models: {}
tools: {}
mcp: { servers: {} }
a2a: { remote_agents: {} }
nodes:
  fetch:
    type: tool
    tool_ref: get_ticket
    input: { id: "{{ state.id }}" }
    output_schema: schemas.Ticket
    retry_policy: io_default
    timeout: 30s
    description: fetch the ticket
    labels: { kind: io }
    next: route
  route:
    type: condition
    conditions:
      - if: "state.priority == 'high'"
        next: review
      - when: "state.priority == 'low'"
        to: answer
    default: answer
  review:
    type: human_approval
    description: approve it
    timeout: 1h
    fallback: answer
    next: answer
  answer:
    type: model
    model: default_chat
    prompt: prompts/answer.md
    system_prompt: be brief
    next:
      - to: end
        when: "state.done == true"
  end:
    type: end
"""


def _compile_quietly(source: str) -> dict:
    with warnings.catch_warnings():
        warnings.simplefilter("error")
        return irc.compile_yaml(source)


def test_a_document_using_every_known_key_compiles_without_warnings() -> None:
    ir = _compile_quietly(VALID_GRAPH)
    assert set(ir["nodes"]) == {"fetch", "route", "review", "answer"}


@pytest.mark.parametrize(
    "source",
    [
        "workflow:\n  id: w\n",
        HEADER + "nodes: {}\n",
        HEADER + "nodes:\n  done:\n    type: end\n",
    ],
    ids=["no-nodes-key", "empty-nodes", "only-end"],
)
def test_a_workflow_with_no_nodes_is_rejected(source: str) -> None:
    with pytest.raises(ValueError, match="has no nodes"):
        irc.compile_yaml(source)


def test_nodes_nested_under_workflow_are_rejected() -> None:
    # The shape four examples used: a node list under `workflow:`.
    source = "workflow:\n  id: w\n  nodes:\n    - id: a\n      kind: model\n"
    with pytest.raises(ValueError, match="top level"):
        irc.compile_yaml(source)


def test_a_node_list_is_rejected() -> None:
    with pytest.raises(ValueError, match="mapping of node id"):
        irc.compile_yaml(HEADER + "nodes:\n  - id: a\n    type: model\n")


def test_an_unknown_node_key_is_an_error() -> None:
    source = HEADER + "nodes:\n  a:\n    type: model\n    model: m\n    output_key: answer\n"
    with pytest.raises(ValueError, match="node 'a' \\(model\\): unknown key\\(s\\) output_key"):
        irc.compile_yaml(source)


def test_a_key_of_another_node_type_is_an_error() -> None:
    # `server` belongs to mcp_tool; on a tool node it used to be dropped.
    source = HEADER + "nodes:\n  a:\n    type: tool\n    server: github\n    tool: search\n"
    with pytest.raises(ValueError, match="server, tool"):
        irc.compile_yaml(source)


def test_an_unknown_node_type_is_an_error() -> None:
    with pytest.raises(ValueError, match="unknown type 'wait'"):
        irc.compile_yaml(HEADER + "nodes:\n  a:\n    type: wait\n    next: end\n")


def test_an_unknown_condition_key_is_an_error() -> None:
    source = HEADER + "nodes:\n  a:\n    type: condition\n    conditions:\n      - condition: x\n        next: end\n"
    with pytest.raises(ValueError, match=r"conditions\[0\] has unknown key\(s\) condition"):
        irc.compile_yaml(source)


def test_an_unknown_next_entry_key_is_an_error() -> None:
    source = HEADER + "nodes:\n  a:\n    type: model\n    model: m\n    next:\n      - else: end\n"
    with pytest.raises(ValueError, match=r"next\[0\] has unknown key\(s\) else"):
        irc.compile_yaml(source)


def test_an_unknown_policy_key_is_an_error() -> None:
    source = HEADER + "nodes:\n  a:\n    type: model\n    model: m\n    policy:\n      block_tools: [x]\n"
    with pytest.raises(ValueError, match="block_tools"):
        irc.compile_yaml(source)


def test_unknown_top_level_and_header_keys_warn_but_compile() -> None:
    source = "workflow:\n  id: w\n  start_node: a\nextras: 1\nnodes:\n  a:\n    type: model\n    model: m\n"
    with pytest.warns(irc.WorkflowSchemaWarning) as record:
        ir = irc.compile_yaml(source)
    messages = " ".join(str(w.message) for w in record)
    assert "extras" in messages
    assert "start_node" in messages
    assert set(ir["nodes"]) == {"a"}


AGENT = (
    "agent:\n  id: a\n  strategy: react\n  goal: g\n  model: m\n"
    "  limits: {max_iterations: 2, max_cost_usd: 0.1, timeout_seconds: 10}\n"
)


def test_agent_tools_must_be_names() -> None:
    with pytest.raises(ValueError, match="agent.tools must be a list of tool names"):
        irc.compile_yaml(AGENT + "  tools:\n    - name: web_search\n")


def test_unknown_agent_keys_warn_but_compile() -> None:
    with pytest.warns(irc.WorkflowSchemaWarning, match="on_limit_exceeded"):
        ir = irc.compile_yaml(AGENT + "  on_limit_exceeded: return_partial\n")
    assert ir["nodes"]


def test_a_fleet_file_is_pointed_at_compile_bundle() -> None:
    with pytest.raises(ValueError, match="compile_bundle"):
        irc.compile_yaml("workflows:\n  w:\n    nodes:\n      a: {type: model, model: m}\n")


@pytest.mark.parametrize("template", AVAILABLE_TEMPLATES)
def test_every_template_renders_and_compiles_cleanly(template: str) -> None:
    files = render_template(template, "demo")
    for name, content in files.items():
        if name == "workflow.yaml":
            ir = _compile_quietly(content)
            assert ir["nodes"], f"{template}/{name} compiled to zero nodes"


# ── jamjet validate ──────────────────────────────────────────────────────────


def test_validate_rejects_the_old_nested_example_shape(tmp_path: Path) -> None:
    path = tmp_path / "workflow.yaml"
    path.write_text("workflow:\n  id: w\n  nodes:\n    - id: a\n      kind: model\n")
    result = runner.invoke(app, ["validate", str(path)])
    assert result.exit_code == 1
    assert "Valid." not in result.output


def test_validate_prints_schema_warnings_and_passes(tmp_path: Path) -> None:
    path = tmp_path / "workflow.yaml"
    path.write_text("workflow:\n  id: w\n  start_node: a\nnodes:\n  a:\n    type: model\n    model: m\n")
    result = runner.invoke(app, ["validate", str(path)])
    assert result.exit_code == 0, result.output
    assert "Warning:" in result.output
    assert "start_node" in result.output
    assert "Valid." in result.output


def test_validate_accepts_a_fleet_file(tmp_path: Path) -> None:
    path = tmp_path / "fleet.yaml"
    path.write_text(
        "workflows:\n"
        "  ping:\n"
        "    schedule: {cron: '*/5 * * * *'}\n"
        "    nodes:\n"
        "      check: {type: tool, tool_ref: ping}\n"
    )
    result = runner.invoke(app, ["validate", str(path)])
    assert result.exit_code == 0, result.output
    assert "fleet with 1 unit(s)" in result.output
    assert "ping v1.0.0" in result.output or "ping v0.1.0" in result.output
