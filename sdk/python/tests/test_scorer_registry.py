"""Tests for the custom eval scorer plugin registry (task 3.14)."""

from __future__ import annotations

import pytest

from jamjet.eval.registry import (
    ScorerDefinition,
    ScorerRegistry,
    ScorerResult,
    get_scorer_registry,
    invoke_scorer,
    scorer,
)

# ── ScorerResult construction ────────────────────────────────────────────────


def test_scorer_result_basic():
    result = ScorerResult(score=0.85, passed=True, reason="Looks good")
    assert result.score == 0.85
    assert result.passed is True
    assert result.reason == "Looks good"
    assert result.metadata is None


def test_scorer_result_with_metadata():
    result = ScorerResult(score=0.0, passed=False, metadata={"key": "val"})
    assert result.score == 0.0
    assert result.passed is False
    assert result.reason is None
    assert result.metadata == {"key": "val"}


def test_scorer_result_boundary_scores():
    low = ScorerResult(score=0.0, passed=False)
    high = ScorerResult(score=1.0, passed=True)
    assert low.score == 0.0
    assert high.score == 1.0


# ── ScorerDefinition ─────────────────────────────────────────────────────────


def test_scorer_definition_defaults():
    async def dummy(input, output, context):
        return ScorerResult(score=1.0, passed=True)

    defn = ScorerDefinition(name="test", fn=dummy)
    assert defn.name == "test"
    assert defn.fn is dummy
    assert defn.description is None
    assert defn.version == "1.0"


def test_scorer_definition_custom_fields():
    def sync_fn(input, output, context):
        return ScorerResult(score=0.5, passed=True)

    defn = ScorerDefinition(name="my_scorer", fn=sync_fn, description="Custom", version="2.0")
    assert defn.description == "Custom"
    assert defn.version == "2.0"


# ── ScorerRegistry ──────────────────────────────────────────────────────────


def test_registry_register_and_get():
    registry = ScorerRegistry()

    async def my_fn(input, output, context):
        return ScorerResult(score=1.0, passed=True)

    registry.register("my_scorer", my_fn, description="A scorer")
    defn = registry.get("my_scorer")
    assert defn is not None
    assert defn.name == "my_scorer"
    assert defn.fn is my_fn
    assert defn.description == "A scorer"


def test_registry_get_nonexistent():
    registry = ScorerRegistry()
    assert registry.get("nonexistent") is None


def test_registry_list():
    registry = ScorerRegistry()

    async def fn(input, output, context):
        return ScorerResult(score=1.0, passed=True)

    registry.register("beta", fn)
    registry.register("alpha", fn)
    registry.register("gamma", fn)
    assert registry.list() == ["alpha", "beta", "gamma"]


def test_registry_list_empty():
    registry = ScorerRegistry()
    assert registry.list() == []


def test_registry_unregister():
    registry = ScorerRegistry()

    async def fn(input, output, context):
        return ScorerResult(score=1.0, passed=True)

    registry.register("temp", fn)
    assert "temp" in registry
    registry.unregister("temp")
    assert "temp" not in registry
    assert registry.get("temp") is None


def test_registry_unregister_nonexistent():
    registry = ScorerRegistry()
    with pytest.raises(KeyError, match="not registered"):
        registry.unregister("ghost")


def test_registry_duplicate_raises():
    registry = ScorerRegistry()

    async def fn(input, output, context):
        return ScorerResult(score=1.0, passed=True)

    registry.register("dup", fn)
    with pytest.raises(ValueError, match="already registered"):
        registry.register("dup", fn)


def test_registry_duplicate_overwrite():
    registry = ScorerRegistry()

    async def fn1(input, output, context):
        return ScorerResult(score=0.5, passed=True)

    async def fn2(input, output, context):
        return ScorerResult(score=0.9, passed=True)

    registry.register("overwritable", fn1)
    registry.register("overwritable", fn2, overwrite=True)
    defn = registry.get("overwritable")
    assert defn is not None
    assert defn.fn is fn2


def test_registry_contains():
    registry = ScorerRegistry()

    async def fn(input, output, context):
        return ScorerResult(score=1.0, passed=True)

    registry.register("exists", fn)
    assert "exists" in registry
    assert "missing" not in registry


def test_registry_len():
    registry = ScorerRegistry()
    assert len(registry) == 0

    async def fn(input, output, context):
        return ScorerResult(score=1.0, passed=True)

    registry.register("a", fn)
    registry.register("b", fn)
    assert len(registry) == 2


# ── Built-in scorers are registered by default ───────────────────────────────


def test_builtins_registered(clean_registry):
    """The 4 built-in scorers should be auto-registered in the singleton."""
    registry = get_scorer_registry()
    builtins = {"llm_judge", "assertion", "latency", "cost"}
    for name in builtins:
        defn = registry.get(name)
        assert defn is not None, f"Built-in scorer '{name}' not found"
        assert defn.description is not None


def test_builtin_names_in_list(clean_registry):
    registry = get_scorer_registry()
    names = registry.list()
    for expected in ["assertion", "cost", "latency", "llm_judge"]:
        assert expected in names


