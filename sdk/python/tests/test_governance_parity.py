"""T3-7: enforcement PARITY across the in-process and durable paths + the
consolidated fail-closed adversarial duals.

The load-bearing invariant (plan, Global Constraints): a governance knob MUST
take effect on BOTH ``agent.run()`` (in-process) AND the durable IR, or it must
fail LOUD where it cannot.  NEVER a silent no-op.

What T3-7 wires
---------------
Before T3-7 the in-process seam was built ``default_model_middleware()`` WITHOUT
the agent's ``GovernanceConfig``, so ``budget=`` / ``policy=`` silently no-opped
on ``agent.run()`` (PII was already on by the factory default).  T3-7 threads
``agent.governance`` through ``agent.run()`` -> ``LocalRuntime.execute`` ->
``_run_agent`` -> ``get_adapter(spec.llm, governance)`` -> ``SeamAdapter`` ->
``Model(middleware=default_model_middleware(governance))`` so budget, the
policy-derived allowlist, and PII all ENFORCE on the in-process path at parity
with the durable path.

Parity matrix asserted here
---------------------------
================  ==========================  ===============================
knob              in-process (agent.run)      durable (compile_agent_to_ir)
================  ==========================  ===============================
budget=           BudgetExceededError (deny)  cost_budget_usd / token_budget
policy= allowlist ModelDeniedError    (deny)  policy.model_allowlist
pii=              redacted before backend     data_policy
approval_required UserWarning (fail-LOUD)     policy.require_approval_for
================  ==========================  ===============================

Run:
    uv run python -m pytest tests/test_governance_parity.py -v
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import sys
import warnings
from pathlib import Path
from typing import Any

import pytest

from jamjet import Agent, tool
from jamjet.agents.governance import Budget, normalize_governance
from jamjet.compiler.agent_ir import compile_agent_to_ir
from jamjet.model.budget import BudgetMiddleware
from jamjet.model.defaults import default_model_middleware
from jamjet.model.metering import MeteringMiddleware
from jamjet.model.middleware import (
    BudgetExceededError,
    ModelAllowlistMiddleware,
    ModelDeniedError,
)
from jamjet.model.pii import PiiRedactionMiddleware
from jamjet.model.policy_resolver import resolve_policy_allowlist
from jamjet.model.types import ModelRequest, parse_model_ref
from jamjet.runtime.local.llm_adapters import SeamAdapter, get_adapter

# Repo root (.../jamjet) from .../sdk/python/tests/<this file>.
_REPO_ROOT = Path(__file__).resolve().parents[3]


@tool
async def search(query: str) -> str:
    """A trivial tool that drives the in-process strategy loop (forces a model
    call that returns tool_calls, then a second model call)."""
    return f"Results for: {query}"


# ---------------------------------------------------------------------------
# Test harness — drive agent.run() through the REAL seam while controlling cost
# and capturing exactly what the backend received (post-middleware).
# ---------------------------------------------------------------------------


class _Backend:
    """Records every (redacted) request the backend receives + the call count.

    The conftest autouse fixture installs a litellm mock; we wrap its
    ``acompletion`` so we both count provider calls (to prove deny-before-provider)
    and snapshot the messages the provider actually saw (to prove PII redaction
    happened upstream at the seam).  ``cost`` sets a fixed per-call cost so the
    run-scoped budget accumulator trips deterministically.
    """

    def __init__(self) -> None:
        self.calls = 0
        self.seen: list[Any] = []


def _install_backend(monkeypatch: pytest.MonkeyPatch, *, cost: float = 0.0) -> _Backend:
    lm = sys.modules["litellm"]
    rec = _Backend()
    original = lm.acompletion

    async def _acompletion(model: str, messages: list, tools: list | None = None, **kw: object) -> object:
        rec.calls += 1
        # Snapshot what the provider actually received (after seam middleware).
        rec.seen.append(json.loads(json.dumps(messages, default=str)))
        return await original(model=model, messages=messages, tools=tools, **kw)

    monkeypatch.setattr(lm, "acompletion", _acompletion)
    monkeypatch.setattr(lm, "completion_cost", lambda completion_response=None, **kw: cost)
    return rec


def _llm_config(model: str):
    """Build a real LLMConfig the way Agent.compile() does."""
    return Agent("cfg", model=model, tools=[search]).compile().llm


def _seam_middleware(adapter: SeamAdapter) -> list:
    return adapter._model._middleware  # noqa: SLF001 - test introspection


# ===========================================================================
# 1. ON BY DEFAULT — a plain Agent() is governed (PII + metering + audit +
#    receipts ON), with allow-all allowlist and no budget cap (the documented
#    defaults).  Assert the default seam chain.
# ===========================================================================


class TestOnByDefault:
    def test_plain_agent_governance_defaults_are_on(self) -> None:
        agent = Agent("plain", model="anthropic/claude-sonnet-4-6", tools=[search])
        g = agent.governance
        # Safe defaults ON without the developer opting in.
        assert g.pii is True
        assert g.audit is True
        assert g.receipts is True
        # No cap / no policy by default (allow-all model layer; documented v1).
        assert g.budget is None
        assert g.policy is None
        assert g.approval_required is False

    def test_plain_agent_default_chain_is_allowlist_pii_budget_metering(self) -> None:
        """The documented default seam chain for a plain Agent()."""
        agent = Agent("plain", model="anthropic/claude-sonnet-4-6", tools=[search])
        chain = default_model_middleware(agent.governance)
        assert [type(mw).__name__ for mw in chain] == [
            "ModelAllowlistMiddleware",
            "PiiRedactionMiddleware",
            "BudgetMiddleware",
            "MeteringMiddleware",
        ]
        # allow-all (no policy) + uncapped (no budget) — PII + metering active.
        assert isinstance(chain[0], ModelAllowlistMiddleware)
        assert chain[0]._allowed is None  # noqa: SLF001
        assert isinstance(chain[1], PiiRedactionMiddleware)
        assert isinstance(chain[2], BudgetMiddleware)
        assert chain[2]._budget is None  # noqa: SLF001
        assert isinstance(chain[3], MeteringMiddleware)

    def test_plain_agent_inprocess_seam_has_pii_and_metering_on(self) -> None:
        """The in-process adapter a plain Agent() runs through carries PII +
        metering (the on-by-default proof, end of the threading)."""
        agent = Agent("plain", model="anthropic/claude-sonnet-4-6", tools=[search])
        adapter = get_adapter(agent.compile().llm, agent.governance)
        mws = _seam_middleware(adapter)
        assert any(isinstance(m, PiiRedactionMiddleware) for m in mws)
        assert any(isinstance(m, MeteringMiddleware) for m in mws)


# ===========================================================================
# 2. THE KEY GAP CLOSED — governance is threaded into the in-process seam.
# ===========================================================================


class TestInProcessSeamThreading:
    def test_get_adapter_threads_budget_into_seam(self) -> None:
        gov = normalize_governance(budget=Budget(cost_usd=0.25))
        adapter = get_adapter(_llm_config("anthropic/claude-sonnet-4-6"), gov)
        budget_mw = next(m for m in _seam_middleware(adapter) if isinstance(m, BudgetMiddleware))
        assert budget_mw._budget == Budget(cost_usd=0.25)  # noqa: SLF001

    def test_get_adapter_threads_policy_allowlist_into_seam(self) -> None:
        gov = normalize_governance(policy="strict")
        adapter = get_adapter(_llm_config("anthropic/claude-sonnet-4-6"), gov)
        allow_mw = _seam_middleware(adapter)[0]
        assert isinstance(allow_mw, ModelAllowlistMiddleware)
        assert allow_mw._allowed == {"anthropic"}  # noqa: SLF001

    def test_get_adapter_omits_pii_when_pii_false(self) -> None:
        gov = normalize_governance(pii=False)
        adapter = get_adapter(_llm_config("anthropic/claude-sonnet-4-6"), gov)
        assert not any(isinstance(m, PiiRedactionMiddleware) for m in _seam_middleware(adapter))

    def test_get_adapter_without_governance_is_allow_all_no_budget(self) -> None:
        """Back-compat: no governance -> Track-1 default (allow-all, no budget)."""
        adapter = get_adapter(_llm_config("anthropic/claude-sonnet-4-6"))
        mws = _seam_middleware(adapter)
        assert mws[0]._allowed is None  # noqa: SLF001
        budget_mw = next(m for m in mws if isinstance(m, BudgetMiddleware))
        assert budget_mw._budget is None  # noqa: SLF001


# ===========================================================================
# 3. BUDGET PARITY — enforces on agent.run() AND carried in the durable IR.
# ===========================================================================


class TestBudgetParity:
    def test_inprocess_run_denies_over_budget(self, monkeypatch: pytest.MonkeyPatch) -> None:
        """agent.run() DENIES the over-budget model call (BudgetExceededError).

        react: call 1 proceeds (consumed $0.50), call 2's pre-provider check sees
        $0.50 >= $0.40 and denies BEFORE the provider — backend hit exactly once.
        """
        backend = _install_backend(monkeypatch, cost=0.5)
        agent = Agent("b", model="gpt-5.2", tools=[search], strategy="react", budget=0.4)
        with pytest.raises(BudgetExceededError):
            asyncio.run(agent.run("q"))
        assert backend.calls == 1  # the over-budget 2nd call never reached the provider

    def test_durable_ir_carries_cost_and_token_budget(self) -> None:
        agent = Agent(
            "b",
            model="gpt-5.2",
            tools=[search],
            budget=Budget(cost_usd=2.0, tokens=5000),
        )
        ir = compile_agent_to_ir(agent, "q")
        assert ir["cost_budget_usd"] == 2.0
        assert ir["token_budget"] == {"total_tokens": 5000}


# ===========================================================================
# 4. POLICY / ALLOWLIST PARITY — denies on agent.run() AND carried in the IR.
# ===========================================================================


class TestPolicyParity:
    def test_inprocess_run_denies_disallowed_model(self, monkeypatch: pytest.MonkeyPatch) -> None:
        """agent.run() with policy='strict' (Anthropic-only) DENIES an openai model
        before any provider call."""
        backend = _install_backend(monkeypatch)
        agent = Agent("p", model="gpt-5.2", tools=[search], strategy="react", policy="strict")
        with pytest.raises(ModelDeniedError) as exc:
            asyncio.run(agent.run("q"))
        assert exc.value.code == "model_not_allowed"
        assert backend.calls == 0  # denied at the seam, never reached the provider

    def test_inprocess_run_allows_listed_model(self, monkeypatch: pytest.MonkeyPatch) -> None:
        """The dual happy path: an allowed model is NOT denied."""
        backend = _install_backend(monkeypatch)
        agent = Agent(
            "p",
            model="anthropic/claude-sonnet-4-6",
            tools=[search],
            strategy="react",
            policy="strict",
        )
        result = asyncio.run(agent.run("q"))
        assert result.output  # ran to completion
        assert backend.calls >= 1

    def test_durable_ir_carries_policy_allowlist(self) -> None:
        agent = Agent(
            "p",
            model="anthropic/claude-sonnet-4-6",
            tools=[search],
            policy="strict",
        )
        ir = compile_agent_to_ir(agent, "q")
        assert ir["policy"]["model_allowlist"] == ["anthropic"]


# ===========================================================================
# 5. PII PARITY — redacts before the backend on agent.run() AND sets data_policy
#    in the durable IR.
# ===========================================================================


class TestPiiParity:
    def test_inprocess_run_redacts_pii_before_backend(self, monkeypatch: pytest.MonkeyPatch) -> None:
        backend = _install_backend(monkeypatch)
        agent = Agent("pii", model="anthropic/claude-sonnet-4-6", tools=[search], strategy="react")
        asyncio.run(agent.run("email alice@example.com and SSN 123-45-6789"))
        flat = json.dumps(backend.seen)
        assert backend.calls >= 1
        # The provider NEVER saw the raw PII.
        assert "alice@example.com" not in flat
        assert "123-45-6789" not in flat
        # It saw the typed redaction tokens instead.
        assert "[REDACTED:EMAIL]" in flat
        assert "[REDACTED:US_SSN]" in flat

    def test_inprocess_run_pii_false_passes_through(self, monkeypatch: pytest.MonkeyPatch) -> None:
        """pii=False is an explicit opt-out: the provider receives unredacted text."""
        backend = _install_backend(monkeypatch)
        agent = Agent(
            "nopii",
            model="anthropic/claude-sonnet-4-6",
            tools=[search],
            strategy="react",
            pii=False,
        )
        asyncio.run(agent.run("email alice@example.com"))
        flat = json.dumps(backend.seen)
        assert "alice@example.com" in flat  # not redacted when pii=False

    def test_durable_ir_sets_data_policy_when_pii_on(self) -> None:
        agent = Agent("pii", model="anthropic/claude-sonnet-4-6", tools=[search])  # pii on by default
        ir = compile_agent_to_ir(agent, "q")
        assert "data_policy" in ir
        assert "email" in ir["data_policy"]["pii_detectors"]
        assert "ssn" in ir["data_policy"]["pii_detectors"]

    def test_durable_ir_omits_data_policy_when_pii_off(self) -> None:
        agent = Agent("nopii", model="anthropic/claude-sonnet-4-6", tools=[search], pii=False)
        ir = compile_agent_to_ir(agent, "q")
        assert "data_policy" not in ir


# ===========================================================================
# 6. APPROVAL_REQUIRED PARITY — in-process fails LOUD (UserWarning), durable IR
#    carries require_approval_for.  It NEVER silently no-ops.
# ===========================================================================


class TestApprovalParity:
    def test_inprocess_run_warns_never_silent(self, monkeypatch: pytest.MonkeyPatch) -> None:
        """approval_required on agent.run() emits a UserWarning (fail-LOUD).

        The in-process strategy runner cannot enforce tool-level approval gates
        (F-t3-inprocess-approval); rather than silently no-op, it warns and points
        the developer at run_durable().
        """
        _install_backend(monkeypatch)
        agent = Agent(
            "a",
            model="anthropic/claude-sonnet-4-6",
            tools=[search],
            strategy="react",
            approval_required=["delete_*"],
        )
        with warnings.catch_warnings(record=True) as caught:
            warnings.simplefilter("always")
            asyncio.run(agent.run("q"))
        approval = [w for w in caught if issubclass(w.category, UserWarning) and "approval_required" in str(w.message)]
        assert len(approval) >= 1
        assert "run_durable" in str(approval[0].message)

    def test_no_approval_no_warning(self, monkeypatch: pytest.MonkeyPatch) -> None:
        """The dual: a plain agent does NOT emit a spurious approval warning."""
        _install_backend(monkeypatch)
        agent = Agent("a", model="anthropic/claude-sonnet-4-6", tools=[search], strategy="react")
        with warnings.catch_warnings(record=True) as caught:
            warnings.simplefilter("always")
            asyncio.run(agent.run("q"))
        approval = [w for w in caught if "approval" in str(w.message).lower()]
        assert approval == []

    def test_durable_ir_carries_require_approval_for_list(self) -> None:
        agent = Agent(
            "a",
            model="anthropic/claude-sonnet-4-6",
            tools=[search],
            approval_required=["delete_*", "send_*"],
        )
        ir = compile_agent_to_ir(agent, "q")
        assert ir["policy"]["require_approval_for"] == ["delete_*", "send_*"]

    def test_durable_ir_approval_true_is_wildcard(self) -> None:
        agent = Agent(
            "a",
            model="anthropic/claude-sonnet-4-6",
            tools=[search],
            approval_required=True,
        )
        ir = compile_agent_to_ir(agent, "q")
        assert ir["policy"]["require_approval_for"] == ["*"]


# ===========================================================================
# 7. CONSOLIDATED ADVERSARIAL DUALS — the fail-closed proof (second-review).
# ===========================================================================


class TestAdversarialDuals:
    # -- budget-exceed -> DENIED (not warned), backend not hit ----------------
    def test_dual_budget_exceed_is_denied_not_warned(self, monkeypatch: pytest.MonkeyPatch) -> None:
        backend = _install_backend(monkeypatch, cost=1.0)
        agent = Agent("d", model="gpt-5.2", tools=[search], strategy="react", budget=0.5)
        with warnings.catch_warnings(record=True) as caught:
            warnings.simplefilter("always")
            with pytest.raises(BudgetExceededError):
                asyncio.run(agent.run("q"))
        # It is a hard DENY, not a soft warning-and-continue.
        assert backend.calls == 1
        budget_warns = [w for w in caught if "budget" in str(w.message).lower()]
        assert budget_warns == []

    # -- allowlist miss -> DENIED, backend not hit ----------------------------
    def test_dual_allowlist_miss_is_denied(self, monkeypatch: pytest.MonkeyPatch) -> None:
        backend = _install_backend(monkeypatch)
        agent = Agent("d", model="openai/gpt-4o", tools=[search], strategy="react", policy="strict")
        with pytest.raises(ModelDeniedError):
            asyncio.run(agent.run("q"))
        assert backend.calls == 0

    # -- PII (email / SSN / card) -> NOT leaked to the provider ----------------
    def test_dual_pii_not_leaked_to_provider(self, monkeypatch: pytest.MonkeyPatch) -> None:
        backend = _install_backend(monkeypatch)
        agent = Agent("d", model="anthropic/claude-sonnet-4-6", tools=[search], strategy="react")
        asyncio.run(agent.run("card 4111 1111 1111 1111 mail to bob@corp.io ssn 987-65-4321"))
        flat = json.dumps(backend.seen)
        assert backend.calls >= 1
        assert "bob@corp.io" not in flat
        assert "987-65-4321" not in flat
        assert "4111 1111 1111 1111" not in flat

    # -- a budget of 0 -> fail-LOUD at construction (fail-closed, never allow) --
    def test_dual_zero_budget_fails_loud(self) -> None:
        """A zero budget can never silently allow: it is rejected at config time."""
        with pytest.raises(ValueError, match="must be positive"):
            Budget(cost_usd=0)
        with pytest.raises(ValueError, match="must be positive"):
            Agent("z", model="anthropic/claude-sonnet-4-6", tools=[search], budget=0)

    # -- an empty allowlist SET -> deny-all (the primitive is fail-closed) -----
    def test_dual_empty_allowlist_set_denies_all(self) -> None:
        """``ModelAllowlistMiddleware(set())`` (a genuinely empty allowlist) DENIES
        every model — the enforcement primitive does not fail open."""
        mw = ModelAllowlistMiddleware(set())
        req = ModelRequest(ref=parse_model_ref("anthropic/claude-sonnet-4-6"), messages=[])
        with pytest.raises(ModelDeniedError):
            asyncio.run(mw.before(req))

    def test_documented_open_default_is_not_a_silent_bypass(self) -> None:
        """The resolver maps None / "open" / an empty model_allowlist list -> allow-all
        (the documented v1 "open" default).  Asserted here so it is a DOCUMENTED
        choice, never a silent surprise: an empty LIST means open; an empty SET
        (above) means deny.  A restrictive non-empty allowlist denies the outsider.
        """
        assert resolve_policy_allowlist(None) is None
        assert resolve_policy_allowlist("open") is None
        assert resolve_policy_allowlist({"model_allowlist": []}) is None
        # A real, non-empty allowlist is fail-closed for an outsider.
        assert resolve_policy_allowlist({"model_allowlist": ["anthropic"]}) == {"anthropic"}

    # -- audit tamper -> verification fails (Python bundle + Rust chain) -------
    def test_dual_audit_tamper_detected_python(self) -> None:
        """A tampered signed audit bundle fails ed25519 verification (the cloud
        export path).  Executable re-assert of "audit tamper -> verify fails"."""
        pytest.importorskip("cryptography")
        from cryptography.hazmat.primitives import serialization
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

        from jamjet.cloud.audit_verify import verify_package

        bundle = b'{"schema_version":"1.0","payload":"governed-action"}\n'
        sk = Ed25519PrivateKey.generate()
        sig = sk.sign(hashlib.sha256(bundle).digest())
        pk_bytes = sk.public_key().public_bytes(
            encoding=serialization.Encoding.Raw,
            format=serialization.PublicFormat.Raw,
        )
        assert verify_package(bundle, sig, pk_bytes).ok is True
        tampered = bundle.replace(b"governed-action", b"forged-action")
        assert verify_package(tampered, sig, pk_bytes).ok is False

    def test_dual_audit_tamper_dual_present_in_rust(self) -> None:
        """Guard the T3-4 Rust signed-chain tamper dual so the cross-language
        reference can't silently rot: tampering a sealed entry breaks
        ``verify_chain`` (HashMismatch)."""
        rust = _REPO_ROOT / "runtime" / "api" / "tests" / "audit_emit.rs"
        if not rust.exists():
            pytest.skip(f"runtime workspace not present at {rust}")
        text = rust.read_text()
        assert "verify_chain(&tampered" in text
        assert "HashMismatch" in text


# ===========================================================================
# 8. NO KNOB SILENTLY NO-OPS — a single matrix that, for every knob, asserts it
#    either enforces on agent.run() or fails LOUD.  This is the M1-moat claim.
# ===========================================================================


class TestNoKnobSilentlyNoOps:
    def test_budget_does_not_silently_noop_inprocess(self, monkeypatch: pytest.MonkeyPatch) -> None:
        backend = _install_backend(monkeypatch, cost=0.9)
        agent = Agent("n", model="gpt-5.2", tools=[search], strategy="react", budget=0.5)
        with pytest.raises(BudgetExceededError):
            asyncio.run(agent.run("q"))
        assert backend.calls == 1

    def test_policy_does_not_silently_noop_inprocess(self, monkeypatch: pytest.MonkeyPatch) -> None:
        _install_backend(monkeypatch)
        agent = Agent("n", model="gpt-5.2", tools=[search], strategy="react", policy="strict")
        with pytest.raises(ModelDeniedError):
            asyncio.run(agent.run("q"))

    def test_approval_does_not_silently_noop_inprocess(self, monkeypatch: pytest.MonkeyPatch) -> None:
        _install_backend(monkeypatch)
        agent = Agent(
            "n",
            model="anthropic/claude-sonnet-4-6",
            tools=[search],
            strategy="react",
            approval_required=True,
        )
        with warnings.catch_warnings(record=True) as caught:
            warnings.simplefilter("always")
            asyncio.run(agent.run("q"))
        assert any("approval_required" in str(w.message) for w in caught)

    def test_every_knob_compiles_into_the_durable_ir(self) -> None:
        """One fully-governed agent -> every knob lands in the durable IR."""
        agent = Agent(
            "full",
            model="anthropic/claude-sonnet-4-6",
            tools=[search],
            budget=Budget(cost_usd=1.5, tokens=2000),
            policy="strict",
            approval_required=["delete_*"],
        )  # pii on by default
        ir = compile_agent_to_ir(agent, "q")
        assert ir["cost_budget_usd"] == 1.5
        assert ir["token_budget"] == {"total_tokens": 2000}
        assert ir["policy"]["model_allowlist"] == ["anthropic"]
        assert ir["policy"]["require_approval_for"] == ["delete_*"]
        assert "data_policy" in ir
