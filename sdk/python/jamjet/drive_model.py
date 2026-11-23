"""Run a worker model step through the caller's own model.

A ``model`` node marked ``executor: worker`` leaves its model call to the
run's own worker. The claim hands out the request the engine built from the
run's state; :func:`run_model_step` sends it through the caller's
:class:`~jamjet.model.Model` (the seam, with its middleware) and returns the
answer as the engine's ``model_result`` takes it.

The rules it keeps:

- The request is the engine's: the node's system prompt, then the prompt, or
  with no prompt the run's conversation. The worker never sends a prompt of
  its own making, so a payload with none fails the step.
- The backend sees the run: ``request.metadata["jamjet"]`` holds the run,
  node and item ids, the attempt, the claim's idempotency key, the caller's
  run key and what the run's caps leave. A step claimed again after its lease
  ran out carries the same idempotency key, so a router that keeps answers by
  it can answer a retry without paying again.
- Unknown usage is never free. Tokens and cost are sent only when the backend
  says it read them (``ModelResponse.usage_reported``). Without that they are
  left out, and the engine fails the step closed under a cap.
- The model used is the one the backend reports, never the one asked for: the
  engine holds it to the allow lists.
- Anything else (an answer of another shape, a token count that is not a
  whole number from 0, a cost that is not a finite number from 0, an
  exception, the timeout) raises with the reason, and the step executor
  reports it on the fenced fail route.
"""

from __future__ import annotations

import asyncio
import json
import math
from dataclasses import asdict, dataclass
from typing import Any

from jamjet.model.seam import Model
from jamjet.model.types import ModelRequest, ModelResponse, parse_model_ref

MODEL_QUEUE = "model_worker"
"""The built-in queue the engine puts worker model steps on."""


@dataclass(frozen=True)
class StepContext:
    """The run a model step belongs to, shown to the backend as ``metadata["jamjet"]``.

    ``budget`` is what the run's caps leave, counted at claim (the claim's
    ``budget_left`` less the caps the run does not have), or ``None``. It is
    a limit to pass on, not a promise: the engine enforces the caps.
    """

    execution_id: str
    node_id: str
    item_id: str
    attempt: int
    idempotency_key: str | None
    run_key: str | None
    budget: dict[str, float] | None


def step_context(work_item: dict[str, Any], *, run_key: str | None) -> StepContext:
    """The :class:`StepContext` of a claimed model step."""
    attempt = work_item.get("attempt")
    return StepContext(
        execution_id=work_item["execution_id"],
        node_id=work_item["node_id"],
        item_id=work_item["id"],
        attempt=attempt if isinstance(attempt, int) and not isinstance(attempt, bool) else 0,
        idempotency_key=work_item.get("idempotency_key"),
        run_key=run_key,
        budget=_budget(work_item.get("budget_left")),
    )


def _budget(left: Any) -> dict[str, float] | None:
    # A null field is a cap the run does not have.
    if not isinstance(left, dict):
        return None
    caps = {
        k: v for k, v in left.items() if isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v)
    }
    return caps or None


def _get(obj: Any, key: str) -> Any:
    return obj.get(key) if isinstance(obj, dict) else getattr(obj, key, None)


def _has(obj: Any, key: str) -> bool:
    return key in obj if isinstance(obj, dict) else hasattr(obj, key)


def build_request(payload: dict[str, Any], ctx: StepContext) -> ModelRequest:
    """The model request the engine built, read as the in-process executor reads it.

    The node's system prompt comes first. Then the prompt, rendered by the
    engine, as the one user message; with no prompt, the conversation in the
    payload, less a system message that repeats the system prompt.
    """
    if payload.get("prompt_error"):
        raise ValueError(f"the engine could not build the prompt: {payload['prompt_error']}")
    model_id = payload.get("model")
    if not isinstance(model_id, str) or not model_id.strip():
        raise ValueError("the work item names no model")

    system_prompt = payload.get("system_prompt")
    if not isinstance(system_prompt, str) or not system_prompt:
        system_prompt = None
    prompt = payload.get("prompt")
    if isinstance(prompt, str) and prompt:
        conversation: list[dict[str, Any]] = [{"role": "user", "content": prompt}]
    else:
        raw = payload.get("messages") or []
        if not isinstance(raw, list) or not all(isinstance(m, dict) for m in raw):
            raise ValueError("the work item's messages are not a list of messages")
        conversation = [
            m for m in raw if not (system_prompt and m.get("role") == "system" and m.get("content") == system_prompt)
        ]
    if not conversation:
        raise ValueError("the work item has no prompt or messages provided")
    messages = ([{"role": "system", "content": system_prompt}] if system_prompt else []) + conversation

    temperature = payload.get("temperature")
    max_tokens = payload.get("max_tokens")
    metadata: dict[str, Any] = {"jamjet": asdict(ctx)}
    if payload.get("output_schema"):
        metadata["output_schema"] = payload["output_schema"]
    return ModelRequest(
        ref=parse_model_ref(model_id),
        messages=messages,
        tools=payload.get("tools") or None,
        temperature=temperature
        if isinstance(temperature, (int, float)) and not isinstance(temperature, bool)
        else None,
        max_tokens=max_tokens if isinstance(max_tokens, int) and not isinstance(max_tokens, bool) else None,
        metadata=metadata,
    )


