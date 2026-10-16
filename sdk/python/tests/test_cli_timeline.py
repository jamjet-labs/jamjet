"""Tests for the followed text-mode execution timeline."""

from __future__ import annotations

from typing import Any

import pytest
from typer.testing import CliRunner

from jamjet.cli.main import app

runner = CliRunner()


def _event(event_type: str, node_id: str, **fields: Any) -> dict[str, Any]:
    return {"kind": {"type": event_type, "node_id": node_id, **fields}}


@pytest.fixture()
def fake_client(monkeypatch: pytest.MonkeyPatch):
    class FakeClient:
        events: list[dict[str, Any]] = []
        events_response: Any = None
        events_error: Exception | None = None
        event_fetches = 0
        final_status = "completed"

        def __init__(self, *args: Any, **kwargs: Any) -> None:
            self.events = list(type(self).events)
            self.events_response = type(self).events_response
            self.events_error = type(self).events_error
            self.event_fetches = 0
            self.final_status = type(self).final_status

        async def __aenter__(self) -> FakeClient:
            return self

        async def __aexit__(self, *args: Any) -> None:
            return None

        async def start_execution(self, **kwargs: Any) -> dict[str, str]:
            return {"execution_id": "exec_timeline"}

        async def get_execution(self, execution_id: str) -> dict[str, str]:
            return {"status": self.final_status}

        async def get_events(self, execution_id: str) -> dict[str, Any]:
            self.event_fetches += 1
            type(self).event_fetches = self.event_fetches
            if self.events_error is not None:
                raise self.events_error
            return self.events_response if self.events_response is not None else {"events": self.events}

    monkeypatch.setattr("jamjet.cli.main._client", lambda runtime: FakeClient())
    monkeypatch.setattr("jamjet.cli.main.asyncio.sleep", _no_sleep)
    return FakeClient


async def _no_sleep(delay: float) -> None:
    return None


def _run_text(fake_client: type, events: list[dict[str, Any]], final_status: str = "completed") -> Any:
    fake_client.events = events
    fake_client.events_response = None
    fake_client.events_error = None
    fake_client.event_fetches = 0
    fake_client.final_status = final_status
    return runner.invoke(app, ["run", "test-wf", "--runtime", "http://fake:7700"])


def test_followed_text_run_renders_terminal_step_timeline(fake_client) -> None:
    result = _run_text(
        fake_client,
        [
            _event("node_completed", "fetch", duration_ms=2000),
            _event("node_failed", "transform"),
            _event("node_cancelled", "publish"),
            _event("node_skipped", "notify"),
        ],
    )

    assert result.exit_code == 0, result.output
    assert "Execution timeline" in result.output
    assert "completed" in result.output
    assert "failed" in result.output
    assert "cancelled" in result.output
    assert "skipped" in result.output
    assert "fetch" in result.output
    assert "Recorded step time: 2,000 ms" in result.output


def test_timeline_marks_unknown_timings_and_keeps_literal_node_ids(fake_client) -> None:
    node_id = "[bold]literal[/bold]"
    result = _run_text(
        fake_client,
        [
            _event("node_completed", node_id, duration_ms=0),
            _event("node_failed", "failed-step"),
        ],
    )

    assert result.exit_code == 0, result.output
    assert node_id in result.output
    assert "n/a" in result.output
    assert "Recorded step time: 0 ms (1 step timing unknown)" in result.output


def test_repeated_terminal_events_keep_same_node_id_in_recorded_order(fake_client) -> None:
    result = _run_text(
        fake_client,
        [
            _event("node_completed", "retry-step", duration_ms=11),
            _event("node_failed", "retry-step"),
        ],
    )

    assert result.exit_code == 0, result.output
    retry_lines = [line for line in result.output.splitlines() if "retry-step" in line]
    assert len(retry_lines) == 2
    assert "completed" in retry_lines[0]
    assert "failed" in retry_lines[1]


