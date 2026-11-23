"""Drive mode: work a run's steps in the process that started it.

:func:`drive_run` claims the run's items by execution, on its tool and worker
model queues, and runs each one with the step executor ``jamjet worker`` uses
(:func:`jamjet.worker_step.run_claimed_item`): a tool step through the
``@tool`` functions, a model step through the caller's model. It goes on until
the run ends or one of the exit rules below applies.

What each claim answer leads to:

- An item: it is run. A lost lease is not an error; the loop claims again.
- ``approval_required``: :class:`RunWaiting` with ``waiting_for="approval"``.
- ``blocked`` or ``refused``: :class:`RunBlocked` with the reason.
- ``claimed_by_other``: claim again each second. Another driver holds the
  step, or a dead one whose lease will lapse. After ``stall_after_s``,
  :class:`RunWaiting` with ``waiting_for="worker"``.
- ``not_ready``, or a reason this build does not know: read the run. An
  ended run returns or raises by its status; else claim again. After
  ``stall_after_s`` with no progress, :class:`RunStalled`.
- ``not_found``: the run ended (it returns or raises by its status), or this
  caller may not drive it (:class:`RunBlocked`).
- No reason (a server that is stopping, or an older one): pause a second and
  claim again.
- An engine 401 or 403: raised at once. Nothing more is sent.

An ended run: ``cancelled`` raises :class:`RunCancelled`; ``completed``,
``failed`` and ``limit_exceeded`` return the run as the engine shows it.

The worker id is ``drive-<12 hex>``, made once per process. A driver started
again after a crash has a new id, so it waits for the old lease to lapse
before it gets the step: two processes never share a lease. Within a process
a run is driven once at a time: the engine hands a worker id the lease it
already holds, so a second drive here would run the first one's step too. It
raises :class:`RunWaiting` instead.

On the ``stop`` event nothing more is claimed. The step in hand gets
:data:`STOP_GRACE_S` to finish; then :class:`RunInterrupted` is raised, and a
step still running is abandoned with its lease left to lapse.
"""

# ruff: noqa: N818 - the exception names are the ones callers match on

from __future__ import annotations

import asyncio
import contextlib
import hashlib
import json
import os
import secrets
import threading
import time
from collections.abc import Awaitable
from typing import TYPE_CHECKING, Any, TypeVar

from jamjet.drive_model import MODEL_QUEUE
from jamjet.worker_step import StepResult, run_claimed_item

if TYPE_CHECKING:
    from jamjet.client import JamjetClient
    from jamjet.model import Model

TOOL_QUEUE = "python_tool"
"""The queue an agent's tool dispatch steps go to."""

DRIVE_QUEUES = (TOOL_QUEUE, MODEL_QUEUE)
"""The queues a driven agent run's steps go to."""

CLAIM_WAIT_MS = 10_000
"""How long each claim waits for the run's next item (the engine's limit)."""

IDLE_PAUSE_S = 1.0
"""The least time between two claims that brought nothing."""

STOP_GRACE_S = 5.0
"""How long the step in hand may run after the stop signal."""

_TERMINAL = frozenset({"completed", "failed", "cancelled", "limit_exceeded"})

# The loop's clock and pause, kept apart so tests can run them without waiting.
_monotonic = time.monotonic
_sleep = asyncio.sleep

_T = TypeVar("_T")


def _new_worker_id() -> str:
    return f"drive-{secrets.token_hex(6)}"


_worker_id = _new_worker_id()

# The runs this process is driving now. The engine hands the lease a worker id
# holds to that same id again, and every drive here has one id, so a second
# drive of a run already driven here would be handed the first one's step and
# run it beside the first.
_driving: set[str] = set()
_driving_lock = threading.Lock()


def _after_fork_in_child() -> None:
    global _worker_id, _driving_lock
    # A forked child is another process: it must not claim under its parent's
    # id, or the engine would hand it the lease the parent holds. Nor does it
    # inherit the parent's drives, or a lock the parent held at the fork.
    _worker_id = _new_worker_id()
    _driving.clear()
    _driving_lock = threading.Lock()


if hasattr(os, "register_at_fork"):
    os.register_at_fork(after_in_child=_after_fork_in_child)


def driver_worker_id() -> str:
    """The worker id this process drives runs under: ``drive-<12 hex>``."""
    return _worker_id


