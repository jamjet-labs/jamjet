"""
JamJet reasoning strategy compiler (§14.3–14.5).

Each strategy is compiled into an explicit IR sub-DAG.  The runtime executes
the IR directly — strategy names are never visible at execution time.

Supported strategies
--------------------
- ``react``            — Reason + Act loop (thought → tool → observation)
- ``plan-and-execute`` — Generate plan, execute steps sequentially (default)
- ``critic``           — Draft → critic evaluation → revise loop
- ``reflection``       — Execute → self-reflect → revise loop (§3.29)
- ``consensus``        — Multiple agents → vote → judge (§3.30)
- ``debate``           — Propose → counter → judge loop (§3.31)

Compiled IR contract (§14.4)
-----------------------------
1. Input : agent declaration + strategy name + config + tools + limits
2. Output: IR dict compatible with ``compile_yaml`` / ``compile_to_ir`` format
3. Limits: ``max_iterations``, ``max_cost_usd``, ``timeout_seconds`` are wired
           as guard condition nodes at compile time.
4. Metadata: compiled IR carries ``strategy_name`` + ``strategy_config``
             snapshot in ``labels`` for observability.

Node naming convention
----------------------
All strategy-generated nodes are prefixed with ``__`` to avoid collisions with
user-defined nodes.
"""

from __future__ import annotations

import dataclasses
from typing import Any


@dataclasses.dataclass
class StrategyLimits:
    """Required limits block for all agent-first workflows (§14.5)."""

    max_iterations: int
    max_cost_usd: float
    timeout_seconds: int

    def validate(self) -> None:
        """Raise ValueError if any limit is invalid."""
        if self.max_iterations < 1:
            raise ValueError("max_iterations must be >= 1")
        if self.max_cost_usd <= 0:
            raise ValueError("max_cost_usd must be > 0")
        if self.timeout_seconds < 1:
            raise ValueError("timeout_seconds must be >= 1")

    def to_dict(self) -> dict[str, Any]:
        return {
            "max_iterations": self.max_iterations,
            "max_cost_usd": self.max_cost_usd,
            "timeout_seconds": self.timeout_seconds,
        }


def compile_strategy(
    strategy_name: str,
    strategy_config: dict[str, Any],
    tools: list[str],
    model: str,
    limits: StrategyLimits,
    goal: str,
    agent_id: str,
) -> dict[str, Any]:
    """
    Compile a named strategy into IR nodes + edges.

    Returns a dict with keys:
        nodes: dict[str, node_def]
        edges: list[edge_def]
        start_node: str
        strategy_metadata: dict

    Raises
    ------
    ValueError
        If ``strategy_name`` is unknown or ``limits`` is invalid.
    """
    limits.validate()

    compilers = {
        "react": _compile_react,
        "plan-and-execute": _compile_plan_and_execute,
        "critic": _compile_critic,
        "reflection": _compile_reflection,
        "consensus": _compile_consensus,
        "debate": _compile_debate,
    }

    compiler_fn = compilers.get(strategy_name)
    if compiler_fn is None:
        known = ", ".join(sorted(compilers))
        raise ValueError(f"Unknown strategy '{strategy_name}'. Known strategies: {known}")

    result = compiler_fn(strategy_config, tools, model, limits, goal, agent_id)
    result["strategy_metadata"] = {
        "strategy_name": strategy_name,
        "strategy_config": strategy_config,
        "limits": limits.to_dict(),
        "agent_id": agent_id,
    }
    return result


# ── Helpers ───────────────────────────────────────────────────────────────────


def _model_node(
    model: str,
    prompt: str,
    output_key: str,
    *,
    system_prompt: str | None = None,
    labels: dict[str, str] | None = None,
) -> dict[str, Any]:
    return {
        "kind": {
            "type": "model",
            "model_ref": model,
            "prompt_ref": prompt,
            "output_schema": output_key,
            "system_prompt": system_prompt,
        },
        "retry_policy": "llm_default",
        "node_timeout_secs": None,
        "description": prompt,
        "labels": labels or {},
    }


