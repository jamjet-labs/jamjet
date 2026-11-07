"""The YAML compiler refuses what it would otherwise drop.

Before this, a document with its nodes in the wrong place compiled to zero
nodes and `jamjet validate` printed "Valid.", and a misspelled or unsupported
node key compiled to an IR that silently ignored it.
"""

from __future__ import annotations

import warnings
from pathlib import Path

import pytest
import yaml
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
    assert set(ir["nodes"]) == {"fetch", "route", "review", "answer", "answer__route"}


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


def _compile_warning_messages(source: str) -> tuple[dict, list[str]]:
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        ir = irc.compile_yaml(source)
    return ir, [str(w.message) for w in caught if issubclass(w.category, irc.WorkflowSchemaWarning)]


def test_an_unknown_node_key_is_dropped_with_a_warning_that_says_what_to_use() -> None:
    source = HEADER + "nodes:\n  a:\n    type: model\n    model: m\n    output_key: answer\n"
    ir, messages = _compile_warning_messages(source)
    assert "output_key" not in ir["nodes"]["a"]["kind"]
    assert any("'output_key' is ignored" in m and "state.last_model_output" in m for m in messages), messages


def test_spellings_with_a_real_ir_target_are_mapped() -> None:
    source = (
        HEADER + "nodes:\n"
        "  a:\n    type: model\n    model: m\n    label: First\n    system: be brief\n"
        "    temperature: 0.2\n    max_tokens: 300\n    next: b\n"
        "  b:\n    type: tool\n    server: github\n    tool: search\n    arguments: {q: '{{ state.q }}'}\n"
        "    next: c\n"
        "  c:\n    type: a2a_task\n    agent_url: https://agent.example\n    skill: s\n    timeout_seconds: 30\n"
        "    next: d\n"
        "  d:\n    type: human_approval\n    message: approve?\n    next: e\n"
        "  e:\n    type: condition\n    branches:\n      - condition: state.x == 1\n        next: a\n"
        "    default: end\n"
    )
    ir, _ = _compile_warning_messages(source)
    nodes = ir["nodes"]
    assert nodes["a"]["description"] == "First"
    assert nodes["a"]["kind"]["system_prompt"] == "be brief"
    # model_ref stays the real model (model_allowlist and audit key on it); the
    # settings sit on the models entry the runtime reads for it.
    assert nodes["a"]["kind"]["model_ref"] == "m"
    assert ir["models"]["m"] == {"provider": "", "model": "m", "temperature": 0.2, "max_tokens": 300}
    assert nodes["b"]["kind"] == {
        "type": "mcp_tool",
        "server": "github",
        "tool": "search",
        "input_mapping": {"q": "{{ state.q }}"},
        "output_schema": "",
    }
    assert nodes["c"]["kind"]["remote_agent"] == "c"
    assert ir["remote_agents"]["c"] == {"url": "https://agent.example"}
    assert nodes["c"]["kind"]["timeout_secs"] == 30
    assert nodes["d"]["kind"]["description"] == "approve?"
    assert nodes["e"]["kind"]["branches"][0] == {"condition": "state.x == 1", "target": "a"}
    # The runtime refuses a branch with no matching out-edge.
    assert {"from": "e", "to": "a", "condition": None} in ir["edges"]
    assert {"from": "e", "to": "end", "condition": None} in ir["edges"]


def test_ir_shaped_branches_keep_their_targets() -> None:
    source = (
        HEADER + "nodes:\n  a:\n    type: condition\n    branches:\n"
        "      - condition: state.x == 1\n        target: b\n      - condition: null\n        target: c\n"
        "  b: {type: model, model: m}\n  c: {type: model, model: m}\n"
    )
    ir = _compile_quietly(source)
    assert [b["target"] for b in ir["nodes"]["a"]["kind"]["branches"]] == ["b", "c"]
    assert {e["to"] for e in ir["edges"] if e["from"] == "a"} == {"b", "c"}


