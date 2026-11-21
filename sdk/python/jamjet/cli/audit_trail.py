"""The audit trail of one execution, built from the engine's own records.

Pure functions, no I/O: the execution (``GET /executions/{id}``), its event
log (``GET /executions/{id}/events``) and its decided approvals (the
``decided`` list of ``GET /executions/{id}/approvals``) in, trail rows out.
``jamjet audit`` makes the HTTP calls.

Where each row comes from (event kinds are in ``runtime/state/src/event.rs``):

- ``model asked for <tool>``: a model node's ``NodeCompleted`` output
  ``tool_calls``. A tool-dispatch node's output messages (the assistant
  ``tool_calls``) fill in any call the model output did not show.
- ``<tool> blocked``: ``PolicyViolation``, with its rule.
- ``<tool> needs approval``: ``ToolApprovalRequired``.
- ``approved by <name>`` / ``rejected by <name>``: ``ApprovalReceived``. Its
  ``principal`` is the authenticated identity; the engine leaves it out in dev
  mode, where the name is whatever the caller typed.
- ``ran <tool>``: each ``role: tool`` message a dispatch node's output added,
  by its ``tool_call_id``. Messages the run was seeded with do not count.
- ``budget stopped the run``: ``BudgetExceeded``, ``TokenBudgetExceeded``,
  ``CostBudgetExceeded``.
- ``final answer`` / ``run failed: <reason>`` / ``run cancelled``.

The closing "ran N time(s)" lines count the tool results the engine recorded.
A tool whose worker died before its result was recorded is not counted, though
the tool may still have had an effect (a payment, say). Keep your own record of
effects that matter.
"""

from __future__ import annotations

import json
import re
import uuid
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any

SOURCE_LINE = "Source: the engine's event log and approval records. Times are UTC."

_ANSWER_LIMIT = 80
_RESULT_LIMIT = 60
_STEP_WIDTH_CAP = 34
_BUDGET_EVENTS = ("budget_exceeded", "token_budget_exceeded", "cost_budget_exceeded")
# The engine's block rule: "tool 'cancel_booking' matches blocked pattern 'cancel_*'".
_RULE_TOOL = re.compile(r"tool '([^']+)'")
# Shown escaped: they could move the cursor or reorder the reviewer's lines.
_BIDI_CONTROLS = frozenset("\u061c\u200e\u200f\u202a\u202b\u202c\u202d\u202e\u2066\u2067\u2068\u2069")


@dataclass
class TrailRow:
    """One thing that happened. ``time`` is the event's ``created_at`` (UTC)."""

    time: str
    step: str
    detail: str = ""


@dataclass
class Trail:
    """The rows in log order, how many times each tool ran, and for each tool
    the decided approvals (name, time) that covered a run of it."""

    rows: list[TrailRow] = field(default_factory=list)
    tool_runs: dict[str, int] = field(default_factory=dict)
    approved_runs: dict[str, list[tuple[str, str]]] = field(default_factory=dict)


# ── building ─────────────────────────────────────────────────────────────────


def build_trail(
    execution: dict[str, Any],
    events: list[dict[str, Any]],
    approvals: list[dict[str, Any]],
) -> Trail:
    """The trail for one execution, in event-log order."""
    return _Builder(execution, approvals).run(events)