def _condition_node(
    expression: str,
    branches: list[dict[str, Any]],
    *,
    labels: dict[str, str] | None = None,
) -> dict[str, Any]:
    return {
        "kind": {
            "type": "condition",
            "branches": branches,
            "expression": expression,
        },
        "retry_policy": None,
        "node_timeout_secs": None,
        "description": expression,
        "labels": labels or {},
    }


def _limit_exceeded_node() -> dict[str, Any]:
    """Terminal node that signals a strategy limit was hit."""
    return {
        "kind": {
            "type": "limit_exceeded",
            "description": "Strategy limit reached",
        },
        "retry_policy": None,
        "node_timeout_secs": None,
        "description": "Strategy limit exceeded — execution halted",
        "labels": {"jamjet.strategy.limit": "true"},
    }


def _edge(from_: str, to: str, condition: str | None = None) -> dict[str, Any]:
    return {"from": from_, "to": to, "condition": condition}


def _cost_guard_branches(ok_target: str) -> list[dict[str, Any]]:
    """Branches for a cost-guard condition node."""
    return [
        {"condition": "state.__cost_exceeded__", "target": "__limit_exceeded__"},
        {"condition": None, "target": ok_target},
    ]


# ── plan-and-execute ──────────────────────────────────────────────────────────


def _compile_plan_and_execute(
    config: dict[str, Any],
    tools: list[str],
    model: str,
    limits: StrategyLimits,
    goal: str,
    agent_id: str,
) -> dict[str, Any]:
    """
    Compiled structure (§14.4):

        __plan__ → __cost_guard_0__ → __step_0__
                                    → __limit_exceeded__ (if cost exceeded)
        __step_0__ → __cost_guard_1__ → __step_1__
                                      → __limit_exceeded__
        ...
        __step_{N-1}__ → __finalize__ → end
    """
    n = limits.max_iterations
    verifier_model = config.get("verifier_model")

    nodes: dict[str, Any] = {}
    edges: list[dict[str, Any]] = []

    # ── Plan generation node ──────────────────────────────────────────────────
    tools_list = ", ".join(tools) if tools else "none"
    nodes["__plan__"] = _model_node(
        model,
        (
            f"You are an AI agent working on: {goal}\n"
            f"Available tools: {tools_list}\n"
            f"Generate a structured plan with up to {n} concrete steps. "
            'Output JSON: {"steps": ["step 1", "step 2", ...]}'
        ),
        "__plan__",
        system_prompt="You are a planning AI. Output valid JSON only.",
        labels={"jamjet.strategy.node": "plan_generation", "jamjet.strategy.event": "plan_generated"},
    )

    # First edge: plan → cost_guard_0
    edges.append(_edge("__plan__", "__cost_guard_0__"))

    # ── Step executor nodes (unrolled loop) ────────────────────────────────────
    for i in range(n):
        step_id = f"__step_{i}__"
        guard_id = f"__cost_guard_{i}__"
        next_guard_id = f"__cost_guard_{i + 1}__"

        # Cost guard node before this step
        ok_target = step_id
        nodes[guard_id] = _condition_node(
            "state.__cost_exceeded__ == true",
            _cost_guard_branches(ok_target),
            labels={"jamjet.strategy.node": "cost_guard", "jamjet.strategy.iteration": str(i)},
        )

        # Step executor node
        step_prompt = (
            f"You are executing step {i + 1} of {n} for the goal: {goal}\n"
            f"The plan is: {{{{ state.__plan__ }}}}\n"
            f"Execute step {i + 1} using the available tools. "
            "Record your result."
        )
        step_labels: dict[str, str] = {
            "jamjet.strategy.node": "step_executor",
            "jamjet.strategy.event": "iteration_started",
            "jamjet.strategy.iteration": str(i),
        }
        nodes[step_id] = _model_node(
            model,
            step_prompt,
            f"__step_{i}_output__",
            labels=step_labels,
        )

        # Optional verifier after each step
        if verifier_model:
            verifier_id = f"__verify_{i}__"
            nodes[verifier_id] = _model_node(
                verifier_model,
                (
                    f"Verify step {i + 1} output against the goal: {goal}\n"
                    'Output JSON: {"passed": true/false, "score": 0.0-1.0, "feedback": "..."}'
                ),
                f"__verify_{i}_result__",
                labels={"jamjet.strategy.node": "verifier", "jamjet.strategy.event": "critic_verdict"},
            )
            edges.append(_edge(step_id, verifier_id))
            # After verification, continue to next guard
            if i < n - 1:
                edges.append(_edge(verifier_id, next_guard_id))
            else:
                edges.append(_edge(verifier_id, "__finalize__"))
        else:
            # No verifier: step → next guard (or finalize on last step)
            if i < n - 1:
                edges.append(_edge(step_id, next_guard_id))
            else:
                edges.append(_edge(step_id, "__finalize__"))

        # Cost guard edges
        edges.append(_edge(guard_id, "__limit_exceeded__", "state.__cost_exceeded__ == true"))
        edges.append(_edge(guard_id, step_id, None))

    # ── Finalizer node ────────────────────────────────────────────────────────
    nodes["__finalize__"] = _model_node(
        model,
        (
            f"You completed all steps for the goal: {goal}\n"
            "Synthesize the results from all steps into a final, well-structured response."
        ),
        "result",
        labels={"jamjet.strategy.node": "finalizer", "jamjet.strategy.event": "strategy_completed"},
    )
    edges.append(_edge("__finalize__", "end"))

    # ── Limit exceeded terminal ───────────────────────────────────────────────
    nodes["__limit_exceeded__"] = _limit_exceeded_node()
    edges.append(_edge("__limit_exceeded__", "end"))

    return {
        "nodes": nodes,
        "edges": edges,
        "start_node": "__plan__",
    }