class RunWaiting(Exception):
    """The run waits for something this process cannot do. Not a failure: a
    job that drives a run can exit 0 on it and be started again later.

    ``waiting_for`` is ``"approval"`` (a reviewer's decision) or ``"worker"``
    (another worker holds the step).
    """

    def __init__(self, execution_id: str, waiting_for: str, node_id: str | None = None, message: str = "") -> None:
        self.execution_id = execution_id
        self.waiting_for = waiting_for
        self.node_id = node_id
        at = f" at node {node_id}" if node_id else ""
        super().__init__(message or f"run {execution_id} is waiting for {waiting_for}{at}")


class _RunError(RuntimeError):
    def __init__(self, execution_id: str, message: str) -> None:
        self.execution_id = execution_id
        super().__init__(message)


class RunBlocked(_RunError):
    """The engine will not hand the run's next step out: ``reason`` is the
    claim's answer (``blocked``, ``refused`` or ``not_found``)."""

    def __init__(self, execution_id: str, reason: str, message: str) -> None:
        self.reason = reason
        super().__init__(execution_id, message)


class RunStalled(_RunError):
    """The run made no progress for ``stall_after_s``."""


class RunCancelled(_RunError):
    """The run was cancelled."""


class RunInterrupted(_RunError):
    """The stop signal ended the drive before the run did."""


async def drive_run(
    client: JamjetClient,
    execution_id: str,
    *,
    queues: list[str],
    model: Model | None,
    run_key: str | None,
    lease_secs: int | None = None,
    stall_after_s: float = 180.0,
    stop: asyncio.Event | None = None,
) -> dict[str, Any]:
    """Work ``execution_id``'s steps on ``queues`` until it ends; return the
    ended run as ``GET /executions/:id`` shows it.

    ``model`` answers the run's worker model steps. ``run_key`` is shown to
    tool handlers on their work context. ``lease_secs`` is the lease each
    claim asks for. See the module docstring for the exit rules.

    A process drives a run once at a time. A second call for a run this
    process is already driving raises :class:`RunWaiting` at once and claims
    nothing.
    """
    with _driving_lock:
        if execution_id in _driving:
            raise RunWaiting(
                execution_id,
                "worker",
                message=f"this process is already driving run {execution_id}; it was not driven a second time",
            )
        _driving.add(execution_id)
    try:
        return await _drive(
            client,
            execution_id,
            queues=queues,
            model=model,
            run_key=run_key,
            lease_secs=lease_secs,
            stall_after_s=stall_after_s,
            stop=stop,
        )
    finally:
        with _driving_lock:
            _driving.discard(execution_id)


async def _drive(
    client: JamjetClient,
    execution_id: str,
    *,
    queues: list[str],
    model: Model | None,
    run_key: str | None,
    lease_secs: int | None,
    stall_after_s: float,
    stop: asyncio.Event | None,
) -> dict[str, Any]:
    worker_id = driver_worker_id()
    last_progress = _monotonic()
    seen: str | None = None
    while True:
        if stop is not None and stop.is_set():
            raise RunInterrupted(execution_id, f"stopped driving run {execution_id}: nothing more is claimed")
        asked_at = _monotonic()
        answer = await _unless_stopped(
            client.claim_for_execution(worker_id, queues, execution_id, lease_secs=lease_secs, wait_ms=CLAIM_WAIT_MS),
            stop,
            execution_id,
        )

        if answer.get("claimed"):
            item = answer.get("work_item")
            if not isinstance(item, dict) or item.get("execution_id") != execution_id:
                other = item.get("execution_id") if isinstance(item, dict) else None
                raise RunBlocked(
                    execution_id,
                    "refused",
                    f"asked for an item of run {execution_id} and the engine handed out one of run {other!r}; "
                    "it was not run, and its lease is left to lapse",
                )
            result = await _run_step(client, worker_id, item, model, run_key, lease_secs, stop, execution_id)
            if result is StepResult.CANCELLED:
                raise RunCancelled(execution_id, f"run {execution_id} was cancelled while its step ran")
            last_progress = _monotonic()
            continue

        reason = answer.get("reason")
        if reason == "approval_required":
            node_id = answer.get("node_id")
            raise RunWaiting(execution_id, "approval", node_id if isinstance(node_id, str) else None)
        if reason in ("blocked", "refused"):
            raise RunBlocked(
                execution_id,
                reason,
                f"the engine will not run the next step of run {execution_id} ({reason})",
            )
        if reason == "not_found":
            execution = await client.get_execution(execution_id)
            if execution.get("status") in _TERMINAL:
                return _ended(execution_id, execution)
            raise RunBlocked(
                execution_id,
                "not_found",
                f"the engine does not hand the steps of run {execution_id} to this caller (not_found): "
                "only the token that started the run may drive it",
            )

        now = _monotonic()
        if reason == "claimed_by_other":
            if now - last_progress >= stall_after_s:
                raise RunWaiting(
                    execution_id,
                    "worker",
                    message=f"another worker has held the step of run {execution_id} for {stall_after_s:g} s",
                )
        elif reason is None or reason == "draining":
            # A server that is stopping, or one that does not say why.
            await _pause(IDLE_PAUSE_S, stop)
            continue
        else:
            # not_ready, or a reason this build does not know: the engine has
            # the run's next move. Read the run to see whether it moves.
            execution = await client.get_execution(execution_id)
            if execution.get("status") in _TERMINAL:
                return _ended(execution_id, execution)
            mark = _progress_mark(execution)
            if seen is not None and mark != seen:
                last_progress = now
            seen = mark
            if now - last_progress >= stall_after_s:
                raise RunStalled(execution_id, await _stall_message(client, execution_id, stall_after_s, reason))
        # A claim answered at once is not sent again straight away.
        await _pause(max(0.0, IDLE_PAUSE_S - (_monotonic() - asked_at)), stop)


