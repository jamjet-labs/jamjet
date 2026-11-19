from typing import Literal, Optional

import pytest
from jsonschema import Draft202012Validator, ValidationError
from pydantic import BaseModel

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


class _Address(BaseModel):
    city: str
    postcode: str


class _Customer(BaseModel):
    name: str
    address: _Address


class _Opaque:
    """A class Pydantic cannot describe."""


@tool
async def schema_shapes(
    tags: list[str],
    scores: dict[str, float],
    mode: Literal["fast", "full"],
    handle: _Opaque,
    untyped,
    top_k: int | None = None,
    label: Optional[str] = None,  # noqa: UP045 - the older spelling is the point
) -> str:
    return ""


def test_container_optional_and_literal_parameters_say_what_they_accept():
    schema = get_tool("schema_shapes").input_schema
    Draft202012Validator.check_schema(schema)
    assert schema == {
        "type": "object",
        "properties": {
            "tags": {"type": "array", "items": {"type": "string"}},
            "scores": {"type": "object", "additionalProperties": {"type": "number"}},
            "mode": {"type": "string", "enum": ["fast", "full"]},
            # Pydantic has no schema for this class; it stays as @tool always described it.
            "handle": {"type": "object"},
            # No annotation means any value, not an object.
            "untyped": {},
            "top_k": {"anyOf": [{"type": "integer"}, {"type": "null"}]},
            "label": {"anyOf": [{"type": "string"}, {"type": "null"}]},
        },
        "required": ["tags", "scores", "mode", "handle", "untyped"],
    }
    validator = Draft202012Validator(schema)
    arguments = {"tags": ["a", "b"], "scores": {"a": 0.5}, "mode": "fast", "handle": {}, "untyped": "anything"}
    validator.validate(arguments)
    validator.validate({**arguments, "untyped": 7, "top_k": None, "label": "x"})
    validator.validate({**arguments, "top_k": 3, "label": None})
    for name, value in [
        ("tags", {"a": "b"}),
        ("tags", [1]),
        ("scores", {"a": "high"}),
        ("mode", "slow"),
        ("top_k", "three"),
        ("label", 5),
    ]:
        with pytest.raises(ValidationError):
            validator.validate({**arguments, name: value})


@tool
async def schema_models(buyer: _Customer, others: list[_Customer], ship_to: _Address | None = None) -> str:
    return ""


def test_model_parameters_share_one_set_of_definitions_at_the_schema_root():
    schema = get_tool("schema_models").input_schema
    Draft202012Validator.check_schema(schema)
    # Each model is defined once, at the root, where a "#/$defs/..." reference resolves.
    assert sorted(schema["$defs"]) == ["_Address", "_Customer"]
    assert schema["properties"] == {
        "buyer": {"$ref": "#/$defs/_Customer"},
        "others": {"type": "array", "items": {"$ref": "#/$defs/_Customer"}},
        "ship_to": {"anyOf": [{"$ref": "#/$defs/_Address"}, {"type": "null"}]},
    }
    assert schema["required"] == ["buyer", "others"]
    assert all("$defs" not in prop for prop in schema["properties"].values())

    validator = Draft202012Validator(schema)
    customer = {"name": "Ada", "address": {"city": "Leeds", "postcode": "LS1"}}
    validator.validate({"buyer": customer, "others": [customer], "ship_to": None})
    validator.validate({"buyer": customer, "others": [], "ship_to": {"city": "York", "postcode": "YO1"}})
    for arguments in [
        {"buyer": {"name": "Ada"}, "others": []},
        {"buyer": customer, "others": [{"name": "Bob", "address": {"city": "Hull"}}]},
        {"buyer": customer, "others": [], "ship_to": {"city": 1, "postcode": "YO1"}},
    ]:
        with pytest.raises(ValidationError):
            validator.validate(arguments)


def test_a_tool_without_model_parameters_has_no_definitions_key():
    assert "$defs" not in get_tool("schema_search").input_schema
    assert "$defs" not in get_tool("schema_shapes").input_schema


@tool
async def schema_no_parameters() -> str:
    return ""


def test_a_tool_with_no_parameters_is_an_empty_object_schema():
    assert get_tool("schema_no_parameters").input_schema == {"type": "object", "properties": {}, "required": []}


def _item_model(module: str, **fields: type) -> type[BaseModel]:
    return type("Item", (BaseModel,), {"__module__": module, "__annotations__": fields})


def test_models_with_the_same_class_name_keep_separate_definitions():
    orders_item = _item_model("shop.orders", sku=str)
    returns_item = _item_model("shop.returns", reason=int)

    async def schema_two_items(first, second, again) -> str:
        return ""

    schema_two_items.__annotations__ = {"first": orders_item, "second": returns_item, "again": orders_item}
    tool(schema_two_items)
    schema = get_tool("schema_two_items").input_schema

    Draft202012Validator.check_schema(schema)
    assert len(schema["$defs"]) == 2
    refs = {name: prop["$ref"] for name, prop in schema["properties"].items()}
    assert refs["first"] == refs["again"] != refs["second"]
    validator = Draft202012Validator(schema)
    validator.validate({"first": {"sku": "A1"}, "second": {"reason": 2}, "again": {"sku": "B2"}})
    with pytest.raises(ValidationError):
        validator.validate({"first": {"reason": 2}, "second": {"reason": 2}, "again": {"sku": "B2"}})