def test_sampling_settings_that_disagree_for_one_model_are_an_error() -> None:
    source = (
        HEADER + "nodes:\n  a: {type: model, model: m, temperature: 0.1, next: b}\n"
        "  b: {type: model, model: m, temperature: 0.9}\n"
    )
    with pytest.raises(ValueError, match="different temperature/max_tokens"):
        irc.compile_yaml(source)
    shared = HEADER + "nodes:\n  a: {type: model, model: m, temperature: 0.1, next: b}\n  b: {type: model, model: m}\n"
    with pytest.raises(ValueError, match="uses\\s+without them|without them"):
        irc.compile_yaml(shared)


@pytest.mark.parametrize("key", ["cost_budget_usd: 1", "token_budget: {total_tokens: 5}", "on_budget_exceeded: a"])
def test_a_budget_on_a_node_is_an_error(key: str) -> None:
    with pytest.raises(ValueError, match="no per-node budget"):
        irc.compile_yaml(HEADER + f"nodes:\n  a:\n    type: model\n    model: m\n    {key}\n")


def test_input_values_become_strings_and_nested_values_are_an_error() -> None:
    node = "  a:\n    type: mcp_tool\n    server: s\n    tool: t\n    arguments: {count: 10, deep: true}\n"
    source = HEADER + "nodes:\n" + node
    ir = _compile_quietly(source)
    assert ir["nodes"]["a"]["kind"]["input_mapping"] == {"count": "10", "deep": "true"}
    nested = HEADER + "nodes:\n  a:\n    type: tool\n    tool_ref: t\n    input: {filter: {x: 1}}\n"
    with pytest.raises(ValueError, match="node 'a': input 'filter' must be a string"):
        irc.compile_yaml(nested)


def test_an_alias_and_its_name_together_are_an_error() -> None:
    source = HEADER + "nodes:\n  a:\n    type: model\n    model: m\n    system: x\n    system_prompt: y\n"
    with pytest.raises(ValueError, match="both system and system_prompt"):
        irc.compile_yaml(source)


def test_an_unknown_node_type_is_an_error() -> None:
    with pytest.raises(ValueError, match="unknown type 'wait'"):
        irc.compile_yaml(HEADER + "nodes:\n  a:\n    type: wait\n    next: end\n")


def test_limit_exceeded_is_a_node_type() -> None:
    ir = _compile_quietly(
        HEADER + "nodes:\n  a:\n    type: model\n    model: m\n    next: stop\n  stop:\n    type: limit_exceeded\n"
    )
    assert ir["nodes"]["stop"]["kind"] == {"type": "limit_exceeded"}


def test_agent_ref_on_a_non_agent_node_points_at_acts_for() -> None:
    source = HEADER + "nodes:\n  a:\n    type: model\n    model: m\n    agent_ref: jamjet://x\n"
    with pytest.raises(ValueError, match="use acts_for"):
        irc.compile_yaml(source)


def test_an_unknown_condition_key_warns() -> None:
    source = HEADER + "nodes:\n  a:\n    type: condition\n    conditions:\n      - unless: x\n        next: end\n"
    _, messages = _compile_warning_messages(source)
    assert any("conditions[0]" in m and "unless" in m for m in messages), messages


def test_else_in_next_is_the_unconditional_edge() -> None:
    nodes = (
        "nodes:\n  a:\n    type: model\n    model: m\n    next:\n"
        "      - when: state.x\n        to: b\n      - else: c\n"
        "  b: {type: model, model: m}\n  c: {type: model, model: m}\n"
    )
    source = HEADER + nodes
    ir = _compile_quietly(source)
    # The runtime evaluates `when` only on condition nodes; given to another
    # node it would run both targets, so the list becomes a routing node.
    assert ir["nodes"]["a__route"]["kind"]["branches"] == [
        {"condition": "state.x", "target": "b"},
        {"condition": None, "target": "c"},
    ]
    a_edges = [e for e in ir["edges"] if e["from"] == "a"]
    assert a_edges == [{"from": "a", "to": "a__route", "condition": None}]
    assert {e["to"] for e in ir["edges"] if e["from"] == "a__route"} == {"b", "c"}


