"""A scripted model: run an agent with no API key.

Use ``model="scripted/<path to YAML>"``. Each model call in a run is answered
from the script. It needs no key and no ``[model]`` extra, and it goes through
the same seam as every other model, so policy, budget and metering still apply.

Script format::

    # A scripted model for running an agent with no API key.
    # Turn N answers the agent's Nth model call in a run.
    turns:
      - content: "Let me look for venues."      # optional text
        tool_calls:
          - name: search_venues
            arguments: {city: Lisbon, guests: 40}
      - tool_calls:
          - name: check_dates
            arguments: {venue: Casa do Rio}
      - content: "Casa do Rio is free on 4 December."
        usage: {input_tokens: 900, output_tokens: 40}   # optional; default is an estimate

The path is relative to the current working directory and is read on every
call. The turn is the number of assistant messages already in the thread, so a
script answers a fresh conversation: in a continued session the count includes
the earlier assistant messages and the script can run out of turns. The path
must end in ``.yaml`` or ``.yml``.
Tool calls get the ids ``call_{turn}_{index}``, both 0-based. If a result for a
scripted call is missing from the thread, the next call fails loudly.
"""

from __future__ import annotations

import json
from collections.abc import AsyncIterator
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import yaml

from jamjet.model.types import ModelRequest, ModelResponse, StreamChunk


class ScriptedModelError(RuntimeError):
    """The script cannot answer this model call."""


@dataclass
class _Function:
    name: str
    arguments: str


@dataclass
class _ToolCall:
    id: str
    function: _Function
    type: str = "function"


@dataclass
class _Message:
    """Same attributes the consumers read from a LiteLLM message."""

    content: str
    tool_calls: list[_ToolCall] | None
    finish_reason: str
    role: str = "assistant"


def load_script(path: str) -> list[dict]:
    """Read and validate a script file; return its turns.

    Errors name the path as the model string gave it and never repeat what the
    file contains.
    """
    file = Path(path).expanduser()
    if file.suffix.lower() not in (".yaml", ".yml"):
        raise ScriptedModelError(f"scripted model: the script path must end in .yaml or .yml (got {path})")
    absolute = file if file.is_absolute() else Path.cwd() / file
    try:
        text = absolute.read_text(encoding="utf-8")
    except OSError as exc:
        raise ScriptedModelError(
            f"scripted model: cannot read the script at {path} ({exc.strerror or 'error'})"
        ) from exc
    except UnicodeDecodeError as exc:
        raise ScriptedModelError(f"scripted model: {path} is not a valid script (not UTF-8 text)") from exc
    try:
        data = yaml.safe_load(text)
    except yaml.YAMLError as exc:
        raise ScriptedModelError(f"scripted model: {path} is not a valid script (not valid YAML)") from exc
    turns = data.get("turns") if isinstance(data, dict) else None
    if not isinstance(turns, list):
        raise ScriptedModelError(f"scripted model: {path} needs a top-level 'turns' list")
    for n, turn in enumerate(turns):
        where = f"scripted model: {path}, turn {n}"
        if not isinstance(turn, dict):
            raise ScriptedModelError(f"{where}: a turn must be a mapping")
        calls = turn.get("tool_calls") or []
        if not isinstance(calls, list):
            raise ScriptedModelError(f"{where}: tool_calls must be a list")
        for i, call in enumerate(calls):
            if not isinstance(call, dict) or not isinstance(call.get("name"), str):
                raise ScriptedModelError(f"{where}, tool call {i}: needs a string 'name'")
            if not isinstance(call.get("arguments", {}), dict):
                raise ScriptedModelError(f"{where}, tool call {i}: 'arguments' must be a mapping")
    return turns


def _get(obj: Any, key: str) -> Any:
    return obj.get(key) if isinstance(obj, dict) else getattr(obj, key, None)


def _call_name(call: Any) -> str:
    fn = _get(call, "function")
    return str((_get(fn, "name") if fn is not None else _get(call, "name")) or "")


def _offered_names(tools: list[dict[str, Any]]) -> list[str]:
    return [str(_get(_get(t, "function"), "name") or _get(t, "name") or "") for t in tools]


def _build(request: ModelRequest) -> ModelResponse:
    turns = load_script(request.ref.model)
    path = request.ref.model
    messages = request.messages

    n = sum(1 for m in messages if _get(m, "role") == "assistant")
    if n >= len(turns):
        raise ScriptedModelError(f"the script has {len(turns)} turns; the agent asked for turn {n + 1}")

    if n > 0:
        answered = {_get(m, "tool_call_id") for m in messages if _get(m, "role") == "tool"}
        prev = turns[n - 1].get("tool_calls") or []
        for i, call in enumerate(prev):
            call_id = f"call_{n - 1}_{i}"
            if call_id not in answered:
                raise ScriptedModelError(
                    f"scripted model, turn {n}: no result for {call_id} ({call['name']}). The agent lost a tool result."
                )

    turn = turns[n]
    calls = turn.get("tool_calls") or []
    if calls:
        if not request.tools:
            raise ScriptedModelError(
                f"scripted model, turn {n} in {path}: the script calls {calls[0]['name']}, "
                "but the agent offered no tools"
            )
        offered = _offered_names(request.tools)
        for call in calls:
            if call["name"] not in offered:
                raise ScriptedModelError(
                    f"scripted model, turn {n} in {path}: the script calls {call['name']}, "
                    f"but the agent offered only: {', '.join(offered)}"
                )

    tool_calls = [
        _ToolCall(
            id=f"call_{n}_{i}",
            function=_Function(name=c["name"], arguments=json.dumps(c.get("arguments") or {})),
        )
        for i, c in enumerate(calls)
    ]
    content = str(turn.get("content") or "")
    message = _Message(
        content=content,
        tool_calls=tool_calls or None,
        finish_reason="tool_calls" if tool_calls else "stop",
    )

    usage = turn.get("usage") or {}
    input_tokens = usage.get("input_tokens")
    if input_tokens is None:
        input_tokens = sum(len(str(_get(m, "content") or "")) for m in messages) // 4
    output_tokens = usage.get("output_tokens")
    if output_tokens is None:
        output_tokens = len(content) // 4 + 10 * len(tool_calls)
    return ModelResponse(
        message=message,
        input_tokens=int(input_tokens),
        output_tokens=int(output_tokens),
        cost_usd=0.0,
        raw=turn,
    )


class ScriptedBackend:
    """Seam backend that answers from a YAML script. Never imports LiteLLM."""

    async def complete(self, request: ModelRequest) -> ModelResponse:
        return _build(request)

    async def stream(self, request: ModelRequest) -> AsyncIterator[StreamChunk]:
        response = _build(request)
        yield StreamChunk(delta=response.message.content, raw=response.raw)