# ── react ─────────────────────────────────────────────────────────────────────


def _compile_react(
    config: dict[str, Any],
    tools: list[str],
    model: str,
    limits: StrategyLimits,
    goal: str,
    agent_id: str,
) -> dict[str, Any]:
    """
    Compiled structure (§14.4):

        __think_0__ → __cost_guard_0__ → __observe_0__
                                       → __limit_exceeded__
        __observe_0__ → __think_1__ → ...
        __think_{N-1}__ → __finalize__
    """
    n = limits.max_iterations
    tools_list = ", ".join(tools) if tools else "none"

    nodes: dict[str, Any] = {}
    edges: list[dict[str, Any]] = []

    for i in range(n):
        think_id = f"__think_{i}__"
        observe_id = f"__observe_{i}__"
        guard_id = f"__react_guard_{i}__"
        next_think_id = f"__think_{i + 1}__"

        # Thought node
        nodes[think_id] = _model_node(
            model,
            (
                f"Goal: {goal}\n"
                f"Available tools: {tools_list}\n"
                f"Iteration {i + 1} of {n}. "
                "Think about what to do next. If you have enough information to answer, say FINISH. "
                "Otherwise choose a tool and describe what input to pass. "
                'Output JSON: {"thought": "...", "action": "tool_name or FINISH", "input": {...}}'
            ),
            f"__think_{i}_output__",
            labels={
                "jamjet.strategy.node": "react_think",
                "jamjet.strategy.event": "iteration_started",
                "jamjet.strategy.iteration": str(i),
            },
        )

        # Cost guard
        ok_target = observe_id if i < n - 1 else "__finalize__"
        nodes[guard_id] = _condition_node(
            "state.__cost_exceeded__ == true",
            _cost_guard_branches(ok_target),
            labels={"jamjet.strategy.node": "cost_guard", "jamjet.strategy.iteration": str(i)},
        )

        edges.append(_edge(think_id, guard_id))
        edges.append(_edge(guard_id, "__limit_exceeded__", "state.__cost_exceeded__ == true"))
        edges.append(_edge(guard_id, ok_target, None))

        if i < n - 1:
            # Observation node (process tool result)
            nodes[observe_id] = _model_node(
                model,
                (
                    f"Goal: {goal}\n"
                    f"Previous thought: {{{{ state.__think_{i}_output__ }}}}\n"
                    "Process the tool result and update your understanding. "
                    'Output JSON: {"observation": "...", "progress": "..."}'
                ),
                f"__observe_{i}_output__",
                labels={
                    "jamjet.strategy.node": "react_observe",
                    "jamjet.strategy.event": "tool_called",
                    "jamjet.strategy.iteration": str(i),
                },
            )
            edges.append(_edge(observe_id, next_think_id))

    # Finalizer
    nodes["__finalize__"] = _model_node(
        model,
        (f"Goal: {goal}\nBased on all thoughts and observations, produce a final answer."),
        "result",
        labels={"jamjet.strategy.node": "finalizer", "jamjet.strategy.event": "strategy_completed"},
    )
    edges.append(_edge("__finalize__", "end"))

    nodes["__limit_exceeded__"] = _limit_exceeded_node()
    edges.append(_edge("__limit_exceeded__", "end"))

    return {
        "nodes": nodes,
        "edges": edges,
        "start_node": "__think_0__",
    }


