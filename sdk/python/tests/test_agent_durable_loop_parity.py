"""Track 2j-5 — the agent loop runs end to end, and in-process == durable.

This is the final 2j test: it proves authoring one ``Agent`` runs the same
``model -> tool -> model`` loop two ways and reaches the same answer.

Why a loop-logic driver instead of a full in-process engine run
---------------------------------------------------------------
A live durable run of the agent loop spans TWO processes that CI does not stand
up: the Rust engine (HTTP API) AND a separate ``jamjet worker`` that executes the
Python ``@tool`` functions. The tool-dispatch node lands in the ``python_tool``
queue, and the engine's default worker pool spawns ZERO internal workers for that
queue on purpose (``runtime/workers/src/pool.rs``) — only an external
``jamjet worker`` claims it. So the loop cannot complete inside one in-process
test; the README is the true full-stack demonstration.

Instead we drive the **real** compiled IR (:func:`compile_agent_to_ir`) and the
**real** tool-dispatch helper (:func:`dispatch_tool_calls`) through a faithful
re-implementation of the engine's control flow (:func:`drive_agent_loop`), with a
deterministic mock model. The driver mirrors the engine exactly:

* the scheduler dispatches a node once its LIVE predecessors completed
  (``runner.rs::is_runnable``); it NOW evaluates the per-turn gate (F-2j-dynamic-loop /
  FDL): a Condition gate records the branch it routes to, the dead branch's
  exclusive tail is marked ``NodeSkipped`` via a dead-edge fixpoint, and the loop
  EARLY-EXITS the moment a gate routes to ``end`` — the remaining turns are
  skipped, never run (so a model that finishes at turn 0 makes exactly ONE model
  call, not ``max_turns + 1``);
* a Model node reads ``state['messages']`` and writes
  ``last_model_output`` / ``last_model_finish_reason`` / ``last_model_tool_calls``
  (``model_node.rs`` state_patch);
* a PythonFn (``__tools__``) node receives the full state as input
  (``runner.rs`` passes ``progress.final_state``) and its return dict IS the
  state_patch (the worker posts the return AS state_patch, top-level merge), so
  ``dispatch_tool_calls``' ``{'messages': [...]}`` replaces ``state['messages']``.

The per-leg Rust/Python contracts (payload threading, message mapping, state_patch
fold, tool-call fidelity) are unit-tested in 2j-1..2j-4; this file proves the legs
compose into a terminating loop with the right answer, and that the in-process
``Agent.run`` reaches the same answer with the same scripted model.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path
from typing import Any

from jamjet.agents.tool_runtime import dispatch_tool_calls
from jamjet.compiler.agent_ir import build_initial_state, compile_agent_to_ir

# The shipped example IS the agent under test: import its module (tools + factory)
# so this test exercises exactly what the README demonstrates.
_EXAMPLE_DIR = Path(__file__).resolve().parents[3] / "examples" / "react-agent-durable"
if str(_EXAMPLE_DIR) not in sys.path:
    sys.path.insert(0, str(_EXAMPLE_DIR))
import weather_agent  # noqa: E402  (path inserted just above)

_PROMPT = "What's the weather in Paris?"
# The engine NOW evaluates the per-turn gate (FDL), so the durable loop EARLY-EXITS
# instead of running the full max_turns unroll: a turn whose model returns a tool
# call routes gate -> dispatch and proceeds; a turn whose model returns a final
# answer (finish_reason "stop") routes gate -> end, and the scheduler skips the
# remaining turns. The visit order is therefore the LIVE path only.
#
# tool-then-stop (turn 0 asks for a tool, turn 1 answers): the turn-1 gate routes
# to `end`, so __tools_1__ / __model_2__ are skipped — two model calls, not three.
_EARLY_EXIT_TOOL_THEN_STOP = [
    "__model_0__",
    "__tool_gate_0__",
    "__tools_0__",
    "__model_1__",
    "__tool_gate_1__",
]
# stop-at-turn-0 (the model answers immediately): the turn-0 gate routes to `end`,
# so EVERY tool turn is skipped — exactly ONE model call.
_EARLY_EXIT_STOP_AT_0 = [
    "__model_0__",
    "__tool_gate_0__",
]


# ── Deterministic mock model (single source of truth for both paths) ──────────


class ScriptedModel:
    """A deterministic model, shared by the durable loop and the in-process run so
    the parity assertion compares like for like.

    ``tool_turns`` leading turns ask for ``get_weather(city="Paris")`` (finish
    ``tool_calls``); every turn after that returns a final answer with no tool
    calls (finish ``stop``). The default (``tool_turns=1``) is the canonical
    react case: turn 0 asks for the tool, turn 1 answers. ``tool_turns=0`` makes
    the model answer immediately at turn 0 (so the gate routes straight to ``end``
    and the loop early-exits after a single model call).
    """

    FINAL_ANSWER = "It is sunny in Paris."
    TOOL_NAME = "get_weather"
    TOOL_ARGS = {"city": "Paris"}

    def __init__(self, tool_turns: int = 1) -> None:
        self.calls = 0
        self._tool_turns = tool_turns

    def next_turn(self) -> dict[str, Any]:
        """Return the next turn in engine-state shape (content/finish/tool_calls)."""
        turn = self.calls
        self.calls += 1
        if turn < self._tool_turns:
            return {
                "content": "",
                "finish_reason": "tool_calls",
                "tool_calls": [{"id": f"call_{turn}", "name": self.TOOL_NAME, "arguments": dict(self.TOOL_ARGS)}],
            }
        return {"content": self.FINAL_ANSWER, "finish_reason": "stop", "tool_calls": []}


# ── In-process adapter shim over the same ScriptedModel (OpenAI message shape) ──


class _OAIFunction:
    def __init__(self, name: str, arguments: str) -> None:
        self.name = name
        self.arguments = arguments


class _OAIToolCall:
    def __init__(self, call: dict[str, Any]) -> None:
        self.id = call["id"]
        self.type = "function"
        self.function = _OAIFunction(call["name"], json.dumps(call["arguments"]))


class _OAIMessage:
    def __init__(self, content: str | None, tool_calls: list[_OAIToolCall]) -> None:
        self.role = "assistant"
        self.content = content
        self.tool_calls = tool_calls


class ScriptedAdapter:
    """``LLMAdapter`` over a :class:`ScriptedModel` for the in-process react runner.

    The react strategy calls ``generate(messages, tools=...)`` and reads
    ``msg.content`` / ``msg.tool_calls``; we render the scripted turn into that
    OpenAI-message shape.
    """

    def __init__(self, script: ScriptedModel) -> None:
        self.script = script

    async def generate(self, messages: list[Any], *, tools: list[Any] | None = None) -> _OAIMessage:
        turn = self.script.next_turn()
        return _OAIMessage(
            content=turn["content"] or None,
            tool_calls=[_OAIToolCall(c) for c in turn["tool_calls"]],
        )


# ── Faithful in-process re-implementation of the engine's control flow ────────


async def drive_agent_loop(ir: dict[str, Any], state: dict[str, Any], model: ScriptedModel) -> list[str]:
    """Drive the compiled agent-loop IR exactly as the JamJet engine now does.

    Mirrors the FDL scheduler (``runner.rs``): a Condition gate evaluates its
    branches against committed state and records the chosen route; the dead
    branch's exclusive tail is marked ``NodeSkipped`` via a dead-edge fixpoint
    (``edge_dead``); a node is runnable iff it has a LIVE in-edge whose every live
    source has completed (``is_runnable``, the AND-join over live edges only). The
    loop therefore EARLY-EXITS the moment a gate routes to ``end``: the remaining
    turns are skipped, never run.

    ``end`` is the graph's terminal sentinel (``agent_ir._END``), not a node, so it
    is never runnable — routing to it simply leaves no runnable node and the loop
    stops. Returns the visit order of node ids; mutates *state* in place to the
    terminal state.
    """
    nodes = ir["nodes"]
    edges = ir["edges"]
    in_edges = {nid: [e for e in edges if e["to"] == nid] for nid in nodes}

    completed: set[str] = set()
    skipped: set[str] = set()
    routes: dict[str, str | None] = {}  # condition node id -> chosen branch target
    visited: list[str] = []

    def edge_dead(src: str, dst: str) -> bool:
        # A skipped source kills its out-edges (cascades a skip down the branch).
        if src in skipped:
            return True
        # A COMPLETED routing Condition's only live out-edge is its recorded route;
        # every other out-edge is dead (a null route makes ALL of them dead).
        node = nodes.get(src)
        if node is not None and node["kind"]["type"] == "condition":
            branches = node["kind"].get("branches") or []
            if branches and src in completed:
                return routes.get(src) != dst
        return False

    def is_runnable(nid: str) -> bool:
        if nid in completed or nid in skipped:
            return False
        ins = in_edges[nid]
        if not ins:
            return True  # root
        has_live = False
        all_live_sources_done = True
        for e in ins:
            if not edge_dead(e["from"], nid):
                has_live = True
                if e["from"] not in completed:
                    all_live_sources_done = False
        return has_live and all_live_sources_done

    def skip_cascade() -> None:
        # Fixpoint: a node whose EVERY in-edge is dead can never run, so skip it.
        # The skip folds into `completed` (satisfying a downstream AND-join without
        # it) AND into `skipped` (so its out-edges die, which may make more nodes
        # skippable). Roots (no in-edges) are never skipped here.
        changed = True
        while changed:
            changed = False
            for nid in nodes:
                if nid in completed or nid in skipped:
                    continue
                ins = in_edges[nid]
                if ins and all(edge_dead(e["from"], nid) for e in ins):
                    skipped.add(nid)
                    completed.add(nid)
                    changed = True

    while True:
        skip_cascade()
        runnable = [nid for nid in nodes if is_runnable(nid)]
        if not runnable:
            break
        # The live path of the static unroll is linear: the gate's route makes
        # exactly one successor live, so exactly one node runs per step.
        assert len(runnable) == 1, f"agent-loop unroll must be linear; runnable={runnable}"
        node_id = runnable[0]
        kind = nodes[node_id]["kind"]["type"]

        if kind == "model":
            # model_node.rs: read messages, call the model, write the last_model_* keys.
            turn = model.next_turn()
            state["last_model_output"] = turn["content"]
            state["last_model_finish_reason"] = turn["finish_reason"]
            state["last_model_tool_calls"] = turn["tool_calls"]
        elif kind == "python_fn":
            # Worker passes the FULL state as input; the return dict is the
            # state_patch (top-level merge -> replaces state["messages"]).
            patch = await dispatch_tool_calls(dict(state))
            for key, value in patch.items():
                state[key] = value
        elif kind == "condition":
            # FDL: honor the gate. Evaluate the branches against committed state
            # and record the chosen route (empty branches = pass-through, no
            # route). The next skip_cascade()/is_runnable() act on it.
            branches = nodes[node_id]["kind"].get("branches") or []
            if branches:
                routes[node_id] = _route_gate(branches, state)
        else:  # pragma: no cover - the compiler only emits these three kinds
            raise AssertionError(f"unexpected node kind {kind!r}")

        completed.add(node_id)
        visited.append(node_id)
    return visited


# ── Authored gate semantics (wired into drive_agent_loop above) ───────────────


def _eval_branch_condition(condition: str | None, state: dict[str, Any]) -> bool:
    """Evaluate a ``state.<key> == "<literal>"`` condition. ``None`` is the else."""
    if condition is None:
        return True
    match = re.fullmatch(r'\s*state\.(\w+)\s*==\s*"([^"]*)"\s*', condition)
    assert match, f"unexpected condition form: {condition!r}"
    key, literal = match.group(1), match.group(2)
    return state.get(key) == literal


def _route_gate(branches: list[dict[str, Any]], state: dict[str, Any]) -> str | None:
    for branch in branches:
        if _eval_branch_condition(branch["condition"], state):
            return branch["target"]
    return None


# ── Tests ─────────────────────────────────────────────────────────────────────


async def test_agent_loop_ir_runs_model_tool_model_and_terminates() -> None:
    """The compiled IR drives a model -> tool -> model loop that EARLY-EXITS at the
    turn the model answers: the tool is invoked once, the messages accumulate, and
    the gate routes the final turn to `end` so the remaining unroll is skipped."""
    agent = weather_agent.build_agent()
    ir = compile_agent_to_ir(agent, _PROMPT, max_turns=2)

    # The model node carries the agent's tool schemas (2j-2/2j-3 wiring) so the
    # model can emit tool_calls; the dispatch node points at the loop helper.
    assert ir["nodes"]["__model_0__"]["kind"]["tools"], "model node must carry tool schemas"
    disp = ir["nodes"]["__tools_0__"]["kind"]
    assert (disp["module"], disp["function"]) == ("jamjet.agents.tool_runtime", "dispatch_tool_calls")

    state = build_initial_state(agent, _PROMPT)
    model = ScriptedModel()
    visited = await drive_agent_loop(ir, state, model)

    # The model asked for a tool at turn 0 then answered at turn 1, so the turn-1
    # gate routes to `end`: the loop runs model -> tool -> model and EARLY-EXITS,
    # skipping __tools_1__/__model_2__ (NOT the full max_turns unroll).
    assert visited == _EARLY_EXIT_TOOL_THEN_STOP
    assert model.calls == 2  # turn 0 (tool) + turn 1 (final answer); NOT max_turns + 1

    # The tool was actually invoked once, with the model's arguments, and its
    # result was appended to the running messages (message accumulation).
    expected_tool_output = await weather_agent.get_weather(city="Paris")
    tool_msgs = [m for m in state["messages"] if m.get("role") == "tool"]
    assert len(tool_msgs) == 1
    assert tool_msgs[0]["name"] == "get_weather"
    assert tool_msgs[0]["content"] == expected_tool_output

    assistant_calls = [m for m in state["messages"] if m.get("role") == "assistant" and m.get("tool_calls")]
    assert json.loads(assistant_calls[0]["tool_calls"][0]["function"]["arguments"]) == {"city": "Paris"}

    # Terminates with the turn-1 answer (last_model_output, what run_durable extracts).
    assert state["last_model_output"] == ScriptedModel.FINAL_ANSWER


async def test_durable_loop_early_exits_on_immediate_stop() -> None:
    """EARLY-EXIT headline: a model that answers at turn 0 (finish_reason "stop")
    makes EXACTLY ONE model call — the turn-0 gate routes to `end`, every tool turn
    is skipped, and the loop never reaches __model_1__ (NOT the max_turns unroll)."""
    agent = weather_agent.build_agent()
    ir = compile_agent_to_ir(agent, _PROMPT, max_turns=2)

    state = build_initial_state(agent, _PROMPT)
    model = ScriptedModel(tool_turns=0)  # answer immediately, no tool call
    visited = await drive_agent_loop(ir, state, model)

    # One model call, the gate, then `end` — the rest of the unroll is skipped.
    assert visited == _EARLY_EXIT_STOP_AT_0
    assert model.calls == 1, "stop at turn 0 must make exactly ONE model call"
    # The dispatch node never ran, so no tool was invoked.
    assert [m for m in state["messages"] if m.get("role") == "tool"] == []
    # The answer is turn 0's output.
    assert state["last_model_output"] == ScriptedModel.FINAL_ANSWER


def test_compiled_gate_routes_tool_calls_to_dispatch_and_stop_to_end() -> None:
    """The authored gate routes tool_calls -> dispatch and a final answer -> end.

    The engine NOW acts on these edge conditions (F-2j-dynamic-loop / FDL): the
    same branch logic that ``drive_agent_loop`` honors above is what the scheduler
    evaluates to early-exit the durable run.
    """
    ir = compile_agent_to_ir(weather_agent.build_agent(), _PROMPT, max_turns=2)
    gate = ir["nodes"]["__tool_gate_0__"]["kind"]
    assert gate["type"] == "condition"

    branches = gate["branches"]
    assert _route_gate(branches, {"last_model_finish_reason": "tool_calls"}) == "__tools_0__"
    assert _route_gate(branches, {"last_model_finish_reason": "stop"}) == "end"


async def test_inprocess_run_matches_durable_loop_answer(monkeypatch: Any) -> None:
    """Parity: the SAME agent + prompt reaches the SAME answer in process and via
    the durable loop, and both actually invoke the tool."""
    agent = weather_agent.build_agent()

    # Durable loop path: real IR + real dispatch_tool_calls + scripted model.
    ir = compile_agent_to_ir(agent, _PROMPT, max_turns=2)
    state = build_initial_state(agent, _PROMPT)
    durable_model = ScriptedModel()
    visited = await drive_agent_loop(ir, state, durable_model)
    durable_output = state["last_model_output"]

    # In-process path: Agent.run() with the same scripted model injected at the
    # adapter seam (so neither litellm nor the network is touched).
    monkeypatch.setattr(
        "jamjet.runtime.local.executor.get_adapter",
        # T3-7: get_adapter now takes (config, governance); the scripted adapter
        # ignores governance because it injects its own ScriptedModel.
        lambda _cfg, _gov=None: ScriptedAdapter(ScriptedModel()),
    )
    inproc = await agent.run(_PROMPT)

    # Shape parity: identical final answer.
    assert durable_output == inproc.output == ScriptedModel.FINAL_ANSWER

    # Both paths invoked the tool exactly once.
    assert [c["tool"] for c in inproc.tool_calls] == ["get_weather"]
    assert [m["name"] for m in state["messages"] if m.get("role") == "tool"] == ["get_weather"]
    # And the durable path ran the model -> tool -> model loop, EARLY-EXITING at
    # the turn the model answered (exactly two model calls, not the full unroll).
    assert visited == _EARLY_EXIT_TOOL_THEN_STOP
    assert durable_model.calls == 2


def test_example_react_agent_durable_compiles() -> None:
    """The shipped example imports, compiles to a valid agent-loop IR, and its
    tools resolve to importable ``module:function`` refs the worker can load."""
    agent = weather_agent.build_agent()
    ir = compile_agent_to_ir(agent, "hi", max_turns=2)

    assert ir["labels"]["jamjet.agent.loop"] == "true"
    assert ir["start_node"] == "__model_0__"

    tools = build_initial_state(agent, "hi")["tools"]
    assert tools["get_weather"] == "weather_agent:get_weather"
    assert tools["add"] == "weather_agent:add"
