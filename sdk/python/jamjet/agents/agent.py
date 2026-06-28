"""
Agent — the simplest way to create a JamJet agent.

Compiles to the chosen reasoning strategy under the hood, giving you full
observability, durability, and tool-use without any boilerplate.

Default strategy is ``plan-and-execute`` (§14.3): generates a structured plan
first, then executes each step in sequence. Use ``strategy="react"`` for
tight tool-loop tasks, or ``strategy="critic"`` for quality-sensitive output.

Usage::

    from jamjet import Agent, tool

    @tool
    async def web_search(query: str) -> str:
        return f"Search results for: {query}"

    agent = Agent(
        "researcher",
        model="claude-sonnet-4-6",
        tools=[web_search],
        instructions="You are a research assistant.",
    )

    result = await agent.run("Summarize the latest trends in agent runtimes")
"""

from __future__ import annotations

import asyncio
import json
import time
import warnings
from collections.abc import AsyncIterator, Callable
from typing import TYPE_CHECKING, Any

from jamjet.agents.governance import Budget, GovernanceConfig, normalize_governance
from jamjet.compiler.strategies import StrategyLimits
from jamjet.runtime.local import LocalRuntime
from jamjet.tools.decorators import ToolDefinition

if TYPE_CHECKING:
    from jamjet.spec import AgentSpec

# Terminal execution statuses (WorkflowStatus, snake_case) the durable poll stops on.
_TERMINAL_STATUSES = frozenset({"completed", "failed", "cancelled", "limit_exceeded"})
# How often run_durable polls get_execution while waiting for a terminal state.
_POLL_INTERVAL_SECONDS = 0.5