# ── critic ────────────────────────────────────────────────────────────────────


def _compile_critic(
    config: dict[str, Any],
    tools: list[str],
    model: str,
    limits: StrategyLimits,
    goal: str,
    agent_id: str,
) -> dict[str, Any]:
    """
    Compiled structure (§14.4):

        __draft__ → __critic_0__ → [pass → __finalize__]
                                  → [fail → __revise_0__]
        __revise_0__ → __critic_1__ → ...
        __critic_{N-1}__ → __finalize__ (forced after max rounds)
    """
    critic_model = config.get("critic_model", model)
    pass_threshold = config.get("pass_threshold", 0.8)
    max_rounds = min(config.get("max_rounds", 3), limits.max_iterations)

    nodes: dict[str, Any] = {}
    edges: list[dict[str, Any]] = []

    # Initial draft
    nodes["__draft__"] = _model_node(
        model,
        f"Goal: {goal}\nProduce a high-quality initial response.",
        "__draft_output__",
        labels={"jamjet.strategy.node": "draft_generation", "jamjet.strategy.event": "iteration_started"},
    )
    edges.append(_edge("__draft__", "__critic_0__"))

    for i in range(max_rounds):
        critic_id = f"__critic_{i}__"
        revise_id = f"__revise_{i}__"
        next_critic_id = f"__critic_{i + 1}__"

        draft_ref = "__draft_output__" if i == 0 else f"__revise_{i - 1}_output__"

        # Critic node
        nodes[critic_id] = _model_node(
            critic_model,
            (
                f"Goal: {goal}\n"
                f"Draft to evaluate: {{{{ state.{draft_ref} }}}}\n"
                f"Evaluate this draft against the goal. Pass threshold: {pass_threshold}.\n"
                'Output JSON: {"score": 0.0-1.0, "passed": true/false, "feedback": "..."}'
            ),
            f"__critic_{i}_verdict__",
            labels={
                "jamjet.strategy.node": "critic_eval",
                "jamjet.strategy.event": "critic_verdict",
                "jamjet.strategy.iteration": str(i),
            },
        )

        # After last round: always finalize
        if i == max_rounds - 1:
            edges.append(_edge(critic_id, "__finalize__"))
        else:
            # Condition: passed? → finalize, else → revise
            gate_id = f"__critic_gate_{i}__"
            nodes[gate_id] = _condition_node(
                f"state.__critic_{i}_verdict__.passed == true",
                [
                    {"condition": f"state.__critic_{i}_verdict__.passed == true", "target": "__finalize__"},
                    {"condition": None, "target": revise_id},
                ],
                labels={"jamjet.strategy.node": "critic_gate", "jamjet.strategy.iteration": str(i)},
            )
            edges.append(_edge(critic_id, gate_id))
            edges.append(_edge(gate_id, "__finalize__", f"state.__critic_{i}_verdict__.passed == true"))
            edges.append(_edge(gate_id, revise_id, None))

            # Revise node
            nodes[revise_id] = _model_node(
                model,
                (
                    f"Goal: {goal}\n"
                    f"Previous draft: {{{{ state.{draft_ref} }}}}\n"
                    f"Critic feedback: {{{{ state.__critic_{i}_verdict__.feedback }}}}\n"
                    "Revise the draft based on the feedback."
                ),
                f"__revise_{i}_output__",
                labels={
                    "jamjet.strategy.node": "revision",
                    "jamjet.strategy.event": "iteration_started",
                    "jamjet.strategy.iteration": str(i + 1),
                },
            )
            edges.append(_edge(revise_id, next_critic_id))

    # Finalizer
    nodes["__finalize__"] = _model_node(
        model,
        (f"Goal: {goal}\nFormat the final, polished response based on all revisions."),
        "result",
        labels={"jamjet.strategy.node": "finalizer", "jamjet.strategy.event": "strategy_completed"},
    )
    edges.append(_edge("__finalize__", "end"))

    nodes["__limit_exceeded__"] = _limit_exceeded_node()
    edges.append(_edge("__limit_exceeded__", "end"))

    return {
        "nodes": nodes,
        "edges": edges,
        "start_node": "__draft__",
    }