@pytest.mark.parametrize("final_status", ["failed", "cancelled"])
def test_timeline_follows_non_completed_terminal_status(fake_client, final_status: str) -> None:
    result = _run_text(
        fake_client,
        [_event("node_completed", "final-step", duration_ms=3)],
        final_status=final_status,
    )

    assert result.exit_code == 0, result.output
    assert f"Execution ended: {final_status}" in result.output
    assert "Execution timeline" in result.output
    assert "final-step" in result.output


def test_timeline_scales_known_bars_and_rejects_invalid_timings(fake_client) -> None:
    result = _run_text(
        fake_client,
        [
            _event("node_completed", "fast", duration_ms=1000),
            _event("node_completed", "slow", duration_ms=2000),
            _event("node_completed", "zero", duration_ms=0),
            _event("node_completed", "negative", duration_ms=-1),
            _event("node_completed", "float", duration_ms=2.5),
            _event("node_completed", "text", duration_ms="3"),
        ],
    )

    assert result.exit_code == 0, result.output
    assert "fast  1,000 ms  ██████░░░░░░" in result.output
    assert "slow  2,000 ms  ████████████" in result.output
    assert "zero  0 ms  ░░░░░░░░░░░░" in result.output
    assert result.output.count("n/a") == 3
    assert "Recorded step time: 3,000 ms (3 step timings unknown)" in result.output


def test_all_unknown_timeline_does_not_divide_by_zero(fake_client) -> None:
    result = _run_text(
        fake_client,
        [
            _event("node_failed", "failed"),
            _event("node_cancelled", "cancelled"),
            _event("node_skipped", "skipped"),
        ],
    )

    assert result.exit_code == 0, result.output
    assert result.output.count("n/a") == 3
    assert "Recorded step time: unavailable (all step timings unknown)" in result.output


def test_empty_timeline_is_reported_honestly(fake_client) -> None:
    result = _run_text(fake_client, [])

    assert result.exit_code == 0, result.output
    assert "Execution timeline unavailable: no terminal step events were recorded." in result.output


def test_timeline_fetch_failure_preserves_final_status(fake_client) -> None:
    fake_client.events_error = RuntimeError("transport unavailable")
    result = runner.invoke(app, ["run", "test-wf", "--runtime", "http://fake:7700"])

    assert result.exit_code == 0, result.output
    assert "Execution completed." in result.output
    assert "Execution timeline unavailable: could not fetch recorded step events." in result.output


def test_invalid_event_history_shape_gets_distinct_warning(fake_client) -> None:
    fake_client.events_response = {"events": "not-a-list"}
    result = runner.invoke(app, ["run", "test-wf", "--runtime", "http://fake:7700"])

    assert result.exit_code == 0, result.output
    assert "Execution completed." in result.output
    assert "Execution timeline unavailable: recorded step events had an invalid shape." in result.output
    assert "could not fetch" not in result.output


def test_no_follow_does_not_fetch_timeline(fake_client) -> None:
    result = runner.invoke(app, ["run", "test-wf", "--no-follow", "--runtime", "http://fake:7700"])

    assert result.exit_code == 0, result.output
    assert fake_client.event_fetches == 0
    assert "Execution timeline" not in result.output


def test_stream_keeps_stream_output_without_final_timeline(fake_client) -> None:
    fake_client.events = [_event("node_completed", "step", duration_ms=7)]
    result = runner.invoke(app, ["run", "test-wf", "--stream", "--runtime", "http://fake:7700"])

    assert result.exit_code == 0, result.output
    assert "Stream complete" in result.output
    assert "Execution timeline" not in result.output
    assert fake_client.event_fetches == 1


def test_json_output_remains_compact_and_fetches_events_once(fake_client) -> None:
    fake_client.events = [_event("node_completed", "step", duration_ms=7)]
    result = runner.invoke(app, ["run", "test-wf", "--output", "json", "--runtime", "http://fake:7700"])

    assert result.exit_code == 0, result.output
    assert "\n" not in result.stdout.strip()
    assert fake_client.event_fetches == 1