class _Builder:
    def __init__(self, execution: dict[str, Any], approvals: list[dict[str, Any]]) -> None:
        self.execution = execution
        self.trail = Trail()
        self.proposed: dict[str, str] = {}  # call key -> tool name
        self.ran: set[str] = set()  # call keys whose result is already counted
        self.last_batch: list[str] = []  # tool names of the latest proposal
        self.requests: dict[int, set[str]] = {}  # request sequence -> gated tool names
        self.latest_request: dict[str, set[str]] = {}  # node -> gated tool names
        self.approved: dict[str, list[tuple[set[str], str, str]]] = {}  # node -> (gated, name, time)
        self.last_node_error = ""
        # The approval records name the decider per node; the event is the
        # primary source, the record fills a missing name.
        self.decided_by = {
            str(r.get("node_id")): str(r.get("user_id"))
            for r in approvals
            if isinstance(r, dict) and r.get("status") in ("approved", "rejected") and r.get("user_id")
        }

    def run(self, events: list[dict[str, Any]]) -> Trail:
        ordered = sorted((e for e in events if isinstance(e, dict)), key=_sequence)
        self._mark_seed(ordered)
        for event in ordered:
            self._event(event)
        return self.trail

    def _add(self, time: str, step: str, detail: str = "") -> None:
        self.trail.rows.append(TrailRow(time=time, step=step, detail=detail))

    def _mark_seed(self, events: list[dict[str, Any]]) -> None:
        """Calls and results the run started with (a continued session) were
        made by an earlier run: not proposed or run here."""
        seed: Any = None
        for event in events:
            kind = _kind(event)
            if kind.get("type") == "workflow_started":
                seed = _messages(kind.get("initial_input"))
                break
        if seed is None:
            seed = _messages(self.execution.get("initial_input"))
        if seed is None:
            return
        for index, msg in enumerate(seed):
            if not isinstance(msg, dict):
                continue
            if msg.get("role") == "assistant":
                for j, call in enumerate(msg.get("tool_calls") or []):
                    if isinstance(call, dict):
                        self.proposed[_call_key(call.get("id"), f"#{index}:{j}")] = _call_name(call)
            elif msg.get("role") == "tool":
                self.ran.add(_call_key(msg.get("tool_call_id"), f"#{index}"))

    def _event(self, event: dict[str, Any]) -> None:
        kind = _kind(event)
        kind_type = kind.get("type")
        at = str(event.get("created_at", ""))
        node = str(kind.get("node_id", ""))
        if kind_type == "node_completed":
            output = kind.get("output")
            if isinstance(output, dict):
                if isinstance(output.get("tool_calls"), list):
                    self._model_calls(at, event, output["tool_calls"])
                if isinstance(output.get("messages"), list):
                    self._dispatch(at, node, output["messages"])
        elif kind_type == "policy_violation":
            if kind.get("decision", "blocked") == "blocked":
                rule = str(kind.get("rule", ""))
                found = _RULE_TOOL.search(rule)
                tool = found.group(1) if found else (", ".join(self.last_batch) or node)
                self._add(at, f"{tool} blocked", rule)
        elif kind_type == "tool_approval_required":
            tool = str(kind.get("tool_name") or "?")
            context = _dict(kind.get("context"))
            gated = {str(t) for t in context.get("gated_tools") or [] if t} or {tool}
            self.requests[_sequence(event)] = gated
            self.latest_request[node] = gated
            self._add(at, f"{tool} needs approval", f"approver: {kind.get('approver', '?')}")
        elif kind_type == "approval_received":
            self._decision(at, node, kind)
        elif kind_type == "approval_expired":
            who = str(kind.get("principal") or "system:deadline")
            self._add(at, f"rejected by {who}", f"no decision by {kind.get('expires_at', '?')}")
        elif kind_type in _BUDGET_EVENTS:
            self._add(at, "budget stopped the run", _budget_detail(kind))
        elif kind_type == "node_failed":
            self.last_node_error = str(kind.get("error", ""))
        elif kind_type == "workflow_completed":
            answer = _final_answer(kind.get("final_state"), self.execution)
            if answer is None:
                self._add(at, "run completed")
            else:
                self._add(at, "final answer", _cut(answer, _ANSWER_LIMIT))
        elif kind_type == "workflow_failed":
            reason = str(kind.get("error", ""))
            detail = self.last_node_error if self.last_node_error != reason else ""
            self._add(at, f"run failed: {reason}", detail)
        elif kind_type == "workflow_cancelled":
            self._add(at, "run cancelled", str(kind.get("reason") or ""))

    def _propose(self, at: str, key: str, name: str, call_id: Any, arguments: Any) -> None:
        if key in self.proposed:
            return
        self.proposed[key] = name
        args = _compact(arguments)
        self._add(at, f"model asked for {name}", f"{call_id} {args}" if call_id else args)

    def _model_calls(self, at: str, event: dict[str, Any], calls: list[Any]) -> None:
        batch = []
        for index, call in enumerate(calls):
            if not isinstance(call, dict):
                continue
            name = _call_name(call)
            batch.append(name)
            key = _call_key(call.get("id"), f"@{_sequence(event)}:{index}")
            self._propose(at, key, name, call.get("id"), call.get("arguments", {}))
        if batch:
            self.last_batch = batch

    def _dispatch(self, at: str, node: str, messages: list[Any]) -> None:
        for index, msg in enumerate(messages):
            if not isinstance(msg, dict):
                continue
            if msg.get("role") == "assistant":
                for j, call in enumerate(msg.get("tool_calls") or []):
                    if isinstance(call, dict):
                        fn = _dict(call.get("function"))
                        key = _call_key(call.get("id"), f"#{index}:{j}")
                        args = fn.get("arguments", call.get("arguments", {}))
                        self._propose(at, key, _call_name(call), call.get("id"), args)
            elif msg.get("role") == "tool":
                call_id = msg.get("tool_call_id")
                key = _call_key(call_id, f"#{index}")
                if key in self.ran:
                    continue
                self.ran.add(key)
                name = str(msg.get("name") or self.proposed.get(key) or "?")
                result = _cut(_text(msg.get("content")), _RESULT_LIMIT)
                self._add(at, f"ran {name}", f"{call_id} -> {result}" if call_id else result)
                self.trail.tool_runs[name] = self.trail.tool_runs.get(name, 0) + 1
                self._cover(node, name)

    def _decision(self, at: str, node: str, kind: dict[str, Any]) -> None:
        decision = str(kind.get("decision", "?"))
        name = str(kind.get("user_id") or self.decided_by.get(node) or "unknown")
        principal = kind.get("principal")
        if principal:
            notes = [f"token {str(principal).removeprefix('token:')}"]
        else:
            notes = ["dev mode, name not verified"]
        if kind.get("comment"):
            notes.append(f"comment: {kind['comment']}")
        self._add(at, f"{decision} by {name}", "; ".join(notes))
        if decision == "approved":
            request = kind.get("request_sequence")
            gated = self.requests.get(request) if isinstance(request, int) else None
            if gated is None:
                gated = self.latest_request.get(node, set())
            self.approved.setdefault(node, []).append((gated, name, at))

    def _cover(self, node: str, tool: str) -> None:
        """Tie a run to the latest approval of its node that gated this tool."""
        for gated, name, at in reversed(self.approved.get(node, [])):
            if not gated or tool in gated:
                runs = self.trail.approved_runs.setdefault(tool, [])
                if (name, at) not in runs:
                    runs.append((name, at))
                return