# ── reflection ─────────────────────────────────────────────────────────────────


def _compile_reflection(
    config: dict[str, Any],
    tools: list[str],
    model: str,
    limits: StrategyLimits,
    goal: str,
    agent_id: str,
) -> dict[str, Any]:
    """
    Compiled structure (§3.29):

        __execute__ → __reflect_0__ → __reflect_gate_0__ → [pass → __finalize__]
                                                          → [fail → __revise_0__]
        __revise_0__ → __reflect_1__ → __reflect_gate_1__ → ...
        __reflect_{N-1}__ → __finalize__ (forced after max rounds)
    """
    pass_threshold = config.get("pass_threshold", 0.8)
    max_rounds = min(config.get("max_rounds", 3), limits.max_iterations)

    nodes: dict[str, Any] = {}
    edges: list[dict[str, Any]] = []

    # Initial execution
    nodes["__execute__"] = _model_node(
        model,
        f"Goal: {goal}. Produce your best response.",
        "__execute_output__",
        labels={"jamjet.strategy.node": "initial_execution", "jamjet.strategy.event": "iteration_started"},
    )
    edges.append(_edge("__execute__", "__reflect_0__"))

    for i in range(max_rounds):
        reflect_id = f"__reflect_{i}__"
        revise_id = f"__revise_{i}__"
        gate_id = f"__reflect_gate_{i}__"
        next_reflect_id = f"__reflect_{i + 1}__"

        output_ref = "__execute_output__" if i == 0 else f"__revise_{i - 1}_output__"

        # Reflection node
        nodes[reflect_id] = _model_node(
            model,
            (
                f"Goal: {goal}. "
                f"Current output: {{{{ state.{output_ref} }}}}. "
                "Reflect: does this fully achieve the goal? "
                f"Score 0-1. Pass threshold: {pass_threshold}. "
                'Output JSON: {"score": 0.0-1.0, "passed": true/false, "gaps": "...", "suggestions": "..."}'
            ),
            f"__reflect_{i}_verdict__",
            labels={
                "jamjet.strategy.node": "reflection",
                "jamjet.strategy.event": "critic_verdict",
                "jamjet.strategy.iteration": str(i),
            },
        )

        # After last round: always finalize
        if i == max_rounds - 1:
            edges.append(_edge(reflect_id, "__finalize__"))
        else:
            # Condition: passed? → finalize, else → revise
            nodes[gate_id] = _condition_node(
                f"state.__reflect_{i}_verdict__.passed == true",
                [
                    {"condition": f"state.__reflect_{i}_verdict__.passed == true", "target": "__finalize__"},
                    {"condition": None, "target": revise_id},
                ],
                labels={"jamjet.strategy.node": "reflect_gate", "jamjet.strategy.iteration": str(i)},
            )
            edges.append(_edge(reflect_id, gate_id))
            edges.append(_edge(gate_id, "__finalize__", f"state.__reflect_{i}_verdict__.passed == true"))
            edges.append(_edge(gate_id, revise_id, None))

            # Revise node
            nodes[revise_id] = _model_node(
                model,
                (
                    f"Goal: {goal}. "
                    f"Previous: {{{{ state.{output_ref} }}}}. "
                    f"Reflection feedback: {{{{ state.__reflect_{i}_verdict__ }}}}. "
                    "Improve the response."
                ),
                f"__revise_{i}_output__",
                labels={
                    "jamjet.strategy.node": "revision",
                    "jamjet.strategy.event": "iteration_started",
                    "jamjet.strategy.iteration": str(i + 1),
                },
            )
            edges.append(_edge(revise_id, next_reflect_id))

    # Finalizer
    nodes["__finalize__"] = _model_node(
        model,
        (f"Goal: {goal}\nFormat the final, polished response based on all reflections and revisions."),
        "result",
        labels={"jamjet.strategy.node": "finalizer", "jamjet.strategy.event": "strategy_completed"},
    )
    edges.append(_edge("__finalize__", "end"))

    nodes["__limit_exceeded__"] = _limit_exceeded_node()
    edges.append(_edge("__limit_exceeded__", "end"))

    return {
        "nodes": nodes,
        "edges": edges,
        "start_node": "__execute__",
    }