# ── get_scorer_registry returns singleton ─────────────────────────────────────


def test_singleton_identity(clean_registry):
    r1 = get_scorer_registry()
    r2 = get_scorer_registry()
    assert r1 is r2


# ── @scorer decorator ────────────────────────────────────────────────────────


def test_scorer_decorator_registers(clean_registry):
    @scorer(name="test_dec", description="Decorator test")
    async def my_scorer(input, output, context):
        return ScorerResult(score=0.7, passed=True, reason="ok")

    registry = get_scorer_registry()
    defn = registry.get("test_dec")
    assert defn is not None
    assert defn.fn is my_scorer
    assert defn.description == "Decorator test"


def test_scorer_decorator_preserves_function(clean_registry):
    @scorer(name="test_preserve")
    async def original_fn(input, output, context):
        return ScorerResult(score=1.0, passed=True)

    # The decorator should return the original function unchanged
    assert original_fn.__name__ == "original_fn"


def test_scorer_decorator_with_version(clean_registry):
    @scorer(name="versioned", version="2.5")
    async def versioned_scorer(input, output, context):
        return ScorerResult(score=1.0, passed=True)

    registry = get_scorer_registry()
    defn = registry.get("versioned")
    assert defn is not None
    assert defn.version == "2.5"


def test_scorer_decorator_duplicate_raises(clean_registry):
    @scorer(name="first_time")
    async def first(input, output, context):
        return ScorerResult(score=1.0, passed=True)

    with pytest.raises(ValueError, match="already registered"):

        @scorer(name="first_time")
        async def second(input, output, context):
            return ScorerResult(score=0.5, passed=True)


def test_scorer_decorator_overwrite(clean_registry):
    @scorer(name="replaceable")
    async def original(input, output, context):
        return ScorerResult(score=0.5, passed=True)

    @scorer(name="replaceable", overwrite=True)
    async def replacement(input, output, context):
        return ScorerResult(score=0.9, passed=True)

    registry = get_scorer_registry()
    defn = registry.get("replaceable")
    assert defn is not None
    assert defn.fn is replacement


# ── Async scorer invocation ──────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_invoke_async_scorer(clean_registry):
    @scorer(name="async_test")
    async def async_scorer(input, output, context):
        return ScorerResult(
            score=0.9,
            passed=True,
            reason=f"Processed {input.get('query')}",
        )

    result = await invoke_scorer(
        "async_test",
        input={"query": "hello"},
        output={"answer": "world"},
        context={},
    )
    assert result.score == 0.9
    assert result.passed is True
    assert "hello" in result.reason


@pytest.mark.asyncio
async def test_invoke_sync_scorer(clean_registry):
    @scorer(name="sync_test")
    def sync_scorer(input, output, context):
        return ScorerResult(score=0.6, passed=True, reason="sync ok")

    result = await invoke_scorer(
        "sync_test",
        input={},
        output={},
        context={},
    )
    assert result.score == 0.6
    assert result.passed is True


@pytest.mark.asyncio
async def test_invoke_scorer_not_found(clean_registry):
    with pytest.raises(KeyError, match="not registered"):
        await invoke_scorer("nonexistent_scorer", input={}, output={}, context={})


@pytest.mark.asyncio
async def test_invoke_scorer_default_context(clean_registry):
    @scorer(name="ctx_default")
    async def ctx_scorer(input, output, context):
        return ScorerResult(score=1.0, passed=True, metadata={"ctx": context})

    result = await invoke_scorer("ctx_default", input={}, output={})
    assert result.metadata == {"ctx": {}}


# ── Built-in scorer invocation ───────────────────────────────────────────────


@pytest.mark.asyncio
async def test_invoke_builtin_llm_judge(clean_registry):
    result = await invoke_scorer("llm_judge", input={}, output={}, context={})
    assert result.passed is True
    assert result.metadata is not None
    assert result.metadata["builtin"] is True


@pytest.mark.asyncio
async def test_invoke_builtin_assertion_pass(clean_registry):
    result = await invoke_scorer(
        "assertion",
        input={"query": "hello"},
        output={"answer": "world"},
        context={"expression": "'answer' in output"},
    )
    assert result.passed is True
    assert result.score == 1.0


@pytest.mark.asyncio
async def test_invoke_builtin_assertion_fail(clean_registry):
    result = await invoke_scorer(
        "assertion",
        input={},
        output={"answer": "world"},
        context={"expression": "'missing' in output"},
    )
    assert result.passed is False
    assert result.score == 0.0


@pytest.mark.asyncio
async def test_invoke_builtin_assertion_error(clean_registry):
    result = await invoke_scorer(
        "assertion",
        input={},
        output={},
        context={"expression": "1 / 0"},
    )
    assert result.passed is False
    assert "error" in result.reason.lower()


@pytest.mark.asyncio
async def test_invoke_builtin_latency_pass(clean_registry):
    result = await invoke_scorer(
        "latency",
        input={},
        output={},
        context={"threshold_ms": 5000, "duration_ms": 3000},
    )
    assert result.passed is True


