"""Run one claimed work item: the step executor ``jamjet worker`` uses.

:func:`run_claimed_item` takes a claim answer and carries the item to its end:
it resolves the handler the payload names, runs it with the item's input,
keeps the lease alive while it runs, and reports the result under the lease
fence. What happened comes back as a :class:`StepResult`.

The rules it keeps:

- Heartbeats go every third of the lease the engine granted (the claim
  answer's ``lease_secs``), from a task on the event loop. A synchronous
  handler runs in a worker thread so it cannot starve them.
- A heartbeat answer with a new ``lease_fence`` means the engine re-attached a
  lease that lapsed while it was unreachable. Every later call carries it.
- The completion carries the fence and the claim's idempotency key.
- A 409 on complete or fail means the lease was reclaimed and another worker
  owns the item. Nothing more is sent for it: reporting a failure then would
  fail work that is running under a newer fence.
- A heartbeat answer with ``cancel_requested`` sets the handler's
  :class:`~jamjet.worker_context.WorkContext` flag, stops waiting for the
  handler and reports nothing.
"""

from __future__ import annotations

import asyncio
import enum
import importlib
import math
import time
from typing import TYPE_CHECKING, Any

import httpx
from rich.console import Console

from jamjet.cli.audit_trail import _clean as _safe
from jamjet.worker_context import WorkContext, reset_current_work, set_current_work

if TYPE_CHECKING:
    from jamjet.client import JamjetClient
    from jamjet.model import Model

console = Console()


class StepResult(enum.Enum):
    """How a claimed item ended."""

    COMPLETED = "completed"
    """The result was recorded."""
    FAILED_REPORTED = "failed_reported"
    """The handler, or the completion, failed and the failure was recorded."""
    LEASE_LOST = "lease_lost"
    """The engine refused the result or the failure with 409: another worker owns the item."""
    CANCELLED = "cancelled"
    """The run was cancelled while the handler ran; nothing was reported."""


def _print_error(label: str, message: object) -> None:
    """Print ``label`` in red, then ``message`` literally: escaped and never read
    as Rich markup, since it can quote a handler or the runtime."""
    if label:
        console.print(label, style="red", end=" ")
    console.print(_safe(message), markup=False, highlight=False, soft_wrap=True)


def failure_report(exc: BaseException) -> str:
    """The error text a worker sends with ``fail_work_item``.

    When the runtime refused the result, its error body can quote the output
    back; sending that to ``/fail`` would store server-echoed values in the
    event log, so only the status goes. Any other HTTP error came from the
    handler's own call: report its status, host and path (not the query
    string, which can carry tokens) without blaming the runtime.
    """
    from jamjet.client import JamjetAPIError

    if isinstance(exc, httpx.HTTPStatusError):
        request = exc.request
        status = exc.response.status_code
        if isinstance(exc, JamjetAPIError):
            return f"runtime refused the result: HTTP {status} for {request.method} {request.url.path}"
        return f"HTTP {status} from {request.method} {request.url.scheme}://{request.url.host}{request.url.path}"
    return str(exc)


def reported_usage(output: dict[str, Any]) -> dict[str, Any]:
    """Token usage and cost a node reports in its output, for the engine to
    charge to the agent the node acts for. Only values that are a spend are
    forwarded: non-negative integers below 2**64 for tokens (the engine reads
    a u64 and refuses the whole result otherwise), a non-negative finite number
    for cost (a negative cost would refund an agent's budget, and the engine
    refuses one). Booleans are not numbers here."""
    usage: dict[str, Any] = {}
    for key in ("input_tokens", "output_tokens"):
        v = output.get(key)
        if isinstance(v, int) and not isinstance(v, bool) and 0 <= v < 2**64:
            usage[key] = v
    cost = output.get("cost_usd")
    if isinstance(cost, (int, float)) and not isinstance(cost, bool) and math.isfinite(cost) and cost >= 0:
        usage["cost_usd"] = float(cost)
    return usage


def heartbeat_interval(work_item: dict[str, Any], lease_secs: int | None = None) -> float:
    """A third of the lease the engine granted, at least a second.

    Paced from the lease the engine GRANTED, which it echoes in the claim
    answer and which may differ from the one asked for (it holds requests to
    [5, 3600] s). Pacing from the request let a worker that asked for 20000 s
    heartbeat every 6667 s against a 3600 s lease. ``lease_secs``, the lease
    asked for, is used only when the answer does not say (else 30 s, the
    engine default).
    """
    granted = work_item.get("lease_secs")
    if not isinstance(granted, (int, float)) or isinstance(granted, bool) or granted <= 0:
        granted = lease_secs or 30
    return max(1.0, granted / 3)