# ── consensus ──────────────────────────────────────────────────────────────────


def _compile_consensus(
    config: dict[str, Any],
    tools: list[str],
    model: str,
    limits: StrategyLimits,
    goal: str,
    agent_id: str,
) -> dict[str, Any]:
    """
    Compiled structure (§3.30):

        __agent_0__ → __cost_guard_0__ → __agent_1__ → __cost_guard_1__ → ...
        __agent_{N-1}__ → __vote__ → __judge__ → __finalize__

    Agents run sequentially (IR has no native parallel node) with cost guards
    between each agent.  No cost guard on vote/judge (cheap nodes).
    """
    num_agents = config.get("num_agents", 3)
    judge_model = config.get("judge_model", model)

    nodes: dict[str, Any] = {}
    edges: list[dict[str, Any]] = []

    for i in range(num_agents):
        agent_id_node = f"__agent_{i}__"
        guard_id = f"__cost_guard_{i}__"

        # Agent node
        nodes[agent_id_node] = _model_node(
            model,
            (
                f"Goal: {goal}. "
                f"You are agent {i + 1} of {num_agents}. "
                "Independently produce your best answer."
            ),
            f"__agent_{i}_output__",
            labels={
                "jamjet.strategy.node": "parallel_agent",
                "jamjet.strategy.event": "iteration_started",
                "jamjet.strategy.iteration": str(i),
            },
        )

        # Cost guard after each agent (except last → goes straight to vote)
        if i < num_agents - 1:
            next_agent_id = f"__agent_{i + 1}__"
            nodes[guard_id] = _condition_node(
                "state.__cost_exceeded__ == true",
                _cost_guard_branches(next_agent_id),
                labels={"jamjet.strategy.node": "cost_guard", "jamjet.strategy.iteration": str(i)},
            )
            edges.append(_edge(agent_id_node, guard_id))
            edges.append(_edge(guard_id, "__limit_exceeded__", "state.__cost_exceeded__ == true"))
            edges.append(_edge(guard_id, next_agent_id, None))
        else:
            # Last agent → vote
            edges.append(_edge(agent_id_node, "__vote__"))

    # Build state refs for all agent outputs
    agent_refs = ", ".join(f"{{{{ state.__agent_{i}_output__ }}}}" for i in range(num_agents))

    # Vote node
    nodes["__vote__"] = _model_node(
        model,
        (
            f"Goal: {goal}. "
            f"You have {num_agents} candidate answers: {agent_refs}. "
            "Analyze each answer. Vote for the best one. "
            'Output JSON: {"votes": [{"agent": i, "score": 0.0-1.0, "reason": "..."}], "winner": i}'
        ),
        "__vote_result__",
        labels={"jamjet.strategy.node": "voting", "jamjet.strategy.event": "critic_verdict"},
    )
    edges.append(_edge("__vote__", "__judge__"))

    # Judge node
    nodes["__judge__"] = _model_node(
        judge_model,
        (
            f"Goal: {goal}. "
            "The voting result: {{ state.__vote_result__ }}. "
            "The winning answer: {{ state.__vote_result__.winner }}. "
            "Verify this is the best choice and produce the final refined answer."
        ),
        "__judge_output__",
        labels={"jamjet.strategy.node": "judge", "jamjet.strategy.event": "strategy_completed"},
    )
    edges.append(_edge("__judge__", "__finalize__"))

    # Finalizer
    nodes["__finalize__"] = _model_node(
        model,
        (f"Goal: {goal}\nFormat the final consensus answer based on the judge's output."),
        "result",
        labels={"jamjet.strategy.node": "finalizer", "jamjet.strategy.event": "strategy_completed"},
    )
    edges.append(_edge("__finalize__", "end"))

    nodes["__limit_exceeded__"] = _limit_exceeded_node()
    edges.append(_edge("__limit_exceeded__", "end"))

    return {
        "nodes": nodes,
        "edges": edges,
        "start_node": "__agent_0__",
    }