@pytest.mark.asyncio
async def test_invoke_builtin_latency_fail(clean_registry):
    result = await invoke_scorer(
        "latency",
        input={},
        output={},
        context={"threshold_ms": 1000, "duration_ms": 2000},
    )
    assert result.passed is False


@pytest.mark.asyncio
async def test_invoke_builtin_latency_no_data(clean_registry):
    result = await invoke_scorer("latency", input={}, output={}, context={})
    assert result.passed is True
    assert result.score == 1.0


@pytest.mark.asyncio
async def test_invoke_builtin_cost_pass(clean_registry):
    result = await invoke_scorer(
        "cost",
        input={},
        output={},
        context={"threshold_usd": 1.0, "cost_usd": 0.5},
    )
    assert result.passed is True


@pytest.mark.asyncio
async def test_invoke_builtin_cost_fail(clean_registry):
    result = await invoke_scorer(
        "cost",
        input={},
        output={},
        context={"threshold_usd": 0.01, "cost_usd": 0.05},
    )
    assert result.passed is False


@pytest.mark.asyncio
async def test_invoke_builtin_cost_no_data(clean_registry):
    result = await invoke_scorer("cost", input={}, output={}, context={})
    assert result.passed is True
    assert result.score == 1.0


# ── Custom scorer integration with EvalNode ──────────────────────────────────


def test_evalnode_compiles_registered_scorer(clean_registry):
    """A scorer referenced by name that exists in the registry should compile as type='custom'."""

    @scorer(name="my_custom_scorer", description="Test custom")
    async def my_custom(input, output, context):
        return ScorerResult(score=1.0, passed=True)

    from jamjet.workflow.nodes import EvalNode

    node = EvalNode(
        scorers=[{"name": "my_custom_scorer", "extra_param": "value"}],
    )
    ir = node.to_ir_kind()
    assert ir["type"] == "eval"
    assert len(ir["scorers"]) == 1

    compiled = ir["scorers"][0]
    assert compiled["type"] == "custom"
    assert compiled["scorer_ref"] == "my_custom_scorer"
    assert compiled["kwargs"]["extra_param"] == "value"
    # The "name" key should not leak into kwargs
    assert "name" not in compiled["kwargs"]


def test_evalnode_compiles_builtin_types_unchanged(clean_registry):
    """Built-in scorer types should still compile via the explicit type-based branches."""
    from jamjet.workflow.nodes import EvalNode

    node = EvalNode(
        scorers=[
            {"type": "llm_judge", "model": "gpt-4", "rubric": "Rate 1-5"},
            {"type": "assertion", "checks": ["'x' in output"]},
            {"type": "latency", "threshold_ms": 3000},
            {"type": "cost", "threshold_usd": 0.5},
        ],
    )
    ir = node.to_ir_kind()
    types = [s["type"] for s in ir["scorers"]]
    assert types == ["llm_judge", "assertion", "latency", "cost"]


def test_evalnode_unknown_scorer_passthrough(clean_registry):
    """Scorers with no type and no matching registry entry pass through as-is."""
    from jamjet.workflow.nodes import EvalNode

    node = EvalNode(
        scorers=[{"name": "totally_unknown_scorer_xyz"}],
    )
    ir = node.to_ir_kind()
    # Should pass through unchanged since "totally_unknown_scorer_xyz" is not registered
    compiled = ir["scorers"][0]
    assert compiled.get("type") != "custom"
    assert compiled.get("name") == "totally_unknown_scorer_xyz"


def test_evalnode_explicit_custom_type_with_scorer_ref(clean_registry):
    """A scorer with type='custom' should preserve scorer_ref."""
    from jamjet.workflow.nodes import EvalNode

    node = EvalNode(
        scorers=[{"type": "custom", "scorer_ref": "external_scorer", "kwargs": {"k": "v"}}],
    )
    ir = node.to_ir_kind()
    compiled = ir["scorers"][0]
    assert compiled["type"] == "custom"
    assert compiled["scorer_ref"] == "external_scorer"
    assert compiled["kwargs"] == {"k": "v"}


# ── Top-level export ─────────────────────────────────────────────────────────


def test_scorer_importable_from_jamjet():
    from jamjet import scorer as s

    assert callable(s)


def test_registry_types_importable_from_eval():
    from jamjet.eval import (
        CustomScorerResult,
        ScorerDefinition,
        ScorerRegistry,
        get_scorer_registry,
        invoke_scorer,
        scorer,
    )

    assert ScorerRegistry is not None
    assert ScorerDefinition is not None
    assert CustomScorerResult is not None
    assert callable(get_scorer_registry)
    assert callable(invoke_scorer)
    assert callable(scorer)


# ── Fixture to reset the singleton registry between tests ────────────────────


@pytest.fixture
def clean_registry():
    """Reset the module-level scorer registry singleton before each test."""
    import jamjet.eval.registry as reg

    reg._SCORER_REGISTRY = None
    yield
    reg._SCORER_REGISTRY = None
