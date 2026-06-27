"""Compile a :class:`jamjet.Agent` to a durable agent-loop ``WorkflowIr`` dict.

Track 2j-3. Authoring an agent (model + ``@tool`` functions + instructions) and
calling the durable path compiles to an event-sourced IR the Rust engine runs as
a ``model -> tools -> model`` loop. Because the model picks tools *dynamically*,
we cannot emit one node per actual call at compile time — instead we **statically
unroll** the loop into ``max_turns`` turns, each turn being three nodes:

1. ``__model_{t}__``     — a Model node carrying the agent's OpenAI tool schemas
   (Track 2j-2 threads them into the model call) and reading the running
   ``messages`` from state. It writes ``last_model_output`` /
   ``last_model_finish_reason`` / ``last_model_tool_calls`` to state.
2. ``__tool_gate_{t}__`` — a Condition node on
   ``state.last_model_finish_reason == "tool_calls"``:
     * true  -> the turn's tool-dispatch node;
     * false -> the terminal ``end`` (the final answer is ``last_model_output``).
3. ``__tools_{t}__``     — a single PythonFn node running
   :func:`jamjet.agents.tool_runtime.dispatch_tool_calls`, which executes every
   requested call and accumulates the messages, then loops to ``__model_{t+1}__``
   (or, on the last turn, to ``end`` — the loop is bounded).

The IR dict shape mirrors ``jamjet.workflow.ir_compiler`` exactly (the canonical
``WorkflowIr`` the engine deserializes); node-kind shapes mirror
``runtime/core/src/node.rs``: ``Model { ..., tools }``,
``Condition { branches }``, ``PythonFn { module, function, output_schema }``.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from jamjet.model.types import parse_model_ref

if TYPE_CHECKING:
    from jamjet.agents.agent import Agent
    from jamjet.tools.decorators import ToolDefinition

# The tool-dispatch PythonFn node points at this coroutine.
_DISPATCH_MODULE = "jamjet.agents.tool_runtime"
_DISPATCH_FUNCTION = "dispatch_tool_calls"

# The condition gate branches on the finish reason the Model executor records.
_TOOL_CALLS_EXPR = 'state.last_model_finish_reason == "tool_calls"'

# Graph terminal sentinel (matches the strategy compiler's edge-to-"end").
_END = "end"


def compile_agent_to_ir(agent: Agent, prompt: str, max_turns: int = 8) -> dict[str, Any]:
    """Compile *agent* + *prompt* into a durable agent-loop ``WorkflowIr`` dict.

    Parameters
    ----------
    agent:
        A constructed :class:`jamjet.Agent` (model + ``@tool`` functions +
        instructions).
    prompt:
        The user prompt; seeds the initial ``user`` message (see
        :func:`build_initial_state`, which the durable run entrypoint passes as
        the execution ``initial_input``).
    max_turns:
        Static unroll bound — the maximum number of ``model -> tools`` turns.
        Must be ``>= 1``.

    Returns
    -------
    dict
        A ``WorkflowIr`` dict ready to ``POST /workflows`` (matches the shape
        produced by ``jamjet.workflow.ir_compiler``).
    """
    if max_turns < 1:
        raise ValueError("max_turns must be >= 1")

    model_ref = parse_model_ref(agent.model).litellm_model
    tool_schemas = [_tool_schema(td) for td in agent._tools]
    tools_map = _tools_map(agent)

    nodes: dict[str, Any] = {}
    edges: list[dict[str, Any]] = []

    for t in range(max_turns):
        model_id = f"__model_{t}__"
        gate_id = f"__tool_gate_{t}__"
        tools_id = f"__tools_{t}__"
        is_last = t == max_turns - 1
        next_target = _END if is_last else f"__model_{t + 1}__"

        # ── Model node: carries tool schemas, reads messages from state. ──────
        nodes[model_id] = _node(
            model_id,
            {
                "type": "model",
                "model_ref": model_ref,
                # Empty prompt_ref => the executor uses the `messages` list from
                # state rather than a single templated user prompt.
                "prompt_ref": "",
                "output_schema": "",
                "system_prompt": agent.instructions or None,
                "tools": tool_schemas,
            },
            retry_policy="llm_default",
            description=f"agent turn {t} — model call",
            labels={"jamjet.agent.loop": "model", "jamjet.agent.turn": str(t)},
        )
        edges.append(_edge(model_id, gate_id))

        # ── Condition gate: tool_calls -> dispatch, else -> final answer. ─────
        nodes[gate_id] = _node(
            gate_id,
            {
                "type": "condition",
                "branches": [
                    {"condition": _TOOL_CALLS_EXPR, "target": tools_id},
                    {"condition": None, "target": _END},
                ],
                # Extra metadata mirroring the strategy compiler's condition
                # nodes; the engine reads `branches` and ignores `expression`.
                "expression": _TOOL_CALLS_EXPR,
            },
            description=f"agent turn {t} — tool-call gate",
            labels={"jamjet.agent.loop": "gate", "jamjet.agent.turn": str(t)},
        )
        edges.append(_edge(gate_id, tools_id, _TOOL_CALLS_EXPR))
        edges.append(_edge(gate_id, _END, None))

        # ── Tool-dispatch node: one PythonFn runs every requested call. ───────
        nodes[tools_id] = _node(
            tools_id,
            {
                "type": "python_fn",
                "module": _DISPATCH_MODULE,
                "function": _DISPATCH_FUNCTION,
                "output_schema": "",
                # Descriptor of the data the dispatch coroutine consumes. The
                # engine passes the full accumulated state to PythonFn nodes
                # (no per-node input mapping), so `dispatch_tool_calls` reads
                # these keys from state at runtime; this records intent + carries
                # the {name: "module:function"} resolver map.
                "input": {
                    "messages": "$state.messages",
                    "assistant_content": "$state.last_model_output",
                    "tool_calls": "$state.last_model_tool_calls",
                    "tools": tools_map,
                },
            },
            description=f"agent turn {t} — dispatch tool calls",
            labels={"jamjet.agent.loop": "tools", "jamjet.agent.turn": str(t)},
        )
        edges.append(_edge(tools_id, next_target))

    return {
        "workflow_id": agent.name,
        "version": "0.1.0",
        "name": agent.name,
        "description": agent.instructions or prompt,
        "state_schema": "",
        "start_node": "__model_0__",
        "nodes": nodes,
        "edges": edges,
        "retry_policies": {},
        "timeouts": {
            "workflow_timeout": agent.limits.timeout_seconds,
            "heartbeat_interval": 30,
        },
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {
            "jamjet.agent.id": agent.name,
            "jamjet.agent.loop": "true",
            "jamjet.agent.max_turns": str(max_turns),
        },
    }


def build_initial_state(agent: Agent, prompt: str) -> dict[str, Any]:
    """The execution ``initial_input`` for a compiled agent-loop IR.

    Seeds the running ``messages`` (system + user) and the
    ``{name: "module:function"}`` tool resolver map into workflow state, so the
    first model node and every ``dispatch_tool_calls`` node find them. The
    durable run entrypoint (Track 2j-4) passes this to ``start_execution``.
    """
    return {
        "messages": [
            {"role": "system", "content": agent.instructions or "You are a helpful assistant."},
            {"role": "user", "content": prompt},
        ],
        "tools": _tools_map(agent),
    }


# ── Helpers ──────────────────────────────────────────────────────────────────


def _tools_map(agent: Agent) -> dict[str, str]:
    """``{tool_name: "module:qualname"}`` — mirrors ``Agent.compile`` handler_ref."""
    return {td.name: f"{td.fn.__module__}:{td.fn.__qualname__}" for td in agent._tools}


def _tool_schema(td: ToolDefinition) -> dict[str, Any]:
    """An OpenAI function schema for a ``@tool`` definition.

    Mirrors ``LocalRuntime._tool_to_openai_schema`` so the durable and
    in-process paths offer the model identical tool schemas.
    """
    return {
        "type": "function",
        "function": {
            "name": td.name,
            "description": td.description,
            "parameters": td.input_schema,
        },
    }


def _node(
    node_id: str,
    kind: dict[str, Any],
    *,
    retry_policy: str | None = None,
    description: str | None = None,
    labels: dict[str, str] | None = None,
) -> dict[str, Any]:
    """Normalise a node into the IR node shape the engine expects."""
    return {
        "id": node_id,
        "kind": kind,
        "retry_policy": retry_policy,
        "node_timeout_secs": None,
        "description": description,
        "labels": labels or {},
    }


def _edge(from_: str, to: str, condition: str | None = None) -> dict[str, Any]:
    return {"from": from_, "to": to, "condition": condition}
