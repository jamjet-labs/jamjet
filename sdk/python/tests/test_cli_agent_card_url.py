"""``jamjet agents discover`` and ``jamjet a2a discover`` fetch the card the
runtime registers: the base's origin plus ``/.well-known/agent.json``.

They appended the path to the text, so ``discover https://h/team/x`` showed
``https://h/team/x/.well-known/agent.json`` while ``--register`` (the
runtime's ``POST /agents/discover``) registered ``https://h/.well-known/agent.json``.
"""

from __future__ import annotations

from typing import Any

import pytest
import typer
from typer.testing import CliRunner

from jamjet.cli import main as cli


@pytest.mark.parametrize(
    "base",
    [
        "https://agents.example.com",
        "https://agents.example.com/",
        "https://agents.example.com/team/x",
        "https://agents.example.com/any/path?",
        "https://agents.example.com/x?y=1#frag",
        "https://agents.example.com/admin/.well-known/agent.json",
    ],
)
def test_the_card_url_is_the_origin_plus_the_well_known_path(base: str) -> None:
    assert cli._agent_card_url(base) == "https://agents.example.com/.well-known/agent.json"


def test_the_port_is_kept() -> None:
    assert cli._agent_card_url("http://localhost:8080/a/b") == "http://localhost:8080/.well-known/agent.json"


@pytest.mark.parametrize("bad", ["ftp://h", "not a url", "https://user:pw@h", "file:///etc/passwd"])
def test_a_url_the_runtime_refuses_is_refused(bad: str) -> None:
    with pytest.raises(typer.BadParameter):
        cli._agent_card_url(bad)


class _Resp:
    status_code = 200
    text = "{}"

    def raise_for_status(self) -> None:
        return None

    def json(self) -> dict[str, Any]:
        return {"id": "a", "name": "agent", "capabilities": {}, "skills": []}


@pytest.mark.parametrize("command", [["agents", "discover"], ["a2a", "discover"]])
def test_both_commands_fetch_the_origin_card(monkeypatch: pytest.MonkeyPatch, command: list[str]) -> None:
    fetched: list[str] = []

    class _Client:
        def __init__(self, *args: Any, **kwargs: Any) -> None:
            pass

        async def __aenter__(self) -> _Client:
            return self

        async def __aexit__(self, *exc: object) -> None:
            return None

        async def get(self, url: str, **kwargs: Any) -> _Resp:
            fetched.append(str(url))
            return _Resp()

    monkeypatch.setattr("httpx.AsyncClient", _Client)
    result = CliRunner().invoke(cli.app, [*command, "https://agents.example.com/team/x?"])
    assert result.exit_code == 0, result.output
    assert fetched == ["https://agents.example.com/.well-known/agent.json"]