def _ended(execution_id: str, execution: dict[str, Any]) -> dict[str, Any]:
    if execution.get("status") == "cancelled":
        raise RunCancelled(execution_id, f"run {execution_id} was cancelled")
    return execution


def _progress_mark(execution: dict[str, Any]) -> str:
    """What changes when the run moves: its status, its state, what it waits for."""
    seen = {k: execution.get(k) for k in ("status", "updated_at", "waiting_for", "current_state")}
    return hashlib.sha256(json.dumps(seen, sort_keys=True, default=str).encode()).hexdigest()


async def _stall_message(client: JamjetClient, execution_id: str, stall_after_s: float, reason: Any) -> str:
    message = f"run {execution_id} made no progress in {stall_after_s:g} s (the engine answered {reason!r})"
    try:
        health = await client.health()
    except Exception:  # noqa: BLE001 - the stall is reported either way
        return message
    if isinstance(health, dict) and health.get("tick_overdue") is True:
        message += (
            ": the engine reports its tick overdue. Its background loops are off and nothing has "
            "called POST /ops/tick lately, so its own steps do not run"
        )
    return message


async def _pause(seconds: float, stop: asyncio.Event | None) -> None:
    if seconds <= 0:
        return
    if stop is None:
        await _sleep(seconds)
        return
    with contextlib.suppress(RunInterrupted):
        # The stop is seen at the top of the loop.
        await _unless_stopped(_sleep(seconds), stop, "")


async def _unless_stopped(work: Awaitable[_T], stop: asyncio.Event | None, execution_id: str) -> _T:
    """``work``'s answer, or :class:`RunInterrupted` when ``stop`` is set first
    (``work`` is then cancelled: a claim it took is left to lapse)."""
    if stop is None:
        return await work
    task = asyncio.ensure_future(work)
    stopped = asyncio.ensure_future(stop.wait())
    try:
        await asyncio.wait({task, stopped}, return_when=asyncio.FIRST_COMPLETED)
    except BaseException:
        task.cancel()
        raise
    finally:
        stopped.cancel()
    if task.done():
        return task.result()
    task.cancel()
    with contextlib.suppress(BaseException):
        await task
    raise RunInterrupted(
        execution_id, f"stopped driving run {execution_id}: nothing more is claimed, and a lease is left to lapse"
    )


async def _run_step(
    client: JamjetClient,
    worker_id: str,
    item: dict[str, Any],
    model: Model | None,
    run_key: str | None,
    lease_secs: int | None,
    stop: asyncio.Event | None,
    execution_id: str,
) -> StepResult:
    step = asyncio.ensure_future(
        run_claimed_item(client, worker_id, item, model=model, run_key=run_key, lease_secs=lease_secs)
    )
    if stop is None:
        return await step
    stopped = asyncio.ensure_future(stop.wait())
    try:
        await asyncio.wait({step, stopped}, return_when=asyncio.FIRST_COMPLETED)
        if not step.done():
            # Stopped mid-step: the step gets its grace, then is abandoned.
            await asyncio.wait({step}, timeout=STOP_GRACE_S)
    except BaseException:
        step.cancel()
        raise
    finally:
        stopped.cancel()
    node = item.get("node_id")
    if not step.done():
        step.cancel()
        with contextlib.suppress(BaseException):
            await step
        raise RunInterrupted(
            execution_id,
            f"stopped driving run {execution_id}: step {node} did not finish within {STOP_GRACE_S:g} s, "
            "so it was abandoned and its lease is left to lapse",
        )
    # Finished, within its grace or before any stop: the loop's next turn
    # sees the stop and claims nothing more.
    return step.result()
