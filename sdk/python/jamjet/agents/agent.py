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
from pathlib import Path
from typing import TYPE_CHECKING, Any

from jamjet.agents.governance import Budget, GovernanceConfig, normalize_governance
from jamjet.agents.session import Session, SessionStore, persist_session_turn, seed_messages_for_run
from jamjet.compiler.strategies import StrategyLimits
from jamjet.runtime.local import LocalRuntime
from jamjet.tools.decorators import ToolDefinition

if TYPE_CHECKING:
    from jamjet.memory.engram_bridge import AgentMemory
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
        # T4-2: optional SessionStore for resolving str session ids and for
        # auto-saving sessions after run()/run_durable().  When None, a default
        # SessionStore() (~/.jamjet/sessions.db) is created lazily on first use.
        # Callers that pass Session objects directly and manage persistence
        # themselves can still omit this; the agent only touches the store when
        # session= is set on a run call.
        session_store: SessionStore | None = None,
        # T4-3: opt-in Engram memory with an AUTOMATIC, GOVERNED retrieve-at-
        # start / record-at-end loop keyed by the session id.  OFF by default.
        #   None / False        -> no memory (default; no behaviour change).
        #   True                -> the default embedded Engram bridge (opened
        #                          lazily on first run; FAILS LOUD here if the
        #                          optional jamjet-engram extra is not installed).
        #   an AgentMemory-like -> used as-is (duck-typed, so a fake can be
        #                          injected in tests without a real Engram).
        memory: bool | AgentMemory | None = None,
    ) -> None:
        self.name = name
        self.model = model
        self.instructions = instructions
        self.strategy = strategy
        self._on_limit_exceeded = on_limit_exceeded
        self._receipt_emitter = receipt_emitter
        self._session_store: SessionStore | None = session_store

        # T4-3: resolve the memory= knob.  See the constructor docstring above.
        # The default embedded Engram (memory=True) is opened LAZILY on the first
        # run because Engram.open is async and __init__ is sync; we only verify
        # the optional extra is importable HERE so the failure is loud and early.
        self._memory_enabled: bool = False
        self._memory_resolved: AgentMemory | None = None
        self._engram: Any | None = None  # the lazily-opened embedded Engram (memory=True)
        if memory is None or memory is False:
            self._memory_enabled = False
        elif memory is True:
            try:
                import engram  # noqa: F401, PLC0415
            except ImportError as e:
                raise ImportError(
                    "Agent(memory=True) requires the optional 'memory' extra "
                    "(jamjet-engram). Install with: pip install 'jamjet[memory]'."
                ) from e
            self._memory_enabled = True
        else:
            # A memory backend object: a real AgentMemory or a duck-typed fake.
            self._memory_resolved = memory
            self._memory_enabled = True

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

    # ── Governance: per-action signed audit on by default (C1) ─────────────

    def _maybe_emit_audit(
        self,
        prompt: str,
        result: AgentResult,
        *,
        execution_id: str,
    ) -> list[Any] | None:
        """Emit a signed, hash-chained audit record for this run's actions.

        Audit is ON by default (``GovernanceConfig.audit``): a plain governed
        ``Agent`` produces one :class:`~jamjet.agents.audit.AuditAction` per tool
        call plus one for the model turn, sealed into a tamper-evident chain and
        signed with the keyed HMAC (``JAMJET_AUDIT_SIGNING_KEY``; unsigned-but-
        chained with a loud warning until a key is provisioned). Returns the list
        of sealed actions (also attached to :class:`AgentResult.audit`), or
        ``None`` when ``audit=False``. This is the in-process / SDK audit path;
        the durable engine additionally signs approval-decision events, and
        per-node engine-internal audit emission is tracked as F-t3-audit-emit.
        """
        if not self.governance.audit:
            return None
        from jamjet.agents.audit import build_action_chain  # noqa: PLC0415

        return build_action_chain(
            agent_name=self.name,
            model=self.model,
            execution_id=execution_id,
            prompt=prompt,
            output=result.output,
            tool_calls=result.tool_calls,
        )

    # ── Session helpers (T4-2) ────────────────────────────────────────────

    def _get_default_store(self) -> SessionStore:
        """Return the agent's SessionStore, creating a default one lazily."""
        if self._session_store is None:
            self._session_store = SessionStore()
        return self._session_store

    def _resolve_session(self, session: Session | str | None) -> Session | None:
        """Return a Session object.

        - ``None`` -> ``None`` (no session, default path unchanged).
        - :class:`Session` -> returned as-is (caller owns persistence).
        - ``str`` -> loaded from the agent's session store; raises
          ``ValueError`` if not found.
        """
        if session is None:
            return None
        if isinstance(session, Session):
            return session
        # str session id: resolve from store
        store = self._get_default_store()
        loaded = store.load(session)
        if loaded is None:
            raise ValueError(
                f"Session {session!r} not found in the agent's SessionStore. "
                "Create it first with SessionStore.create()."
            )
        return loaded

    # ── Memory: governed auto retrieve/record loop (T4-3) ──────────────────

    async def _ensure_memory(self) -> AgentMemory | None:
        """Return the agent's memory backend, opening the default Engram lazily.

        - memory off          -> ``None``.
        - an injected backend -> returned as-is (the caller owns its lifecycle).
        - ``memory=True``     -> open the embedded Engram ONCE and cache an
          :class:`~jamjet.memory.engram_bridge.AgentMemory` over it.  The same
          instance is reused across runs so memory persists; close it with
          :meth:`aclose`.  FAILS LOUD if the optional extra is missing (already
          verified at ``__init__``, re-guarded here for safety).
        """
        if not self._memory_enabled:
            return None
        if self._memory_resolved is not None:
            return self._memory_resolved
        try:
            from engram import Engram  # noqa: PLC0415
            from engram import Scope as EngramScope  # noqa: PLC0415
        except ImportError as e:  # pragma: no cover - guarded at __init__
            raise ImportError(
                "Agent(memory=True) requires the optional 'memory' extra "
                "(jamjet-engram). Install with: pip install 'jamjet[memory]'."
            ) from e
        from jamjet.memory.engram_bridge import AgentMemory  # noqa: PLC0415
        from jamjet.spec import MemoryConfig  # noqa: PLC0415

        db_path = Path.home() / ".jamjet" / "engram.db"
        db_path.parent.mkdir(parents=True, exist_ok=True)
        engram = await Engram.open(path=str(db_path))
        self._engram = engram
        # The per-run memory KEY is the stable session id, applied at call time
        # via as_scope() (Engram retrieval filters by scope); org defaults here.
        self._memory_resolved = AgentMemory(
            engram,
            scope=EngramScope(user_id="default", org_id="default"),
            config=MemoryConfig(),
        )
        return self._memory_resolved

    async def _prepare_run(
        self,
        session: Session | str | None,
        prompt: str,
    ) -> tuple[Session | None, AgentMemory | None, list[dict[str, Any]] | None]:
        """Resolve session + memory and build the seed messages for a run.

        Returns ``(resolved_session, memory, initial_messages)`` shared by both
        :meth:`run` (in-process) and :meth:`run_durable` (durable):

        - *memory* is the resolved backend, or ``None`` when memory is off.
        - When memory is ON it REQUIRES a ``session`` to key on — fail LOUD if
          none was given.  Memory is keyed by the stable ``session.id`` so it
          persists and is retrievable across runs; an ephemeral per-run key
          would never retrieve, silently defeating cross-run memory.
        - *initial_messages* carries the session thread (T4-2) and, when memory
          is on, the AUTO-RETRIEVED "Relevant memory" block prepended after the
          system instruction.  ``None`` means the default scratch seed (no
          session, no memory) — existing behaviour unchanged.
        """
        resolved_session = self._resolve_session(session)

        # Memory requires a session to key on.  Do this fail-loud check BEFORE
        # opening any Engram handle: ``memory=True`` opens a DB handle inside
        # ``_ensure_memory`` (cached for the agent's lifetime), so raising first
        # means the no-session error path never leaks an open handle.  We key off
        # ``self._memory_enabled`` (set at __init__) rather than the resolved
        # backend so we never construct it on the error path.
        if self._memory_enabled and resolved_session is None:
            raise RuntimeError(
                f"Agent {self.name!r}: memory= is enabled but no session= was "
                "provided to the run. Memory is keyed by the stable session.id so "
                "it can persist and be retrieved across runs; an ephemeral per-run "
                "key would never retrieve. Pass session=<Session or id> to "
                "run()/run_durable(), or drop memory= for a stateless run."
            )

        memory = await self._ensure_memory()

        system = self.instructions or "You are a helpful assistant."
        initial_messages: list[dict[str, Any]] | None = None
        if resolved_session is not None:
            initial_messages = seed_messages_for_run(resolved_session, system, prompt)

        if memory is not None and resolved_session is not None:
            ctx = await self._memory_retrieve(memory, resolved_session.id, prompt)
            if ctx:
                initial_messages = self._inject_memory_block(initial_messages, ctx)

        return resolved_session, memory, initial_messages

    async def _memory_retrieve(self, memory: AgentMemory, session_id: str, query: str) -> str:
        """Retrieve a context block for *query*, keyed (scoped) by *session_id*.

        The read is scoped to the stable session id via ``as_scope`` (Engram
        retrieval filters by scope — see ``memory/engram_bridge.py``), so a run
        only sees memory written under the same session.  Returns the
        token-budgeted context block (possibly an empty string).
        """
        with memory.as_scope(user_id=session_id) as scoped:
            ctx = await scoped.context(query=query)
        return ctx or ""

    async def _record_turn(
        self,
        memory: AgentMemory,
        session_id: str,
        prompt: str,
        output: Any,
    ) -> None:
        """Record this turn to memory, keyed by *session_id*, PII-REDACTED.

        Governance: the user prompt and the assistant output are PII-redacted
        (the Track-3 redactor, :meth:`_redact_for_memory`) BEFORE the write, so
        raw PII never lands in the durable temporal KG.  Both records are scoped
        to the stable session id.
        """
        user_text = self._redact_for_memory(prompt)
        with memory.as_scope(user_id=session_id) as scoped:
            await scoped.record(user_text, role="user")
            if output is not None and str(output):
                assistant_text = self._redact_for_memory(str(output))
                await scoped.record(assistant_text, role="assistant")

    def _redact_for_memory(self, text: str) -> str:
        """Redact PII from *text* before a memory write (governed write).

        Uses the same conservative, high-signal PII types as the model seam
        (``jamjet.model.pii``) so memory redaction matches outbound-prompt
        redaction.  Respects ``governance.pii``: when PII governance is disabled
        the caller has opted out and the text is written verbatim, consistent
        with the model seam (which only redacts when ``pii`` is on).
        """
        if not self.governance.pii:
            return text
        from jamjet.cloud.middleware.pii import RegexDetector  # noqa: PLC0415
        from jamjet.model.pii import _DEFAULT_PII_TYPES  # noqa: PLC0415

        return RegexDetector(types=list(_DEFAULT_PII_TYPES)).redact(text)

    @staticmethod
    def _inject_memory_block(messages: list[dict[str, Any]] | None, ctx: str) -> list[dict[str, Any]]:
        """Insert a 'Relevant memory' system block into the seed *messages*.

        The block is placed right after the agent's system instruction so the
        model sees prior-session knowledge before the conversation thread.
        """
        block = {"role": "system", "content": f"Relevant memory from prior sessions:\n{ctx}"}
        msgs = list(messages or [])
        if msgs and msgs[0].get("role") == "system":
            return [msgs[0], block, *msgs[1:]]
        return [block, *msgs]

    async def aclose(self) -> None:
        """Close the embedded Engram opened by ``memory=True`` (if any).

        Injected memory backends are owned by the caller and are NOT closed
        here.  Safe to call when memory is off (no-op).
        """
        engram = self._engram
        if engram is not None:
            self._engram = None
            await engram.close()

    # ── Public run interface ───────────────────────────────────────────────

    async def run(self, prompt: str, *, session: Session | str | None = None) -> AgentResult:
        """
        Run the agent on a single prompt via LocalRuntime.

        Compiles to AgentSpec, hands off to LocalRuntime which dispatches to
        the appropriate strategy runner. Emits a signed-audit-aligned
        AgentBoundary receipt for the turn (on by default).

        T4-2 — Session continuation
        ---------------------------
        When *session* is provided (a :class:`~jamjet.agents.session.Session`
        object or a session id string), the run CONTINUES the session's
        conversation thread instead of re-seeding from scratch.  The model
        receives the full prior thread followed by the new user prompt.  After
        the run the session is updated with the new turn and persisted.

        T4-3 — Memory (``memory=``)
        ---------------------------
        When the agent was built with ``memory=`` (a bool or an
        :class:`~jamjet.memory.engram_bridge.AgentMemory`), the run runs an
        AUTOMATIC, GOVERNED loop keyed by the stable ``session.id``: it RETRIEVES
        a "Relevant memory" context block and injects it before the thread
        (retrieve-at-start), then RECORDS the turn after the run
        (record-at-end), PII-REDACTED so no raw PII is written to the temporal
        KG.  Memory requires a ``session`` to key on (fail-loud if absent) and is
        OFF by default.

        Args:
            prompt: The user turn to run.
            session: Optional session to continue.  Pass a
                     :class:`~jamjet.agents.session.Session` object (caller
                     manages the store) or a ``str`` session id (resolved via
                     the agent's ``session_store``).  ``None`` (default) runs
                     from scratch — existing behaviour unchanged.
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

        # T4-2/T4-3: resolve session + memory and build the seed messages.  The
        # seed carries the session thread (T4-2) plus, when memory is on, the
        # auto-retrieved "Relevant memory" block (T4-3 retrieve-at-start).
        resolved_session, memory, initial_messages = await self._prepare_run(session, prompt)

        spec = self.compile()
        rt = LocalRuntime()
        # T3-7: thread governance into the in-process seam so budget / allowlist
        # / PII enforce on agent.run() (in-process) at parity with run_durable()
        # (the durable IR).  Without this the seam was built allow-all / no-budget
        # and the budget + policy knobs silently no-opped on the in-process path
        # (the gap deferred from T3-2).
        result = await rt.execute(
            spec,
            prompt,
            governance=self.governance,
            initial_messages=initial_messages,
        )
        receipt = self._maybe_mint_receipt(prompt, result.output)
        agent_result = AgentResult(
            output=result.output,
            tool_calls=[tc.model_dump() for tc in result.tool_calls],
            ir=spec.model_dump(),
            duration_us=result.duration_ms * 1000,
            receipt=receipt,
        )
        agent_result.audit = self._maybe_emit_audit(prompt, agent_result, execution_id=result.execution_id)

        # T4-2: persist the session turn (in-process path: no full_messages).
        if resolved_session is not None:
            store = self._get_default_store()
            persist_session_turn(
                resolved_session,
                prompt,
                agent_result.output,
                result.execution_id,
                full_messages=None,  # in-process path; reconstruct from prompt+output
                store=store,
            )

        # T4-3: record-at-end — write this turn to memory keyed by session.id,
        # PII-redacted (governed write).  No-op when memory is off.
        if memory is not None and resolved_session is not None:
            await self._record_turn(memory, resolved_session.id, prompt, agent_result.output)

        return agent_result

    def run_sync(self, prompt: str) -> AgentResult:
        """Synchronous wrapper around :meth:`run` for scripts and notebooks."""
        return asyncio.run(self.run(prompt))

    async def run_durable(
        self,
        prompt: str,
        *,
        max_turns: int = 8,
        runtime_url: str = "http://127.0.0.1:7700",
        session: Session | str | None = None,
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

        T4-2 — Session continuation
        ---------------------------
        Same carried-state contract as :meth:`run`.  When *session* is provided
        the run is seeded from the session's persisted ``messages`` thread plus
        the new user prompt.  After the run the full ``messages`` ledger from
        ``current_state`` (which includes all tool turns) is persisted back to
        the session.

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

        # T4-2/T4-3: resolve session + memory and build the seed (session thread
        # plus the auto-retrieved "Relevant memory" block when memory is on).
        resolved_session, memory, seeded_messages = await self._prepare_run(session, prompt)
        if seeded_messages is not None:
            initial_input = build_initial_state(self, prompt, initial_messages=seeded_messages)
        else:
            initial_input = build_initial_state(self, prompt)

        ir = compile_agent_to_ir(self, prompt, max_turns)
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
        # Mint the turn receipt + emit the per-action signed audit chain from the
        # durable run's extracted result (both on by default), mirroring run().
        result.receipt = self._maybe_mint_receipt(prompt, result.output)
        result.audit = self._maybe_emit_audit(prompt, result, execution_id=exec_id)

        # T4-2: persist the session with the full messages from the engine
        # (durable path has the complete tool-interleaved thread in current_state).
        if resolved_session is not None:
            state = execution.get("current_state") or {}
            full_messages: list[dict[str, Any]] | None = state.get("messages")
            store = self._get_default_store()
            persist_session_turn(
                resolved_session,
                prompt,
                result.output,
                exec_id,
                full_messages=full_messages,
                store=store,
            )

        # T4-3: record-at-end — write this turn to memory keyed by session.id,
        # PII-redacted (governed write).  No-op when memory is off.
        if memory is not None and resolved_session is not None:
            await self._record_turn(memory, resolved_session.id, prompt, result.output)

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

        Governance on streams (M6 parity)
        ---------------------------------
        The seam ``before`` chain still runs, so the **policy/model allowlist and
        PII redaction ENFORCE** on streamed turns exactly as on ``run()``. What a
        stream cannot do is run the ``after`` chain (budget accumulation /
        metering / per-action audit) — a streamed turn carries no usage-bearing
        finalizer (the streaming ``after`` hook was deferred in Track 1). Rather
        than silently drop budget enforcement, a ``budget``-capped agent FAILS
        LOUD here: use :meth:`run` / :meth:`run_durable` for budget-enforced runs,
        or drop the budget to stream. Audit/metering are likewise not accumulated
        for the streamed view (documented, never claimed).
        """
        if self.governance.budget is not None:
            raise RuntimeError(
                f"Agent {self.name!r}: stream() cannot enforce a budget cap — streamed "
                "turns have no usage-bearing finalizer, so token/cost is never "
                "accumulated and the budget would silently not apply. Use agent.run() "
                "or agent.run_durable() for budget-enforced runs, or remove the budget "
                "to stream. (Policy/allowlist and PII redaction DO enforce on streams.)"
            )
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
        audit: list[Any] | None = None,
    ) -> None:
        self.output = output
        self.tool_calls = tool_calls
        self.ir = ir
        self.duration_us = duration_us
        self.receipt = receipt
        # ``audit`` is the per-action signed, hash-chained audit record for the
        # run (a list of jamjet.agents.audit.AuditAction), on by default; None
        # when ``audit=False``. ``jamjet.agents.audit.verify_chain`` accepts it.
        self.audit = audit

    def __str__(self) -> str:
        return self.output

    def __repr__(self) -> str:
        return f"AgentResult(output={self.output!r}, tool_calls={len(self.tool_calls)})"