def test_an_unknown_policy_key_is_an_error() -> None:
    source = HEADER + "nodes:\n  a:\n    type: model\n    model: m\n    policy:\n      block_tools: [x]\n"
    with pytest.raises(ValueError, match="block_tools"):
        irc.compile_yaml(source)


# ── governance is never dropped ──────────────────────────────────────────────

ONE_NODE = "nodes:\n  a:\n    type: model\n    model: m\n  over:\n    type: model\n    model: m\n"


def test_policy_under_the_workflow_header_reaches_the_ir() -> None:
    ir = _compile_quietly("workflow:\n  id: w\n  policy: {blocked_tools: [transfer_funds]}\n" + ONE_NODE)
    assert ir["policy"]["blocked_tools"] == ["transfer_funds"]


@pytest.mark.parametrize("place", ["top", "header"])
def test_budgets_and_data_policy_reach_the_ir_from_either_place(place: str) -> None:
    block = (
        "cost_budget_usd: 2.5\n"
        "token_budget: {total_tokens: 1000}\n"
        "on_budget_exceeded: over\n"
        "data_policy: {pii_fields: [ssn], retention_days: 30}\n"
        "continue_as_new: true\n"
    )
    if place == "top":
        source = "workflow:\n  id: w\n" + block + ONE_NODE
    else:
        source = "workflow:\n  id: w\n" + "".join(f"  {line}\n" for line in block.splitlines()) + ONE_NODE
    ir, _ = _compile_warning_messages(source)
    assert ir["cost_budget_usd"] == 2.5
    assert ir["token_budget"] == {"total_tokens": 1000}
    assert ir["on_budget_exceeded"] == "over"
    assert ir["data_policy"] == {"pii_fields": ["ssn"], "retention_days": 30}
    assert ir["continue_as_new"] is True


def test_governance_set_in_two_places_is_an_error() -> None:
    source = "workflow:\n  id: w\n  policy: {blocked_tools: [a]}\npolicy: {blocked_tools: [b]}\n" + ONE_NODE
    with pytest.raises(ValueError, match="policy is set in the top level and the workflow: header"):
        irc.compile_yaml(source)


@pytest.mark.parametrize(
    ("block", "match"),
    [
        ("cost_budget_usd: -1\n", "cost_budget_usd must be a positive number"),
        ("token_budget: {tokens: 5}\n", "Unknown key\\(s\\) in token_budget"),
        ("on_budget_exceeded: [over]\n", "on_budget_exceeded must name a node"),
        ("data_policy: {pii: true}\n", "Unknown key\\(s\\) in data_policy"),
    ],
)
def test_malformed_governance_is_an_error(block: str, match: str) -> None:
    with pytest.raises(ValueError, match=match):
        irc.compile_yaml("workflow:\n  id: w\n" + block + ONE_NODE)


def test_unenforced_governance_compiles_with_a_warning() -> None:
    source = "workflow:\n  id: w\n  cost_budget_usd: 1\n  on_budget_exceeded: over\n"
    source += "  data_policy: {pii_fields: [ssn]}\n"
    ir, messages = _compile_warning_messages(source + ONE_NODE)
    assert ir["on_budget_exceeded"] == "over" and ir["data_policy"] == {"pii_fields": ["ssn"]}
    assert any("on_budget_exceeded" in m and "does not read it" in m for m in messages), messages
    assert any("data_policy" in m and "does not enforce it" in m for m in messages), messages
    assert not any("enforces each" in m for m in messages)


def test_an_on_budget_exceeded_action_that_is_not_a_node_warns_and_the_budget_stays() -> None:
    source = "workflow:\n  id: w\n  cost_budget_usd: 1\n  on_budget_exceeded: pause\n" + ONE_NODE
    ir, messages = _compile_warning_messages(source)
    assert ir["cost_budget_usd"] == 1.0
    assert "on_budget_exceeded" not in ir
    assert any("'pause' does not name a node" in m for m in messages), messages


