"""Tests for jamjet.durable.keys — deterministic idempotency keys."""
import pytest

from jamjet.durable.keys import args_fingerprint, generate_key


def test_same_args_produce_same_key():
    k1 = generate_key("run-1", "module.fn", (1, 2), {"a": 3})
    k2 = generate_key("run-1", "module.fn", (1, 2), {"a": 3})
    assert k1 == k2


def test_different_execution_id_produces_different_key():
    k1 = generate_key("run-1", "module.fn", (1,), {})
    k2 = generate_key("run-2", "module.fn", (1,), {})
    assert k1 != k2


def test_different_function_produces_different_key():
    k1 = generate_key("run-1", "module.fn_a", (1,), {})
    k2 = generate_key("run-1", "module.fn_b", (1,), {})
    assert k1 != k2


def test_different_args_produce_different_key():
    k1 = generate_key("run-1", "module.fn", (1,), {})
    k2 = generate_key("run-1", "module.fn", (2,), {})
    assert k1 != k2


def test_kwargs_order_does_not_affect_key():
    k1 = generate_key("run-1", "fn", (), {"a": 1, "b": 2})
    k2 = generate_key("run-1", "fn", (), {"b": 2, "a": 1})
    assert k1 == k2


def test_key_is_hex_sha256_length():
    k = generate_key("run-1", "fn", (), {})
    assert len(k) == 64
    int(k, 16)  # raises ValueError if not hex


def test_unhashable_args_raise_clear_error():
    with pytest.raises(TypeError, match="not JSON-serializable"):
        # A function object is not serializable to canonical JSON.
        generate_key("run-1", "fn", (lambda: 1,), {})


def test_pydantic_model_fingerprint_deterministic():
    from pydantic import BaseModel

    class Foo(BaseModel):
        x: int
        y: str

    f1 = args_fingerprint((Foo(x=1, y="hi"),), {})
    f2 = args_fingerprint((Foo(x=1, y="hi"),), {})
    assert f1 == f2


def test_dict_with_nested_lists_deterministic():
    args = ({"users": [{"id": 1, "name": "a"}, {"id": 2, "name": "b"}]},)
    f1 = args_fingerprint(args, {})
    f2 = args_fingerprint(args, {})
    assert f1 == f2
