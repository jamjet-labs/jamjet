import pytest
from jsonschema import Draft202012Validator, ValidationError

from jamjet import Agent, tool
from jamjet.tools.decorators import get_tool


@tool
async def schema_search(
    query: str, limit: int, min_score: float, enabled: bool, options: dict, prefix: str = ""
) -> str:
    return prefix + query


def test_tool_parameters_are_valid_json_schemas_through_agent_compilation():
    definition = get_tool("schema_search")
    assert definition is not None
    spec = Agent("search", model="gpt-4o-mini", tools=[schema_search], strategy="react").compile()
    schema = spec.tools[0].input_schema

    assert schema == definition.input_schema
    Draft202012Validator.check_schema(schema)
    assert schema == {
        "type": "object",
        "properties": {
            "query": {"type": "string"},
            "limit": {"type": "integer"},
            "min_score": {"type": "number"},
            "enabled": {"type": "boolean"},
            "options": {"type": "object"},
            "prefix": {"type": "string"},
        },
        "required": ["query", "limit", "min_score", "enabled", "options"],
    }
    validator = Draft202012Validator(schema)
    arguments = {"query": "invoices", "limit": 3, "min_score": 0.5, "enabled": True, "options": {}}
    validator.validate(arguments)
    validator.validate({**arguments, "prefix": "customer: "})
    for name, value in [("query", 1), ("limit", "three"), ("min_score", "high"), ("enabled", "yes"), ("options", [])]:
        with pytest.raises(ValidationError):
            validator.validate({**arguments, name: value})
    with pytest.raises(ValidationError):
        validator.validate({key: value for key, value in arguments.items() if key != "query"})
