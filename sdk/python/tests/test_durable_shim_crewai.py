"""CrewAI shim contract test."""

import pytest

pytest.importorskip("crewai")

from jamjet.crewai import durable_run
from jamjet.durable.context import get_execution_context


class _FakeCrew:
    def __init__(self, id: str | None = None):
        self.id = id


def test_durable_run_uses_crew_id():
    crew = _FakeCrew(id="crew-001")
    with durable_run(crew) as eid:
        assert eid == "crew-001"
        assert get_execution_context() == "crew-001"


def test_durable_run_generates_id_when_none():
    crew = _FakeCrew(id=None)
    with durable_run(crew) as eid:
        assert isinstance(eid, str) and len(eid) > 0


def test_durable_run_clears_after_block():
    crew = _FakeCrew(id="crew-002")
    with durable_run(crew):
        pass
    assert get_execution_context() is None