# ── debate ─────────────────────────────────────────────────────────────────────


def _compile_debate(
    config: dict[str, Any],
    tools: list[str],
    model: str,
    limits: StrategyLimits,
    goal: str,
    agent_id: str,
) -> dict[str, Any]:
    """
    Compiled structure (§3.31):

        __propose__ → __counter_0__ → __judge_0__ → __judge_gate_0__
            → [settled → __finalize__]
            → [continue → __cost_guard_0__ → __respond_0__]
        __respond_0__ → __counter_1__ → __judge_1__ → __judge_gate_1__ → ...
        __judge_{N-1}__ → __finalize__ (forced after max rounds)
    """
    judge_model = config.get("judge_model", model)
    max_rounds = min(config.get("max_rounds", 3), limits.max_iterations)

    nodes: dict[str, Any] = {}
    edges: list[dict[str, Any]] = []

    # Initial proposal
    nodes["__propose__"] = _model_node(
        model,
        f"Goal: {goal}. Present your initial position with reasoning and evidence.",
        "__propose_output__",
        labels={"jamjet.strategy.node": "proposer", "jamjet.strategy.event": "iteration_started"},
    )
    edges.append(_edge("__propose__", "__counter_0__"))

    for i in range(max_rounds):
        counter_id = f"__counter_{i}__"
        judge_id = f"__judge_{i}__"
        gate_id = f"__judge_gate_{i}__"
        respond_id = f"__respond_{i}__"
        guard_id = f"__cost_guard_{i}__"
        next_counter_id = f"__counter_{i + 1}__"

        # Determine the argument reference for this round
        if i == 0:
            argument_ref = "__propose_output__"
        else:
            argument_ref = f"__respond_{i - 1}_output__"

        # Counter-argument node
        nodes[counter_id] = _model_node(
            model,
            (
                f"Goal: {goal}. "
                f"Previous argument: {{{{ state.{argument_ref} }}}}. "
                "Challenge this argument. Identify weaknesses, present counter-arguments."
            ),
            f"__counter_{i}_output__",
            labels={
                "jamjet.strategy.node": "challenger",
                "jamjet.strategy.event": "tool_called",
                "jamjet.strategy.iteration": str(i),
            },
        )
        edges.append(_edge(counter_id, judge_id))

        # Judge node
        nodes[judge_id] = _model_node(
            judge_model,
            (
                f"Goal: {goal}. "
                f"Proposition: {{{{ state.{argument_ref} }}}}. "
                f"Counter-argument: {{{{ state.__counter_{i}_output__ }}}}. "
                "Judge: has the debate reached a well-supported conclusion? "
                'Output JSON: {"settled": true/false, "score": 0.0-1.0, "ruling": "..."}'
            ),
            f"__judge_{i}_ruling__",
            labels={
                "jamjet.strategy.node": "judge",
                "jamjet.strategy.event": "critic_verdict",
                "jamjet.strategy.iteration": str(i),
            },
        )

        # After last round: always finalize
        if i == max_rounds - 1:
            edges.append(_edge(judge_id, "__finalize__"))
        else:
            # Condition: settled? → finalize, else → cost_guard → respond
            nodes[gate_id] = _condition_node(
                f"state.__judge_{i}_ruling__.settled == true",
                [
                    {"condition": f"state.__judge_{i}_ruling__.settled == true", "target": "__finalize__"},
                    {"condition": None, "target": guard_id},
                ],
                labels={"jamjet.strategy.node": "judge_gate", "jamjet.strategy.iteration": str(i)},
            )
            edges.append(_edge(judge_id, gate_id))
            edges.append(_edge(gate_id, "__finalize__", f"state.__judge_{i}_ruling__.settled == true"))
            edges.append(_edge(gate_id, guard_id, None))

            # Cost guard between rounds
            nodes[guard_id] = _condition_node(
                "state.__cost_exceeded__ == true",
                _cost_guard_branches(respond_id),
                labels={"jamjet.strategy.node": "cost_guard", "jamjet.strategy.iteration": str(i)},
            )
            edges.append(_edge(guard_id, "__limit_exceeded__", "state.__cost_exceeded__ == true"))
            edges.append(_edge(guard_id, respond_id, None))

            # Respond node
            nodes[respond_id] = _model_node(
                model,
                (
                    f"Goal: {goal}. "
                    f"Counter-argument: {{{{ state.__counter_{i}_output__ }}}}. "
                    f"Judge feedback: {{{{ state.__judge_{i}_ruling__ }}}}. "
                    "Respond to the counter-argument, strengthening your position."
                ),
                f"__respond_{i}_output__",
                labels={
                    "jamjet.strategy.node": "responder",
                    "jamjet.strategy.event": "iteration_started",
                    "jamjet.strategy.iteration": str(i + 1),
                },
            )
            edges.append(_edge(respond_id, next_counter_id))

    # Finalizer
    nodes["__finalize__"] = _model_node(
        model,
        (f"Goal: {goal}\nSynthesize the debate into a final, well-supported answer."),
        "result",
        labels={"jamjet.strategy.node": "finalizer", "jamjet.strategy.event": "strategy_completed"},
    )
    edges.append(_edge("__finalize__", "end"))

    nodes["__limit_exceeded__"] = _limit_exceeded_node()
    edges.append(_edge("__limit_exceeded__", "end"))

    return {
        "nodes": nodes,
        "edges": edges,
        "start_node": "__propose__",
    }