class Agent:
    """
    A JamJet agent — tools + model + instructions + strategy → run.

    Default strategy is ``plan-and-execute``: generates a plan first, then
    executes each step. Override with ``strategy="react"`` for tool-heavy
    loops or ``strategy="critic"`` for draft-and-refine quality tasks.

    For full graph control, use :class:`jamjet.Workflow` directly.
    """

    def __init__(
        self,
        name: str,
        *,
        model: str,
        tools: list[Callable[..., Any]],
        instructions: str = "",
        strategy: str = "plan-and-execute",
        max_iterations: int = 10,
        max_cost_usd: float = 1.0,
        timeout_seconds: int = 300,
        on_limit_exceeded: Callable[[str | None, str, Any, Any], str | None] | None = None,
        # Governance knobs (T3-1).  T3-2..6 read self.governance to enforce.
        policy: str | dict | None = None,
        approval_required: bool | list[str] = False,
        budget: Budget | float | int | dict | None = None,
        pii: bool = True,
        audit: bool = True,
        receipts: bool = True,
        # Optional sink for AgentBoundary receipts (T3-4).  When set, every
        # minted receipt is also shipped here (e.g. a JSONL writer); the receipt
        # is always attached to the run result regardless.  Defaults to None so
        # receipts ship nowhere noisy by default while still being produced.
        receipt_emitter: Callable[[dict[str, Any]], None] | None = None,
    ) -> None:
        self.name = name
        self.model = model
        self.instructions = instructions
        self.strategy = strategy
        self._on_limit_exceeded = on_limit_exceeded
        self._receipt_emitter = receipt_emitter
        self.limits = StrategyLimits(
            max_iterations=max_iterations,
            max_cost_usd=max_cost_usd,
            timeout_seconds=timeout_seconds,
        )

        # Build the immutable governance config.
        #
        # budget vs max_cost_usd reconciliation: max_cost_usd is the legacy
        # StrategyLimits cap (used by strategy runners for iteration budgets).
        # budget= is the new governance-layer cap read by the seam middleware
        # (T3-2) and compiled into the durable IR (T3-5).  When budget= is not
        # set but max_cost_usd is explicitly provided, we fold the legacy value
        # into GovernanceConfig.budget.cost_usd so the governance layer inherits
        # the same ceiling without requiring callers to set both.  T3-2 will
        # reconcile and document the authoritative enforcement point.
        _effective_budget: Budget | float | int | dict | None = budget
        if _effective_budget is None and max_cost_usd != 1.0:
            # Non-default max_cost_usd -> carry it forward as the budget cap.
            _effective_budget = max_cost_usd

        self.governance: GovernanceConfig = normalize_governance(
            policy=policy,
            approval_required=approval_required,
            budget=_effective_budget,
            pii=pii,
            audit=audit,
            receipts=receipts,
        )

        # Resolve tool definitions from decorated functions
        self._tools: list[ToolDefinition] = []
        for t in tools:
            defn = getattr(t, "_jamjet_tool", None)
            if defn is None:
                raise TypeError(f"{t!r} is not a @tool-decorated function. Wrap it with @jamjet.tool first.")
            self._tools.append(defn)

    @property
    def tool_names(self) -> list[str]:
        return [t.name for t in self._tools]

    def compile(self) -> AgentSpec:
        """Compile this agent to an AgentSpec."""
        from jamjet.model.types import api_key_env_for, parse_model_ref, provider_literal_for  # noqa: PLC0415
        from jamjet.spec import AgentSpec, AgentStrategy, LLMConfig, ToolSpec  # noqa: PLC0415

        ref = parse_model_ref(self.model)
        return AgentSpec(
            name=self.name,
            instructions=self.instructions,
            llm=LLMConfig(
                provider=provider_literal_for(self.model),
                model=ref.litellm_model,
                api_key_env=api_key_env_for(ref.provider),
            ),
            tools=[
                ToolSpec(
                    name=td.name,
                    description=td.description,
                    input_schema=td.input_schema,
                    handler_ref=f"{td.fn.__module__}:{td.fn.__qualname__}",
                )
                for td in self._tools
            ],
            strategy=AgentStrategy(name=self.strategy),
            limits={
                "max_iterations": self.limits.max_iterations,
                "max_cost_usd": self.limits.max_cost_usd,
                "timeout_seconds": self.limits.timeout_seconds,
            },
        )

    # ── Governance: receipts on by default (T3-4) ──────────────────────────

    def _maybe_mint_receipt(self, prompt: str, output: Any) -> dict[str, Any] | None:
        """Mint an AgentBoundary receipt for this turn when receipts are enabled.

        Receipts are ON by default (``GovernanceConfig.receipts``); a plain
        ``Agent()`` produces one without the developer opting in. Returns the
        receipt dict (also attached to the :class:`AgentResult` and shipped to
        ``receipt_emitter`` if set), or ``None`` when ``receipts=False``. Reuses
        the exact mint path :func:`jamjet.gate` uses, so agent receipts and
        gated-tool receipts are consistent.
        """
        if not self.governance.receipts:
            return None
        from jamjet.agents.receipts import mint_agent_receipt  # noqa: PLC0415

        return mint_agent_receipt(
            agent_name=self.name,
            model=self.model,
            prompt=prompt,
            output="" if output is None else str(output),
            emitter=self._receipt_emitter,
        )

    # ── Public run interface ───────────────────────────────────────────────

    async def run(self, prompt: str) -> AgentResult:
        """
        Run the agent on a single prompt via LocalRuntime.

        Compiles to AgentSpec, hands off to LocalRuntime which dispatches to
        the appropriate strategy runner. Emits a signed-audit-aligned
        AgentBoundary receipt for the turn (on by default).
        """
        # T3-6: approval_required parity — the in-process path cannot enforce
        # tool-level approval gates (the @gate mechanism is opt-in per function;
        # the durable Rust engine enforces require_approval_for via the IR).
        # Fail LOUD rather than silently no-op so the developer knows approval
        # won't fire here.  See follow-up F-t3-inprocess-approval for full
        # in-process enforcement.
        ar = self.governance.approval_required
        if ar is not False and ar != []:
            warnings.warn(
                f"Agent {self.name!r}: approval_required is set but agent.run() uses "
                "the in-process path, which does not enforce approval gates. "
                "Use agent.run_durable() — the durable IR carries "
                "require_approval_for and the Rust engine enforces it fail-closed. "
                "Follow-up: F-t3-inprocess-approval.",
                UserWarning,
                stacklevel=2,
            )
        spec = self.compile()
        rt = LocalRuntime()
        # T3-7: thread governance into the in-process seam so budget / allowlist
        # / PII enforce on agent.run() (in-process) at parity with run_durable()
        # (the durable IR).  Without this the seam was built allow-all / no-budget
        # and the budget + policy knobs silently no-opped on the in-process path
        # (the gap deferred from T3-2).
        result = await rt.execute(spec, prompt, governance=self.governance)
        receipt = self._maybe_mint_receipt(prompt, result.output)
        return AgentResult(
            output=result.output,
            tool_calls=[tc.model_dump() for tc in result.tool_calls],
            ir=spec.model_dump(),
            duration_us=result.duration_ms * 1000,
            receipt=receipt,
        )

    def run_sync(self, prompt: str) -> AgentResult:
        """Synchronous wrapper around :meth:`run` for scripts and notebooks."""
        return asyncio.run(self.run(prompt))

    async def run_durable(
        self,
        prompt: str,
        *,
        max_turns: int = 8,
        runtime_url: str = "http://127.0.0.1:7700",
    ) -> AgentResult:
        """Run the agent durably on the JamJet engine, mirroring :meth:`run`.

        Compiles the agent to an agent-loop IR (``model -> tools -> model``,
        statically unrolled and bounded by *max_turns*), registers it, then
        starts an execution seeded with the system+user ``messages`` and the
        ``{name: "module:function"}`` tool-resolver map. It polls the execution
        to a terminal state and extracts an :class:`AgentResult` with the SAME
        fields as the in-process :meth:`run` (final content, the tool calls made,
        the IR, and the wall-clock duration).

        Unlike :meth:`run` (a pure in-process loop), this routes every model call
        and tool dispatch through the durable event-sourced engine, so the run
        gets the event log, replay, idempotency, park-on-429, and artifacts for
        free.

        v1 limitations
        --------------
        - **Static unroll, no early exit.** The loop runs up to *max_turns* tool
          turns plus a final answer-only model turn, and currently does NOT
          short-circuit when the model returns a final answer early; the engine has
          no edge-condition evaluator yet (F-2j-dynamic-loop). A real model is
          re-invoked on every remaining turn, so cost scales with *max_turns* and
          the answer can drift across turns. Size *max_turns* to the conversation
          depth you actually expect.
        - **Requires running services.** A live engine at *runtime_url*, a
          ``jamjet worker`` draining the ``python_tool`` queue (to execute the
          ``@tool`` functions), and the model sidecar (``JAMJET_MODEL_SEAM_URL``,
          which forwards tools to the provider) must all be running.
        - **The read is the final state.** The returned answer is the durable
          execution's final state — the final model turn's output, which consumes
          the last tool results — not a per-turn early-stop result.

        Raises
        ------
        RuntimeError
            If the execution reaches a non-``completed`` terminal state
            (``failed`` / ``cancelled`` / ``limit_exceeded``).
        TimeoutError
            If no terminal state is reached within the agent's
            ``timeout_seconds`` limit.
        """
        from jamjet.client import JamjetClient  # noqa: PLC0415 - patch point / avoid import cycle
        from jamjet.compiler.agent_ir import build_initial_state, compile_agent_to_ir  # noqa: PLC0415

        ir = compile_agent_to_ir(self, prompt, max_turns)
        initial_input = build_initial_state(self, prompt)
        workflow_id: str = ir["workflow_id"]
        workflow_version: str | None = ir.get("version")

        t0 = time.monotonic()
        async with JamjetClient(runtime_url) as client:
            # Register the compiled IR, then start an execution seeded with the
            # running messages + tool-resolver map (build_initial_state).
            await client.create_workflow(ir)
            started = await client.start_execution(workflow_id, initial_input, workflow_version=workflow_version)
            exec_id = started.get("execution_id")
            if not exec_id:
                raise RuntimeError(f"start_execution returned no execution_id: {started!r}")

            execution = await self._poll_to_terminal(client, exec_id)
            # Events carry artifact-resolved NodeCompleted outputs (server-side),
            # used only as a fallback if a spilled answer sentinel slips through.
            try:
                events = (await client.get_events(exec_id)).get("events", [])
            except Exception:  # noqa: BLE001 - events are a best-effort fallback
                events = []

        duration_us = int((time.monotonic() - t0) * 1_000_000)
        result = self._extract_result(execution, events, ir, duration_us)
        # Mint the turn receipt from the durable run's extracted result (on by
        # default), mirroring the in-process run().
        result.receipt = self._maybe_mint_receipt(prompt, result.output)
        return result

    async def _poll_to_terminal(self, client: Any, exec_id: str) -> dict[str, Any]:
        """Poll ``get_execution`` until the execution reaches a terminal state."""
        timeout_s = self.limits.timeout_seconds
        # Monotonic deadline: time spent inside slow get_execution() calls counts
        # toward the timeout, so a stalled run can't blow past timeout_seconds.
        deadline = time.monotonic() + timeout_s
        while True:
            execution = await client.get_execution(exec_id)
            status = execution.get("status", "unknown")
            if status in _TERMINAL_STATUSES:
                return execution
            if time.monotonic() >= deadline:
                raise TimeoutError(
                    f"durable agent run {exec_id} did not reach a terminal state "
                    f"within {timeout_s}s (last status: {status!r})"
                )
            await asyncio.sleep(_POLL_INTERVAL_SECONDS)

    def _extract_result(
        self,
        execution: dict[str, Any],
        events: list[dict[str, Any]],
        ir: dict[str, Any],
        duration_us: int,
    ) -> AgentResult:
        """Build an :class:`AgentResult` from a terminal durable execution.

        The final answer is the last model turn's content
        (``current_state["last_model_output"]``, written inline by the Model
        executor; falls back to the last assistant message). The tool-call trace
        is reconstructed from the accumulated ``messages``. Field shape matches
        the in-process :meth:`run` result exactly.
        """
        status = execution.get("status")
        if status != "completed":
            detail = execution.get("detail") or status
            raise RuntimeError(f"durable agent run ended in non-completed state: {detail}")

        state = execution.get("current_state") or {}
        messages = state.get("messages") or []

        output = state.get("last_model_output")
        if output is None:
            output = self._last_assistant_content(messages)
        output = self._resolve_artifact_sentinel(output, events)

        return AgentResult(
            output=output if isinstance(output, str) else ("" if output is None else str(output)),
            tool_calls=self._tool_calls_from_messages(messages),
            ir=ir,
            duration_us=duration_us,
        )

    @staticmethod
    def _last_assistant_content(messages: list[dict[str, Any]]) -> str | None:
        """The content of the last assistant message that carried text."""
        for msg in reversed(messages):
            if msg.get("role") == "assistant" and msg.get("content"):
                return msg["content"]
        return None

    @staticmethod
    def _tool_calls_from_messages(messages: list[dict[str, Any]]) -> list[dict[str, Any]]:
        """Reconstruct the in-process tool-call trace from the message history.

        Pairs each ``role: tool`` result with the arguments from the assistant
        ``tool_calls`` that requested it (matched by ``tool_call_id``), yielding
        dicts with the same keys as the in-process ``ToolCallRecord.model_dump()``
        (``tool`` / ``input`` / ``output`` / ``duration_us``). Per-call durations
        are not carried in the message history, so ``duration_us`` is ``0``.
        """
        args_by_id: dict[str, Any] = {}
        for msg in messages:
            if msg.get("role") != "assistant":
                continue
            for call in msg.get("tool_calls") or []:
                fn = call.get("function") or {}
                raw = fn.get("arguments")
                try:
                    parsed = json.loads(raw) if isinstance(raw, str) else (raw or {})
                except (TypeError, ValueError):
                    parsed = {}
                if call.get("id") is not None:
                    args_by_id[call["id"]] = parsed

        calls: list[dict[str, Any]] = []
        for msg in messages:
            if msg.get("role") != "tool":
                continue
            calls.append(
                {
                    "tool": msg.get("name"),
                    "input": args_by_id.get(msg.get("tool_call_id"), {}),
                    "output": msg.get("content"),
                    "duration_us": 0,
                }
            )
        return calls

    @staticmethod
    def _resolve_artifact_sentinel(value: Any, events: list[dict[str, Any]]) -> Any:
        """Recover the answer if it surfaced as a 2i artifact spill sentinel.

        ``last_model_output`` is written inline (never spilled to the artifact
        store), so it is normally already the answer text. As a guard, if a
        ``{"$artifact": ...}`` sentinel slips through, recover the resolved text
        from the most recent NodeCompleted event's ``output.content`` (the
        ``get_events`` endpoint resolves sentinels server-side).
        """
        if not (isinstance(value, dict) and "$artifact" in value):
            return value
        for ev in reversed(events):
            kind = ev.get("kind") or {}
            if kind.get("type") != "node_completed":
                continue
            out = kind.get("output")
            if isinstance(out, dict) and "content" in out and "$artifact" not in out:
                return out["content"]
        return value

    async def stream(
        self,
        prompt: str,
        *,
        model: Any | None = None,
    ) -> AsyncIterator[Any]:
        """Stream token deltas for a single turn.

        Streaming is a view; durability records the completed turn (resume
        returns the checkpoint, it does not re-stream). The looped, durable
        stream over the full agent run lands in Track 2.
        """
        from jamjet.model.defaults import default_model_middleware  # noqa: PLC0415
        from jamjet.model.seam import Model  # noqa: PLC0415
        from jamjet.model.types import ModelRequest, parse_model_ref  # noqa: PLC0415

        spec = self.compile()
        seam = model or Model(middleware=default_model_middleware(self.governance))
        request = ModelRequest(
            ref=parse_model_ref(spec.llm.model),
            messages=[
                {"role": "system", "content": self.instructions or "You are a helpful assistant."},
                {"role": "user", "content": prompt},
            ],
            temperature=spec.llm.temperature,
            max_tokens=spec.llm.max_tokens,
            stream=True,
        )
        async for chunk in seam.stream(request):
            yield chunk

    def __repr__(self) -> str:
        return f"Agent(name={self.name!r}, model={self.model!r}, tools={self.tool_names}, strategy={self.strategy!r})"


class AgentResult:
    """Result returned by Agent.run().

    ``receipt`` is the AgentBoundary Action Receipt minted for the turn (on by
    default), or ``None`` when ``receipts=False``. It is a portable, validatable
    proof of the action -- ``agentboundary.validate_receipt`` /
    ``check_conformance`` accept it, and its ``arguments_hash`` binds it to this
    prompt/agent/model.
    """

    def __init__(
        self,
        output: str,
        tool_calls: list[dict[str, Any]],
        ir: Any,
        duration_us: float = 0.0,
        receipt: dict[str, Any] | None = None,
    ) -> None:
        self.output = output
        self.tool_calls = tool_calls
        self.ir = ir
        self.duration_us = duration_us
        self.receipt = receipt

    def __str__(self) -> str:
        return self.output

    def __repr__(self) -> str:
        return f"AgentResult(output={self.output!r}, tool_calls={len(self.tool_calls)})"
