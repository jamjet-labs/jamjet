"""A command that sends an unsafe call and loses the answer says so, and exits 1."""

from __future__ import annotations

from typing import Any

import httpx
import pytest
import respx
from typer.testing import CliRunner

from jamjet.cli.main import app
from jamjet.transport import OutcomeUnknown

BASE = "http://engine.test:7700"
runner = CliRunner()


@pytest.fixture(autouse=True)
def _no_waits(monkeypatch: pytest.MonkeyPatch) -> None:
    async def instant(seconds: float) -> None:
        return None

    monkeypatch.setattr("jamjet.transport._sleep", instant)


def _lost(*args: Any) -> Any:
    with respx.mock(assert_all_called=False) as mock:
        route = mock.route(method=args[0], url=f"{BASE}{args[1]}").mock(side_effect=httpx.ReadTimeout("slow"))
        result = runner.invoke(app, list(args[2:]))
    return result, route


def test_approve_with_a_lost_answer_says_the_outcome_is_unknown() -> None:
    result, route = _lost("POST", "/executions/exec_1/approve", "approve", "exec_1", "-d", "approved", "-r", BASE)
    assert result.exit_code == 1
    assert "may have taken effect" in result.output
    assert "Traceback" not in result.output
    assert route.call_count == 1  # an unsafe call is sent once


def test_activate_with_a_lost_answer_says_the_outcome_is_unknown() -> None:
    result, route = _lost("POST", "/agents/a1/activate", "agents", "activate", "a1", "-r", BASE)
    assert result.exit_code == 1
    assert "may have taken effect" in result.output
    assert route.call_count == 1


def test_run_with_a_lost_start_says_the_outcome_is_unknown(monkeypatch: pytest.MonkeyPatch) -> None:
    class FakeClient:
        async def __aenter__(self) -> FakeClient:
            return self

        async def __aexit__(self, *args: Any) -> None:
            return None

        async def start_execution(self, **kwargs: Any) -> dict[str, Any]:
            raise OutcomeUnknown("start_execution may have taken effect; check before repeating", operation="start")

    monkeypatch.setattr("jamjet.cli.main._client", lambda runtime, token=None: FakeClient())
    result = runner.invoke(app, ["run", "some-workflow", "--no-follow", "--runtime", BASE])
    assert result.exit_code == 1
    assert "may have taken effect" in result.output
    assert "Traceback" not in result.output