def test_node_data_policy_reaches_the_ir() -> None:
    source = HEADER + "nodes:\n  a:\n    type: model\n    model: m\n    data_policy: {pii_detectors: [email]}\n"
    ir, messages = _compile_warning_messages(source)
    assert ir["nodes"]["a"]["data_policy"] == {"pii_detectors": ["email"]}
    assert any("does not enforce it" in m for m in messages)


def test_agent_first_governance_reaches_the_ir() -> None:
    ir = _compile_quietly(AGENT + "  policy: {blocked_tools: [rm]}\n  cost_budget_usd: 1.5\n")
    assert ir["policy"]["blocked_tools"] == ["rm"]
    assert ir["cost_budget_usd"] == 1.5


def test_fleet_units_keep_their_governance() -> None:
    from jamjet.workflow.bundle import compile_bundle

    data = yaml.safe_load(
        "workflows:\n"
        "  pay:\n"
        "    policy: {blocked_tools: [transfer_funds]}\n"
        "    cost_budget_usd: 3\n"
        "    nodes:\n"
        "      a: {type: model, model: m}\n"
        "agents:\n"
        "  helper:\n"
        "    strategy: react\n"
        "    goal: g\n"
        "    model: m\n"
        "    limits: {max_iterations: 2, max_cost_usd: 0.1, timeout_seconds: 10}\n"
        "    policy: {blocked_tools: [rm]}\n"
    )
    irs = {ir["workflow_id"]: ir for ir in compile_bundle(data).workflows}
    assert irs["pay"]["policy"]["blocked_tools"] == ["transfer_funds"]
    assert irs["pay"]["cost_budget_usd"] == 3.0
    assert irs["helper"]["policy"]["blocked_tools"] == ["rm"]


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


def state_reads_nothing_writes(source: str) -> list[str]:
    """Template expressions in a graph workflow the engine could not fill.

    The engine renders only `{{ state.<path> }}` (no filters, `{% %}` or
    `{# #}`), and a model node writes only state.last_model_output, so a prompt
    may read the declared inputs and that key, nothing else.
    """
    import re

    doc = yaml.safe_load(source)
    schema = (doc.get("workflow") or {}).get("state_schema") or {}
    if isinstance(schema, dict) and isinstance(schema.get("properties"), dict):
        schema = schema["properties"]
    readable = set(schema) | {"last_model_output"}
    problems = [f"block tag {m}" for m in re.findall(r"\{[%#].*?[%#]\}", source)]
    for expr in re.findall(r"\{\{(.*?)\}\}", source):
        expr = expr.strip()
        match = re.fullmatch(r"state\.([A-Za-z_][\w]*)(?:\.[\w.]+)?", expr)
        if not match:
            problems.append(f"expression {{{{ {expr} }}}} is not a plain state.<path>")
        elif match.group(1) not in readable:
            problems.append(f"{{{{ {expr} }}}} reads a key nothing writes")
    return problems


@pytest.mark.parametrize("template", AVAILABLE_TEMPLATES)
def test_template_prompts_read_only_what_the_engine_writes(template: str) -> None:
    content = render_template(template, "demo").get("workflow.yaml")
    if content is None:
        pytest.skip("no workflow.yaml")
    assert state_reads_nothing_writes(content) == []


@pytest.mark.parametrize("template", AVAILABLE_TEMPLATES)
def test_no_template_auto_approves_or_branches_on_rejection(template: str) -> None:
    content = render_template(template, "demo").get("workflow.yaml") or ""
    doc = yaml.safe_load(content) or {}
    nodes = doc.get("nodes") or {}
    for node_id, node in nodes.items():
        if node.get("type") == "human_approval" and node.get("fallback"):
            target = nodes.get(node["fallback"], {})
            # Expiry must go to another person, never straight on as if approved.
            assert target.get("type") == "human_approval", f"{template}: {node_id} falls back to {node['fallback']}"
    assert "approved == false" not in content and "approved == true" not in content


