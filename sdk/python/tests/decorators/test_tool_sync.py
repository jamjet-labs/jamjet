import asyncio

from jamjet import tool
from jamjet.tools.decorators import get_tool


@tool
def check_dates(venue: str) -> str:
    """List the free dates for one venue."""
    return f'["2026-12-04"] for {venue}'


def test_sync_tool_can_be_awaited():
    assert asyncio.run(check_dates(venue="Casa do Rio")) == '["2026-12-04"] for Casa do Rio'


def test_sync_tool_keeps_its_schema():
    assert check_dates.__name__ == "check_dates"
    schema = get_tool("check_dates").input_schema
    assert schema["properties"]["venue"] == {"type": "string"}
    assert schema["required"] == ["venue"]
