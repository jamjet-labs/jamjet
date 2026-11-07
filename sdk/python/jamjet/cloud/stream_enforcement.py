"""Policy and budget enforcement for ``stream=True`` calls.

A streaming ``create()`` returns an iterator of chunks (OpenAI) or events
(Anthropic) before the model has produced anything, so the checks a
non-streaming call runs on the finished response run on the stream instead.
The enforced iterator:

- passes text through as it arrives;
- holds every chunk from the first tool call onward, so a tool call the policy
  refuses never reaches the caller (OpenAI can rename a call in a later delta,
  and approval needs the full arguments);
- at the end of the stream hands the finished tool calls to ``on_tool_calls``
  (which raises to refuse them) and only then releases the held chunks;
- reports usage once through ``on_end``, whether the stream ends, fails, or
  the caller stops reading early.

Mirrors ``sdk/typescript/packages/cloud/src/stream-enforcement.ts``.
"""

from __future__ import annotations

from collections.abc import Callable, Generator, Iterator
from dataclasses import dataclass, field
from typing import Any

from .policy import _field

Vendor = str  # "openai" | "anthropic"


@dataclass
class StreamedToolCall:
    # The name as the openai SDK accumulates it: a non-empty fragment replaces.
    name: str = ""
    id: str | None = None
    arguments: str = ""
    # Every name fragment joined, for a consumer that concatenates them.
    joined_name: str = ""


@dataclass
class StreamTally:
    model: str | None = None
    input_tokens: int | None = None
    output_tokens: int | None = None
    cache_read_tokens: int = 0
    cache_write_tokens: int = 0
    # Characters of generated text and tool arguments; used when the stream
    # reports no usage.
    output_chars: int = 0
    tool_calls: list[StreamedToolCall] = field(default_factory=list)
    # The vendor's end-of-message signal arrived (finish_reason / message_stop).
    finished: bool = False
    _by_key: dict[str, StreamedToolCall] = field(default_factory=dict, repr=False)

    def _call(self, key: str) -> StreamedToolCall:
        entry = self._by_key.get(key)
        if entry is None:
            entry = StreamedToolCall()
            self._by_key[key] = entry
            self.tool_calls.append(entry)
        return entry

    def observe(self, vendor: Vendor, chunk: Any) -> tuple[bool, StreamedToolCall | None]:
        """Fold one chunk in. Returns (carries tool-call content, Anthropic call it starts)."""
        if vendor == "openai":
            return self._observe_openai(chunk), None
        return self._observe_anthropic(chunk)

    def _observe_openai(self, chunk: Any) -> bool:
        model = _field(chunk, "model")
        if isinstance(model, str):
            self.model = model
        usage = _field(chunk, "usage")
        if usage is not None:
            self.input_tokens = _int(_field(usage, "prompt_tokens"), self.input_tokens)
            self.output_tokens = _int(_field(usage, "completion_tokens"), self.output_tokens)
        tool_chunk = False
        for choice in _field(chunk, "choices") or []:
            if _field(choice, "finish_reason") is not None:
                self.finished = True
            text = _field(choice, "text")  # legacy completions
            if isinstance(text, str):
                self.output_chars += len(text)
            delta = _field(choice, "delta")
            content = _field(delta, "content")
            if isinstance(content, str):
                self.output_chars += len(content)
            calls: list[Any] = list(_field(delta, "tool_calls") or [])
            # Legacy `functions` API streams one delta.function_call per choice.
            legacy = _field(delta, "function_call")
            if legacy is not None:
                calls.append({"index": "function_call", "function": legacy})
            for tc in calls:
                tool_chunk = True
                entry = self._call(f"{_field(choice, 'index') or 0}:{_field(tc, 'index') or 0}")
                call_id = _field(tc, "id")
                if isinstance(call_id, str):
                    entry.id = call_id
                fn = _field(tc, "function") or _field(tc, "custom")
                # Mirrors the openai SDK's own accumulator: a non-empty name
                # replaces, argument text appends.
                name = _field(fn, "name")
                if isinstance(name, str) and name:
                    entry.name = name
                    entry.joined_name += name
                for key in ("arguments", "input"):
                    text = _field(fn, key)
                    if isinstance(text, str):
                        entry.arguments += text
                        self.output_chars += len(text)
        return tool_chunk

    def _observe_anthropic(self, event: Any) -> tuple[bool, StreamedToolCall | None]:
        kind = _field(event, "type")
        if kind == "message_start":
            message = _field(event, "message")
            model = _field(message, "model")
            if isinstance(model, str):
                self.model = model
            usage = _field(message, "usage")
            self.input_tokens = _int(_field(usage, "input_tokens"), self.input_tokens)
            self.output_tokens = _int(_field(usage, "output_tokens"), self.output_tokens)
            self.cache_read_tokens = _int(_field(usage, "cache_read_input_tokens"), self.cache_read_tokens) or 0
            self.cache_write_tokens = _int(_field(usage, "cache_creation_input_tokens"), self.cache_write_tokens) or 0
        elif kind == "message_delta":
            usage = _field(event, "usage")
            self.output_tokens = _int(_field(usage, "output_tokens"), self.output_tokens)
            self.input_tokens = _int(_field(usage, "input_tokens"), self.input_tokens)
            self.cache_read_tokens = _int(_field(usage, "cache_read_input_tokens"), self.cache_read_tokens) or 0
            self.cache_write_tokens = _int(_field(usage, "cache_creation_input_tokens"), self.cache_write_tokens) or 0
        elif kind == "message_stop":
            self.finished = True
        elif kind == "content_block_start":
            block = _field(event, "content_block")
            if _field(block, "type") in ("tool_use", "server_tool_use", "mcp_tool_use"):
                entry = self._call(str(_field(event, "index") or 0))
                name = _field(block, "name")
                entry.name = name if isinstance(name, str) else ""
                entry.joined_name = entry.name
                block_id = _field(block, "id")
                if isinstance(block_id, str):
                    entry.id = block_id
                return True, entry
        elif kind == "content_block_delta":
            delta = _field(event, "delta")
            text = _field(delta, "text")
            if isinstance(text, str):
                self.output_chars += len(text)
            partial = _field(delta, "partial_json")
            if isinstance(partial, str):
                self.output_chars += len(partial)
                pending = self._by_key.get(str(_field(event, "index") or 0))
                if pending is not None:
                    pending.arguments += partial
                    return True, None
        return False, None