def _dict(value: Any) -> dict[str, Any]:
    return value if isinstance(value, dict) else {}


def _kind(event: dict[str, Any]) -> dict[str, Any]:
    return _dict(event.get("kind"))


def _sequence(event: dict[str, Any]) -> int:
    value = event.get("sequence")
    return value if isinstance(value, int) else 0


def _messages(state: Any) -> list[Any] | None:
    messages = state.get("messages") if isinstance(state, dict) else None
    return messages if isinstance(messages, list) else None


def _call_key(call_id: Any, fallback: str) -> str:
    return str(call_id) if call_id else fallback


def _call_name(call: dict[str, Any]) -> str:
    fn = call.get("function")
    name = fn.get("name") if isinstance(fn, dict) else None
    return str(name or call.get("name") or "?")


def _compact(arguments: Any) -> str:
    """Arguments as compact JSON. A JSON string is parsed first."""
    if isinstance(arguments, str):
        try:
            arguments = json.loads(arguments)
        except ValueError:
            return arguments
    try:
        return json.dumps(arguments, separators=(",", ":"), sort_keys=True, ensure_ascii=False)
    except (TypeError, ValueError):
        return str(arguments)


def _text(value: Any) -> str:
    if value is None:
        return ""
    text = value if isinstance(value, str) else json.dumps(value, ensure_ascii=False)
    return " ".join(text.split())


def _cut(text: str, limit: int) -> str:
    text = " ".join(text.split())
    return text if len(text) <= limit else text[:limit] + "..."