def test_held_back_templates_are_refused_with_the_reason(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from jamjet.templates import HELD_BACK_TEMPLATES

    monkeypatch.chdir(tmp_path)
    for template in sorted(HELD_BACK_TEMPLATES):
        result = runner.invoke(app, ["init", f"p-{template}", "--template", template])
        assert result.exit_code == 1, template
        assert "not available in this release" in result.output
        assert not (tmp_path / f"p-{template}").exists()
    listing = runner.invoke(app, ["init", "--list-templates"])
    assert "Not available in this release" in listing.output
    assert "research-agent" in listing.output


def test_fleet_wide_governance_reaches_every_unit() -> None:
    from jamjet.workflow.bundle import compile_bundle

    fleet = (
        "policy: {blocked_tools: [transfer_funds]}\n"
        "defaults: {cost_budget_usd: 2}\n"
        "workflows:\n  a:\n    nodes:\n      n: {type: model, model: m}\n"
        "agents:\n  b:\n    strategy: react\n    goal: g\n    model: m\n"
        "    limits: {max_iterations: 2, max_cost_usd: 0.1, timeout_seconds: 10}\n"
    )
    for ir in compile_bundle(yaml.safe_load(fleet)).workflows:
        assert ir["policy"]["blocked_tools"] == ["transfer_funds"], ir["workflow_id"]
        assert ir["cost_budget_usd"] == 2.0, ir["workflow_id"]


@pytest.mark.parametrize(
    ("fleet", "match"),
    [
        ("polcy: {blocked_tools: [x]}\nworkflows: {a: {nodes: {n: {type: model, model: m}}}}\n", "polcy"),
        ("workflows: {a: {cost_budget: 3, nodes: {n: {type: model, model: m}}}}\n", "cost_budget"),
        ("defaults: {polcy: {}}\nworkflows: {a: {nodes: {n: {type: model, model: m}}}}\n", "polcy"),
        (
            "policy: {blocked_tools: [x]}\n"
            "workflows: {a: {policy: {blocked_tools: [y]}, nodes: {n: {type: model, model: m}}}}\n",
            "set it in one place",
        ),
    ],
)
def test_fleet_typos_and_double_settings_are_errors(fleet: str, match: str) -> None:
    from jamjet.workflow.bundle import compile_bundle

    with pytest.raises(ValueError, match=match):
        compile_bundle(yaml.safe_load(fleet))


def test_an_unknown_template_name_is_printed_literally(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.chdir(tmp_path)
    result = runner.invoke(app, ["init", "x", "--template", "[bold]bad[/bold"])
    assert result.exit_code == 1
    assert "[bold]bad[/bold" in result.output


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
    assert "Warning:" in result.stderr
    assert "start_node" in result.stderr
    assert "Valid." in result.stdout


def test_run_json_output_stays_parseable_when_a_file_warns(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    import json

    class FakeClient:
        async def __aenter__(self) -> FakeClient:
            return self

        async def __aexit__(self, *args: object) -> None:
            return None

        async def create_workflow(self, ir: dict) -> dict:
            return {}

        async def start_execution(self, **kwargs: object) -> dict:
            return {"execution_id": "exec_1"}

        async def get_execution(self, execution_id: str) -> dict:
            return {"status": "completed"}

        async def get_events(self, execution_id: str) -> dict:
            return {"events": []}

    async def no_sleep(delay: float) -> None:
        return None

    monkeypatch.setattr("jamjet.cli.main._client", lambda runtime, token=None: FakeClient())
    monkeypatch.setattr("jamjet.cli.main.asyncio.sleep", no_sleep)
    path = tmp_path / "workflow.yaml"
    path.write_text("workflow:\n  id: w\n  start_node: a\nnodes:\n  a:\n    type: model\n    model: m\n")
    result = runner.invoke(app, ["run", str(path), "--output", "json", "--runtime", "http://fake:7700"])
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["execution_id"] == "exec_1"
    assert "start_node" in result.stderr


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