def _status_code(exc: BaseException) -> int | None:
    # httpx surfaces a refusal as HTTPStatusError carrying .response.status_code.
    return getattr(getattr(exc, "response", None), "status_code", None)


async def run_claimed_item(
    client: JamjetClient,
    worker_id: str,
    work_item: dict[str, Any],
    *,
    model: Model | None = None,
    run_key: str | None = None,
    heartbeat_interval_s: float | None = None,
    lease_secs: int | None = None,
) -> StepResult:
    """Run one claimed work item and report its result.

    ``work_item`` is the claim answer's ``work_item``. Its payload names the
    handler (``module`` and ``function``) and its ``input``. ``run_key`` is
    the caller's key for the run, shown to the handler on its
    :class:`~jamjet.worker_context.WorkContext` with the claim's idempotency
    key. ``model`` is the caller's model for worker model steps; a tool item
    does not use it. ``heartbeat_interval_s`` overrides the pace set by
    :func:`heartbeat_interval`; ``lease_secs`` is the lease the claim asked
    for, used for the pace only when the answer does not carry the granted one.

    A 409 on complete or fail returns :attr:`StepResult.LEASE_LOST` and sends
    nothing more. A failure the engine would not take for another reason
    raises, as does a task cancel that did not come from the engine.
    """
    item_id: str = work_item["id"]
    exec_id: str = work_item["execution_id"]
    node_id: str = work_item["node_id"]
    payload: dict[str, Any] = work_item.get("payload", {})
    # The fence can change once: a heartbeat that re-attaches an
    # expired lease returns a newer one, and every later call must
    # carry it. A one-slot holder keeps the closures below in step.
    lease = {"fence": int(work_item.get("lease_fence", 0))}
    # Echoed on complete so the engine records the result against it.
    idempotency_key: str | None = work_item.get("idempotency_key")

    console.print(f"[cyan]Claimed[/cyan] id={item_id} node=[bold]{node_id}[/bold] exec={exec_id}")
    interval = heartbeat_interval_s if heartbeat_interval_s is not None else heartbeat_interval(work_item, lease_secs)

    ctx = WorkContext(
        item_id=item_id,
        execution_id=exec_id,
        node_id=node_id,
        idempotency_key=idempotency_key,
        run_key=run_key,
    )
    ctx_token = set_current_work(ctx)
    handler_task: asyncio.Task[Any] | None = None

    # Keep the lease alive while the handler runs, and stop the handler
    # when the engine says the execution was cancelled.
    async def _heartbeat_loop() -> None:
        while True:
            await asyncio.sleep(interval)
            try:
                answer = await client.heartbeat_work_item(item_id, worker_id, lease["fence"])
            except Exception as hb_err:
                console.print(f"[yellow]Heartbeat error:[/yellow] {hb_err}")
                continue
            if not isinstance(answer, dict):
                continue
            new_fence = answer.get("lease_fence")
            if isinstance(new_fence, int) and new_fence != lease["fence"]:
                console.print(f"[yellow]Lease re-attached[/yellow] id={item_id} fence {lease['fence']} -> {new_fence}")
                lease["fence"] = new_fence
            if answer.get("cancel_requested"):
                ctx.cancel_requested.set()
                if handler_task is not None:
                    handler_task.cancel()
                return

    hb_task = asyncio.create_task(_heartbeat_loop())

    t_start = time.monotonic()
    try:
        mod_path = payload.get("module")
        fn_path = payload.get("function")
        if not mod_path or not fn_path:
            raise ValueError(f"work item payload missing 'module' or 'function': {sorted(payload)}")

        mod = importlib.import_module(mod_path)
        fn: Any = mod
        for part in fn_path.split("."):
            fn = getattr(fn, part)

        tool_input = payload.get("input", {})

        # Inject deterministic seeds so handlers can opt into replay fidelity.
        from jamjet.runtime.local.seed import _inject_seeds

        _inject_seeds(exec_id)

        async def _run_handler() -> Any:
            # A coroutine function runs on the loop; anything else runs in
            # a thread so the heartbeat task keeps the lease alive. The
            # thread inherits this context, so `current_work()` works there.
            if asyncio.iscoroutinefunction(fn):
                return await fn(tool_input)
            outcome = await asyncio.to_thread(fn, tool_input)
            if asyncio.iscoroutine(outcome) or asyncio.isfuture(outcome):
                outcome = await outcome
            return outcome

        handler_task = asyncio.create_task(_run_handler())
        try:
            result = await handler_task
        except asyncio.CancelledError:
            if not ctx.cancel_requested.is_set():
                raise
            console.print(f"[yellow]Abandoned[/yellow] id={item_id} node=[bold]{node_id}[/bold]: execution cancelled")
            return StepResult.CANCELLED

        duration_ms = int((time.monotonic() - t_start) * 1000)
        output: Any = result if isinstance(result, dict) else {"result": result}

        # The node mutates workflow state with its return value: a
        # PythonFn node's return dict IS its `state_patch` (merge-patched
        # into state, top-level keys replaced). This is what carries the
        # agent loop's `dispatch_tool_calls` return, {"messages": [...]},
        # into state["messages"] for the next turn's model node.
        # A non-dict return is coerced to {"result": <value>} above, so a
        # plain tool additively records {"result": ...}; nodes that mean
        # to leave state untouched simply return {}.
        #
        # This is a deliberate semantic (return-as-state_patch), NOT a bug.
        # Caveat (F-2j-statepatch-namespace): because top-level keys are
        # replaced, a plain @tool that returns a dict must avoid the loop's
        # reserved keys (e.g. "messages", "last_model_output") or it will
        # clobber loop state. There is no key namespacing yet; that is a
        # tracked follow-up, not handled here.
        state_patch: dict[str, Any] = output

        # Forward optional GenAI telemetry if the tool surfaced it.
        gen_ai_model_field: str | None = None
        finish_reason_field: str | None = None
        usage: dict[str, Any] = {}
        if isinstance(output, dict):
            gen_ai_model_field = output.get("gen_ai_model") or None
            finish_reason_field = output.get("finish_reason") or None
            usage = reported_usage(output)

        try:
            await client.complete_work_item(
                item_id,
                exec_id,
                node_id,
                output,
                state_patch,
                duration_ms,
                gen_ai_model=gen_ai_model_field,
                finish_reason=finish_reason_field,
                # Echo the lease fence from the claim (or the re-attached one)
                # so the runtime can fence the completion. Omitted when
                # 0/absent, keeping the unfenced fallback backward-compatible.
                lease_fence=lease["fence"],
                idempotency_key=idempotency_key,
                **usage,
            )
        except Exception as complete_exc:
            # A 409 means our echoed fence no longer matches: the lease was
            # reclaimed and a NEW worker owns this item now. This (stale)
            # worker must NOT fail_work_item: that would clobber the
            # reclaimed work the new claimant is running and defeat the fence.
            # Treat the lost lease as a no-op and move on.
            if _status_code(complete_exc) == 409:
                console.print(f"[yellow]Completion rejected; lease lost[/yellow] id={item_id}")
                return StepResult.LEASE_LOST
            # Any other completion error keeps the existing fail behavior.
            raise
        console.print(f"[green]Completed[/green] id={item_id} node=[bold]{node_id}[/bold] {duration_ms}ms")
        return StepResult.COMPLETED
    except Exception as exc:
        _print_error("Failed", f"node={node_id}: {exc}")
        try:
            # Echo the fence here for the same reason completion does: it
            # proves the lease is still ours. It also buys the retry and
            # dead-letter semantics: without it the runtime settles the
            # item, emits no NodeFailed, and the execution never finishes.
            await client.fail_work_item(item_id, failure_report(exc), lease_fence=lease["fence"])
        except Exception as fail_exc:
            # 409: the lease was reclaimed while this node was failing, so
            # a new claimant owns the item. Reporting our failure would
            # kill work that is currently running under a newer fence.
            if _status_code(fail_exc) == 409:
                console.print(f"[yellow]Failure report rejected; lease lost[/yellow] id={item_id}")
                return StepResult.LEASE_LOST
            raise
        return StepResult.FAILED_REPORTED
    finally:
        hb_task.cancel()
        reset_current_work(ctx_token)