def _final_answer(final_state: Any, execution: dict[str, Any]) -> str | None:
    for state in (final_state, execution.get("current_state")):
        if not isinstance(state, dict):
            continue
        output = state.get("last_model_output")
        if isinstance(output, str) and output:
            return output
        for msg in reversed(_messages(state) or []):
            if isinstance(msg, dict) and msg.get("role") == "assistant" and msg.get("content"):
                return _text(msg["content"])
    return None


def _budget_detail(kind: dict[str, Any]) -> str:
    reason = kind.get("reason")
    if kind.get("type") == "cost_budget_exceeded":
        limit, current = kind.get("limit_usd"), kind.get("current_usd")
        if reason == "cost_not_reported":
            return (
                f"a step did not report a usable cost, so the cap of {limit} USD cannot be checked "
                f"({current} USD counted)"
            )
        if reason == "cost_cap_invalid":
            return f"the cost cap of {limit} USD is not a cap a run can be held to (it must be 0 to 10,000 USD)"
        return f"cost {current} USD, limit {limit} USD"
    name = kind.get("kind", "budget")
    if reason == "tokens_not_reported":
        return f"a step did not report its tokens, so the cap on {name} (limit {kind.get('limit')}) cannot be checked"
    return f"{name} {kind.get('current')}, limit {kind.get('limit')}"


# ── rendering ────────────────────────────────────────────────────────────────


def render_trail(trail: Trail, execution: dict[str, Any]) -> str:
    """The trail as text: a header, one line per row, one line per tool that ran.

    Every value from the run is escaped, so a tool name, an argument or a
    comment cannot drive the terminal.
    """
    eid = _clean(_display_id(execution.get("execution_id")))
    agent = _clean(execution.get("workflow_id") or "-")
    status = _clean(execution.get("status") or "-")
    lines = [
        f"Audit trail for {eid}",
        f"Agent: {agent}   Status: {status}   Started: {_stamp(execution.get('started_at'))}",
        SOURCE_LINE,
        "",
    ]
    steps = [_clean(r.step) for r in trail.rows]
    width = min(max((len(s) for s in steps), default=0), _STEP_WIDTH_CAP)
    for row, step in zip(trail.rows, steps, strict=True):
        lines.append(f"{_clock(row.time)}  {step:<{width}}  {_clean(row.detail)}".rstrip())
    if not trail.rows:
        lines.append("No events recorded.")
    lines.append("")
    if not trail.tool_runs:
        lines.append("No tool ran.")
    for tool, count in trail.tool_runs.items():
        line = f"{_clean(tool)} ran {count} time{'' if count == 1 else 's'}"
        for name, at in trail.approved_runs.get(tool, []):
            line += f", approved by {_clean(name)} at {_clock(at)}"
        lines.append(line)
    return "\n".join(lines)


def _clean(value: Any) -> str:
    """``value`` as text that cannot drive the terminal: every C0 and C1
    control, DEL and bidi control is shown escaped (the rule of ``main._safe``)."""
    out = []
    for ch in str(value):
        code = ord(ch)
        if code < 0x20 or 0x7F <= code <= 0x9F:
            out.append(f"\\x{code:02x}")
        elif ch in _BIDI_CONTROLS:
            out.append(f"\\u{code:04x}")
        else:
            out.append(ch)
    return "".join(out)


def _display_id(value: Any) -> str:
    """The id as the engine prints it (``exec_<hex>``); it serializes the UUID form."""
    text = str(value or "-")
    try:
        return "exec_" + uuid.UUID(text.removeprefix("exec_")).hex
    except ValueError:
        return text


display_id = _display_id


def _parse(value: Any) -> datetime | None:
    try:
        parsed = datetime.fromisoformat(str(value).replace("Z", "+00:00"))
    except ValueError:
        return None
    return parsed.astimezone(UTC) if parsed.tzinfo else parsed


def _clock(value: Any) -> str:
    parsed = _parse(value)
    return parsed.strftime("%H:%M:%S") if parsed else _clean(value)


def _stamp(value: Any) -> str:
    parsed = _parse(value)
    return parsed.strftime("%Y-%m-%d %H:%M:%S") if parsed else _clean(value or "-")
