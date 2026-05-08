import pytest
from pydantic import ValidationError

from jamjet.spec import ToolSpec


def test_minimal_tool():
    t = ToolSpec(
        name="web_search",
        description="Search the web",
        input_schema={"type": "object", "properties": {"q": {"type": "string"}}},
        handler_ref="my.module:web_search",
    )
    assert t.is_mcp is False


def test_mcp_flag():
    t = ToolSpec(
        name="fs",
        description="Filesystem MCP server",
        input_schema={"type": "object"},
        handler_ref="mcp-fs-server",
        is_mcp=True,
    )
    assert t.is_mcp is True


def test_round_trip_json():
    t = ToolSpec(name="x", description="y", input_schema={}, handler_ref="m:f")
    assert ToolSpec.model_validate_json(t.model_dump_json()) == t


def test_extra_fields_rejected():
    with pytest.raises(ValidationError):
        ToolSpec(name="x", description="y", input_schema={}, handler_ref="m:f", surprise=1)
