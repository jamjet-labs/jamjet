"""The Python SDK against the shared Cloud SDK contract in sdk/conformance
(see its README): the vendored jamjet-policy decision suite, glob vectors,
and the price table. The TypeScript SDK runs the same files."""

from __future__ import annotations

import importlib
import json
from pathlib import Path
from typing import Any

import pytest
import yaml

from jamjet.cloud.policy import PolicyEvaluator

CONFORMANCE = Path(__file__).resolve().parents[3] / "conformance"
VECTORS: dict[str, Any] = json.loads((CONFORMANCE / "cloud-sdk-vectors.json").read_text(encoding="utf-8"))
SUITE: dict[str, Any] = yaml.safe_load((CONFORMANCE / "policy-decisions.yaml").read_text(encoding="utf-8"))

patcher = importlib.import_module("jamjet.cloud.patcher")


def _pricing() -> Any:
    return importlib.import_module("jamjet.cloud.pricing")


_DECISION = {"block": "BLOCKED", "require_approval": "WAITING_FOR_APPROVAL", "audit": "AUDIT", "allow": "ALLOWED"}


def test_suite_is_present() -> None:
    assert len(SUITE["cases"]) >= 30


@pytest.mark.parametrize(
    "case", [c for c in SUITE["cases"] if not c.get("requires_mcp_prefix_strip")], ids=lambda c: c["id"]
)
def test_policy_decision(case: dict[str, Any]) -> None:
    ev = PolicyEvaluator()
    for rule in case["policy"]["rules"]:
        ev.add(rule["action"], rule["match"])
    decision = ev.evaluate(case["tool"])
    assert _DECISION[decision.policy_kind] == case["expect"]["decision"]
    assert decision.pattern == case["expect"]["rule"]


@pytest.mark.parametrize("vector", VECTORS["glob"], ids=lambda v: f"{v['pattern']!r}~{v['name']!r}")
def test_glob_vector(vector: dict[str, Any]) -> None:
    ev = PolicyEvaluator()
    ev.add("block", vector["pattern"])
    assert ev.evaluate(vector["name"]).blocked is vector["match"]


def test_price_table_equals_the_shared_table() -> None:
    shared = {model: (p["input"], p["output"], p.get("cache_read")) for model, p in VECTORS["prices"]["models"].items()}
    pricing = _pricing()
    assert pricing.PRICES_PER_MTOK == shared
    fallback = VECTORS["prices"]["fallback"]
    assert pricing.FALLBACK_PER_MTOK == (fallback["input"], fallback["output"])


@pytest.mark.parametrize("vector", VECTORS["cost"], ids=lambda v: f"{v['model']}-{v['input_tokens']}")
def test_cost_vector(vector: dict[str, Any]) -> None:
    cost = _pricing().estimate_cost(
        vector["model"],
        vector["input_tokens"],
        vector["output_tokens"],
        cache_read=vector.get("cache_read_tokens", 0),
        cache_write=vector.get("cache_write_tokens", 0),
        cache_write_1h=vector.get("cache_write_1h_tokens", 0),
    )
    assert cost == pytest.approx(vector["usd"], abs=1e-12)


@pytest.mark.parametrize("vector", VECTORS["cache_read_savings"], ids=lambda v: v["model"])
def test_cache_read_savings_vector(vector: dict[str, Any]) -> None:
    saved = _pricing().cache_read_savings_usd(vector["model"], vector["cache_read_tokens"])
    assert saved == pytest.approx(vector["usd"], abs=1e-12)


def test_matching_ignores_the_platform_path_case_rules(monkeypatch: pytest.MonkeyPatch) -> None:
    """fnmatch.fnmatch folds case through os.path.normcase, which lower-cases on
    Windows. Simulate that: the evaluator must stay case-sensitive."""
    import os

    monkeypatch.setattr(os.path, "normcase", lambda s: s.lower())
    ev = PolicyEvaluator()
    ev.add("block", "*delete*")
    assert ev.evaluate("Database.DELETE_all").blocked is False


@pytest.mark.parametrize("vector", VECTORS["normalize"], ids=lambda v: v["model"])
def test_model_id_normalization(vector: dict[str, Any]) -> None:
    assert _pricing().normalize_model(vector["model"]) == vector["as"]


def test_patcher_charges_anthropic_cache_tokens(monkeypatch: pytest.MonkeyPatch) -> None:
    budget = importlib.import_module("jamjet.cloud.budget")
    monkeypatch.setattr(patcher, "emit", lambda _e: None)
    budget.set_budget(100.0)
    try:
        usage = {
            "input_tokens": 1000,
            "output_tokens": 500,
            "cache_read_input_tokens": 10000,
            "cache_creation_input_tokens": 2000,
        }

        class Response:
            model = "claude-sonnet-4-6"
            content: list[Any] = []

            def __init__(self) -> None:
                self.usage = type("Usage", (), usage)()

        patcher._run_enforced(
            "anthropic", lambda _s, **_k: Response(), None, (), {"model": "claude-sonnet-4-6", "messages": []}
        )
        # 1000*$3 + 500*$15 + 10000*$0.30 + 2000*$3.75, per million
        assert budget.get_budget().spent == pytest.approx(0.021)
    finally:
        budget.set_budget(None)


def test_one_hour_cache_writes_are_split_out() -> None:
    split = patcher._cache_writes(
        {
            "cache_creation_input_tokens": 3000,
            "cache_creation": {"ephemeral_5m_input_tokens": 1000, "ephemeral_1h_input_tokens": 2000},
        }
    )
    assert split == {"cache_write": 1000, "cache_write_1h": 2000}
    assert patcher._cache_writes({"cache_creation_input_tokens": 3000}) == {"cache_write": 3000, "cache_write_1h": 0}