def _tool_calls(message: Any) -> list[dict[str, Any]]:
    """The message's calls as the engine takes them: ``{id, name, arguments}``.

    Reads the OpenAI shape (``call.function.name``, arguments as JSON text)
    and the flat one. Arguments that are not valid JSON stay text, which the
    engine keeps as it is.
    """
    raw = _get(message, "tool_calls")
    if raw is None:
        return []
    if not isinstance(raw, list):
        raise ValueError(f"the model's message has tool_calls of {type(raw).__name__}, not a list")
    calls: list[dict[str, Any]] = []
    for i, call in enumerate(raw):
        fn = _get(call, "function")
        source = fn if fn is not None else call
        call_id, name = _get(call, "id"), _get(source, "name")
        if not isinstance(call_id, str) or not call_id:
            raise ValueError(f"the model's message has tool call {i} with no id")
        if not isinstance(name, str) or not name:
            raise ValueError(f"the model's message has tool call {i} with no name")
        if not _has(source, "arguments"):
            raise ValueError(f"the model's message has tool call {i} with no arguments")
        arguments = _get(source, "arguments")
        if isinstance(arguments, str):
            try:
                arguments = json.loads(arguments)
            except ValueError:
                pass
        calls.append({"id": call_id, "name": name, "arguments": arguments})
    return calls


def _tokens(response: ModelResponse, field: str) -> int:
    count = getattr(response, field)
    if isinstance(count, bool) or not isinstance(count, int) or not 0 <= count < 2**64:
        raise ValueError(f"the model reported {field} that is not a whole number from 0")
    return count


def model_result(response: Any) -> dict[str, Any]:
    """``response`` as the engine's ``model_result``, or raise why it cannot be one."""
    if not isinstance(response, ModelResponse):
        raise TypeError(f"the model returned {type(response).__name__}, not a ModelResponse")
    message = response.message
    if message is None:
        raise ValueError("the model's message is missing")
    content = _get(message, "content")
    if content is not None and not isinstance(content, str):
        raise ValueError(f"the model's message has content of {type(content).__name__}, not text")
    calls = _tool_calls(message)
    used = response.model
    if not isinstance(used, str) or not used:
        raise ValueError("the model did not report which model answered")

    # The engine refuses calls with any finish but `tool_calls`, and
    # `tool_calls` with no calls.
    reported_finish = _get(message, "finish_reason")
    if calls:
        finish_reason = "tool_calls"
    elif isinstance(reported_finish, str) and reported_finish and reported_finish != "tool_calls":
        finish_reason = reported_finish
    else:
        finish_reason = "stop"

    result: dict[str, Any] = {"content": content or "", "finish_reason": finish_reason, "model": used}
    if calls:
        result["tool_calls"] = calls
    if response.usage_reported is True:
        result["input_tokens"] = _tokens(response, "input_tokens")
        result["output_tokens"] = _tokens(response, "output_tokens")
        cost = response.cost_usd
        if cost is not None:
            if not (
                isinstance(cost, (int, float)) and not isinstance(cost, bool) and math.isfinite(cost) and cost >= 0
            ):
                raise ValueError(f"the model reported a cost_usd that is not a finite number from 0: {cost!r}")
            result["cost_usd"] = float(cost)
    return result


async def run_model_step(
    model: Model, payload: dict[str, Any], ctx: StepContext, *, timeout_s: float
) -> dict[str, Any]:
    """Run the model step of ``payload`` through ``model`` and return its ``model_result``.

    ``model`` must be a :class:`~jamjet.model.Model`, so the call goes
    through the seam and its middleware. The call is cut after ``timeout_s``
    seconds. Raises with the reason when the step cannot be answered.
    """
    if not isinstance(model, Model):
        raise TypeError(
            f"a worker model step needs a jamjet.model.Model, the seam with its middleware; got {type(model).__name__}"
        )
    request = build_request(payload, ctx)
    cut = asyncio.timeout(timeout_s)
    try:
        async with cut:
            response = await model.complete(request)
    except TimeoutError:
        if not cut.expired():
            raise
        raise TimeoutError(f"the model gave no answer within {timeout_s:g} s") from None
    return model_result(response)
