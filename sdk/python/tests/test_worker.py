"""Tests for `jamjet worker` — python_tool work-item consumer.

Covers scenarios:
  (a) A claimed item whose payload names a module+function runs the handler
      and posts complete with the output.
  (b) A handler that raises causes fail_work_item to be posted.
  (c) --once with no claimed item is a clean no-op.
  (d) Seed injection: same execution_id yields same random value (Task 4).
  (e) GenAI fields in the output dict are forwarded to complete_work_item (Task 5).

No live runtime is required; a _StubClient records all outbound calls.
"""

from __future__ import annotations

from jamjet.cli.main import _worker_loop  # noqa: E402

# ── Test handler functions ────────────────────────────────────────────────────


async def _add(input: dict) -> dict:
    """Returns the sum of a + b."""
    return {"sum": input["a"] + input["b"]}


async def _boom(input: dict) -> dict:
    """Always raises to exercise the fail path."""
    raise RuntimeError("intentional failure")


def _read_seed(input: dict) -> dict:
    """Return the first value from the injected seeded random (sync handler)."""
    from jamjet.runtime.local.seed import get_current_random

    rng = get_current_random()
    return {"seed_value": rng.random() if rng is not None else None}


async def _with_genai(input: dict) -> dict:
    """Return output that includes optional GenAI telemetry fields."""
    return {
        "result": "answer",
        "gen_ai_model": "claude-3-5-sonnet-20241022",
        "finish_reason": "stop",
    }


# ── Stub client ───────────────────────────────────────────────────────────────


class _StubClient:
    """Minimal async fake of JamjetClient for worker unit tests."""

    def __init__(self, *, claimed_item: dict | None = None) -> None:
        self._claimed_item = claimed_item
        self.complete_calls: list[dict] = []
        self.fail_calls: list[dict] = []
        self.heartbeat_calls: list[dict] = []

    async def claim_work_item(self, worker_id: str, queue_types: list[str]) -> dict | None:
        return self._claimed_item

    async def complete_work_item(
        self,
        item_id: str,
        execution_id: str | None,
        node_id: str | None,
        output: object,
        state_patch: dict,
        duration_ms: int = 0,
        gen_ai_model: str | None = None,
        finish_reason: str | None = None,
    ) -> None:
        self.complete_calls.append(
            {
                "item_id": item_id,
                "execution_id": execution_id,
                "node_id": node_id,
                "output": output,
                "gen_ai_model": gen_ai_model,
                "finish_reason": finish_reason,
            }
        )

    async def fail_work_item(self, item_id: str, error: str) -> None:
        self.fail_calls.append({"item_id": item_id, "error": error})

    async def heartbeat_work_item(self, item_id: str, worker_id: str, lease_fence: int = 0) -> None:
        self.heartbeat_calls.append({"item_id": item_id, "lease_fence": lease_fence})


# ── Fixtures / constants ──────────────────────────────────────────────────────

_ADD_ITEM: dict = {
    "id": "wi-001",
    "execution_id": "exec_abc",
    "node_id": "add_step",
    "queue_type": "python_tool",
    "payload": {
        "module": "tests.test_worker",
        "function": "_add",
        "input": {"a": 3, "b": 4},
    },
    "lease_fence": 0,
}

_BOOM_ITEM: dict = {
    "id": "wi-002",
    "execution_id": "exec_def",
    "node_id": "boom_step",
    "queue_type": "python_tool",
    "payload": {
        "module": "tests.test_worker",
        "function": "_boom",
        "input": {},
    },
    "lease_fence": 0,
}

_SEED_ITEM: dict = {
    "id": "wi-003",
    "execution_id": "exec_seed_abc",
    "node_id": "seed_step",
    "queue_type": "python_tool",
    "payload": {
        "module": "tests.test_worker",
        "function": "_read_seed",
        "input": {},
    },
    "lease_fence": 0,
}

_GENAI_ITEM: dict = {
    "id": "wi-004",
    "execution_id": "exec_genai",
    "node_id": "genai_step",
    "queue_type": "python_tool",
    "payload": {
        "module": "tests.test_worker",
        "function": "_with_genai",
        "input": {},
    },
    "lease_fence": 0,
}


# ── Tests ─────────────────────────────────────────────────────────────────────


async def test_worker_runs_handler_and_posts_complete() -> None:
    """(a) Claimed item with a valid handler calls complete_work_item with the output."""
    stub = _StubClient(claimed_item=_ADD_ITEM)
    await _worker_loop(stub, "test-worker", ["python_tool"], once=True)

    assert len(stub.complete_calls) == 1, "complete_work_item must be called exactly once"
    call = stub.complete_calls[0]
    assert call["item_id"] == "wi-001"
    assert call["execution_id"] == "exec_abc"
    assert call["node_id"] == "add_step"
    assert call["output"] == {"sum": 7}
    assert len(stub.fail_calls) == 0, "fail must not be called on success"


async def test_worker_posts_fail_on_handler_exception() -> None:
    """(b) When the handler raises, fail_work_item is posted; complete is not called."""
    stub = _StubClient(claimed_item=_BOOM_ITEM)
    await _worker_loop(stub, "test-worker", ["python_tool"], once=True)

    assert len(stub.fail_calls) == 1, "fail_work_item must be called exactly once"
    call = stub.fail_calls[0]
    assert call["item_id"] == "wi-002"
    assert "intentional failure" in call["error"]
    assert len(stub.complete_calls) == 0, "complete must not be called on failure"


async def test_worker_once_empty_queue_is_noop() -> None:
    """(c) --once with no claimed item exits cleanly; complete and fail are never called."""
    stub = _StubClient(claimed_item=None)
    await _worker_loop(stub, "test-worker", ["python_tool"], once=True)

    assert len(stub.complete_calls) == 0
    assert len(stub.fail_calls) == 0


async def test_worker_seed_injection_deterministic() -> None:
    """(d) Same execution_id yields the same random value via injected context."""
    # Run twice with the same execution_id — seeds must be identical.
    stub_a = _StubClient(claimed_item=_SEED_ITEM)
    await _worker_loop(stub_a, "test-worker", ["python_tool"], once=True)

    stub_b = _StubClient(claimed_item=_SEED_ITEM)
    await _worker_loop(stub_b, "test-worker", ["python_tool"], once=True)

    output_a = stub_a.complete_calls[0]["output"]["seed_value"]
    output_b = stub_b.complete_calls[0]["output"]["seed_value"]
    assert output_a is not None, "seed must be injected before handler invocation"
    assert output_a == output_b, (
        f"same execution_id must yield same seed: {output_a} != {output_b}"
    )


async def test_worker_forwards_gen_ai_fields() -> None:
    """(e) GenAI fields in the output dict are forwarded to complete_work_item."""
    stub = _StubClient(claimed_item=_GENAI_ITEM)
    await _worker_loop(stub, "test-worker", ["python_tool"], once=True)

    assert len(stub.complete_calls) == 1
    call = stub.complete_calls[0]
    assert call["gen_ai_model"] == "claude-3-5-sonnet-20241022"
    assert call["finish_reason"] == "stop"
    # The full output dict (including GenAI fields) is still forwarded as-is.
    assert call["output"]["result"] == "answer"