def _int(value: Any, default: int | None) -> int | None:
    if isinstance(value, bool):
        return default
    if isinstance(value, int):
        return value
    return default


OnToolCalls = Callable[[list[StreamedToolCall], StreamTally], None]
OnStarted = Callable[[StreamedToolCall, StreamTally], None]
# on_end(tally, "complete" | "incomplete" | "error", error)
OnEnd = Callable[[StreamTally, str, BaseException | None], None]


def enforced_iterator(
    inner: Iterator[Any],
    *,
    vendor: Vendor,
    hold_tool_calls: Callable[[], bool],
    on_tool_calls: OnToolCalls,
    on_tool_call_started: OnStarted,
    on_end: OnEnd,
) -> Generator[Any, None, None]:
    tally = StreamTally()
    held: list[Any] = []
    holding = False
    upstream_done = False
    ended = False
    try:
        for chunk in inner:
            tool_chunk, started = tally.observe(vendor, chunk)
            if started is not None:
                on_tool_call_started(started, tally)
            # Asked per chunk, so a rule added while the stream is open still holds.
            if tool_chunk and not holding and hold_tool_calls():
                holding = True
            if holding:
                held.append(chunk)
            else:
                yield chunk
        upstream_done = True
        on_tool_calls([c for c in tally.tool_calls if c.name or c.arguments], tally)
        ended = True
        on_end(tally, "complete", None)
        yield from held
    except GeneratorExit:
        raise
    except BaseException as exc:
        if not ended:
            ended = True
            on_end(tally, "error", exc)
        raise
    finally:
        if not upstream_done:
            # The caller stopped reading, or a check refused the stream: close
            # the upstream iterator so the HTTP response is released.
            close = getattr(inner, "close", None)
            if callable(close):
                try:
                    close()
                except Exception:  # noqa: BLE001  closing is best effort
                    pass
        if not ended:
            on_end(tally, "incomplete", None)


class _EnforcedStream:
    """Fallback wrapper for an iterable result that is not an SDK Stream."""

    def __init__(self, wrapped: Any, iterator: Generator[Any, None, None]) -> None:
        self._wrapped = wrapped
        self._iterator = iterator

    def __iter__(self) -> Iterator[Any]:
        return self._iterator

    def __next__(self) -> Any:
        return next(self._iterator)

    def __enter__(self) -> _EnforcedStream:
        return self

    def close(self) -> None:
        close_iterator = getattr(self._iterator, "close", None)
        if callable(close_iterator):
            close_iterator()
        close = getattr(self._wrapped, "close", None)
        if callable(close):
            close()

    def __exit__(self, *exc: object) -> None:
        self.close()

    def __getattr__(self, name: str) -> Any:
        return getattr(self._wrapped, name)


def enforce_stream(result: Any, **hooks: Any) -> Any | None:
    """Route every read of ``result`` through the enforced iterator.

    SDK streams (``openai.Stream``, ``anthropic.Stream``) read through their
    ``_iterator`` attribute, so it is replaced in place and the object keeps
    its class and its context-manager behaviour. ``close()`` (also called on
    context-manager exit) closes the enforced iterator first, so a stream the
    caller abandons is still recorded. Returns None when ``result`` is not
    iterable.
    """
    inner = getattr(result, "_iterator", None)
    if inner is not None and hasattr(inner, "__next__"):
        enforced = enforced_iterator(inner, **hooks)
        result._iterator = enforced
        original_close = getattr(result, "close", None)
        if callable(original_close):

            def close() -> None:
                enforced.close()
                original_close()

            result.close = close
        return result
    try:
        iterator = iter(result)
    except TypeError:
        return None
    return _EnforcedStream(result, enforced_iterator(iterator, **hooks))
