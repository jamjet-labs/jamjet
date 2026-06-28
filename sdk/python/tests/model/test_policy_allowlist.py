"""T3-6: policy-derived model allowlist + approval_required in-process parity.

Tests cover:
1. Dict policy with ``model_allowlist``   -> allowed model passes; disallowed denied.
2. ``policy=None``                         -> all models allowed (rest of governance on).
3. String ``"strict"``                     -> resolved allowlist (Anthropic only);
                                             disallowed model (e.g. openai) is DENIED.
4. String ``"open"``                       -> explicit allow-all alias.
5. Unknown string policy                   -> ``ValueError`` raised (never silently allows).
6. ``approval_required`` on in-process     -> ``UserWarning`` emitted; never silently no-ops.
7. Adversarial: allowlist denial path      -> ``ModelDeniedError`` raised *before* provider.
8. default_model_middleware wiring         -> policy resolves at chain-build time.

Run:
    uv run python -m pytest tests/model/test_policy_allowlist.py -v
"""

from __future__ import annotations

import warnings

import pytest

from jamjet.agents.governance import normalize_governance
from jamjet.model.defaults import default_model_middleware
from jamjet.model.middleware import ModelAllowlistMiddleware, ModelDeniedError
from jamjet.model.policy_resolver import (
    BUILT_IN_POLICIES,
    resolve_named_policy,
    resolve_policy_allowlist,
)
from jamjet.model.types import ModelRequest, parse_model_ref

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _req(model: str) -> ModelRequest:
    return ModelRequest(ref=parse_model_ref(model), messages=[{"role": "user", "content": "hi"}])


# ---------------------------------------------------------------------------
# 1. Dict policy with model_allowlist
# ---------------------------------------------------------------------------


class TestDictPolicyAllowlist:
    """policy=dict with model_allowlist-> seam denies unlisted models."""

    def test_allowed_model_passes(self):
        """A model in the dict allowlist is not denied."""
        allowlist = resolve_policy_allowlist({"model_allowlist": ["anthropic/claude-sonnet-4-6"]})
        assert allowlist == {"anthropic/claude-sonnet-4-6"}

    def test_allowed_by_provider_passes(self):
        allowlist = resolve_policy_allowlist({"model_allowlist": ["anthropic"]})
        assert "anthropic" in allowlist

    def test_dict_no_model_allowlist_is_allow_all(self):
        """Dict without model_allowlist key -> allow-all (None)."""
        allowlist = resolve_policy_allowlist({"blocked_tools": ["rm_*"]})
        assert allowlist is None

    def test_dict_empty_model_allowlist_is_allow_all(self):
        """Empty model_allowlist -> allow-all (None)."""
        allowlist = resolve_policy_allowlist({"model_allowlist": []})
        assert allowlist is None

    @pytest.mark.asyncio
    async def test_disallowed_model_raises_before_provider(self):
        """A model call whose provider/model is not in the allowlist raises ModelDeniedError."""
        gov = normalize_governance(policy={"model_allowlist": ["anthropic/claude-sonnet-4-6"]})
        chain = default_model_middleware(governance=gov)
        allowlist_mw = chain[0]
        assert isinstance(allowlist_mw, ModelAllowlistMiddleware)
        # openai/gpt-4o is NOT in the allowlist
        with pytest.raises(ModelDeniedError) as exc:
            await allowlist_mw.before(_req("openai/gpt-4o"))
        assert exc.value.code == "model_not_allowed"

    @pytest.mark.asyncio
    async def test_allowed_model_does_not_raise(self):
        """A model call whose model string IS in the allowlist proceeds."""
        gov = normalize_governance(policy={"model_allowlist": ["anthropic/claude-sonnet-4-6"]})
        chain = default_model_middleware(governance=gov)
        allowlist_mw = chain[0]
        assert isinstance(allowlist_mw, ModelAllowlistMiddleware)
        req = _req("anthropic/claude-sonnet-4-6")
        result = await allowlist_mw.before(req)
        assert result is req  # passed through unchanged


# ---------------------------------------------------------------------------
# 2. policy=None -> allow-all (rest of governance still on)
# ---------------------------------------------------------------------------


class TestNoPolicyAllowAll:
    """policy=None -> allow-all; audit/PII/budget still active."""

    def test_none_policy_returns_none_allowlist(self):
        allowlist = resolve_policy_allowlist(None)
        assert allowlist is None

    def test_no_policy_middleware_allowlist_is_none(self):
        chain = default_model_middleware(governance=None)
        mw = chain[0]
        assert isinstance(mw, ModelAllowlistMiddleware)
        assert mw._allowed is None  # allow-all

    def test_no_policy_governance_allowlist_is_none(self):
        """Explicit governance with no policy also keeps allow-all."""
        gov = normalize_governance(policy=None)
        chain = default_model_middleware(governance=gov)
        mw = chain[0]
        assert mw._allowed is None


# ---------------------------------------------------------------------------
# 3. String "strict" -> Anthropic-only
# ---------------------------------------------------------------------------


class TestStrictNamedPolicy:
    """policy="strict" resolves to the Anthropic-only allowlist."""

    def test_strict_resolves_to_dict(self):
        rules = resolve_named_policy("strict")
        assert "model_allowlist" in rules
        assert "anthropic" in rules["model_allowlist"]

    def test_strict_allowlist_contains_anthropic(self):
        allowlist = resolve_policy_allowlist("strict")
        assert allowlist is not None
        assert "anthropic" in allowlist

    def test_strict_allows_anthropic_provider(self):
        """Any anthropic/ model passes the strict allowlist."""
        allowlist = resolve_policy_allowlist("strict")
        assert allowlist is not None
        # provider check: "anthropic" in allowed -> anthropic/any-model passes
        assert "anthropic" in allowlist

    @pytest.mark.asyncio
    async def test_strict_allows_anthropic_model_call(self):
        gov = normalize_governance(policy="strict")
        chain = default_model_middleware(governance=gov)
        mw = chain[0]
        req = _req("anthropic/claude-sonnet-4-6")
        result = await mw.before(req)
        assert result is req

    @pytest.mark.asyncio
    async def test_strict_denies_openai_model(self):
        """strict policy: openai model calls are DENIED (adversarial dual)."""
        gov = normalize_governance(policy="strict")
        chain = default_model_middleware(governance=gov)
        mw = chain[0]
        with pytest.raises(ModelDeniedError) as exc:
            await mw.before(_req("openai/gpt-4o"))
        assert exc.value.code == "model_not_allowed"

    @pytest.mark.asyncio
    async def test_strict_denies_non_anthropic_provider(self):
        """Any non-anthropic provider is denied under strict policy."""
        gov = normalize_governance(policy="strict")
        chain = default_model_middleware(governance=gov)
        mw = chain[0]
        with pytest.raises(ModelDeniedError):
            await mw.before(_req("openai/gpt-3.5-turbo"))


# ---------------------------------------------------------------------------
# 4. String "open" -> allow-all
# ---------------------------------------------------------------------------


class TestOpenNamedPolicy:
    def test_open_allowlist_is_none(self):
        """open policy is an explicit allow-all alias."""
        allowlist = resolve_policy_allowlist("open")
        assert allowlist is None

    @pytest.mark.asyncio
    async def test_open_allows_any_model(self):
        gov = normalize_governance(policy="open")
        chain = default_model_middleware(governance=gov)
        mw = chain[0]
        req = _req("openai/gpt-4o")
        result = await mw.before(req)
        assert result is req


# ---------------------------------------------------------------------------
# 5. Unknown string policy -> ValueError (fail LOUD, never silent-allow)
# ---------------------------------------------------------------------------


class TestUnknownNamedPolicyFails:
    """An unknown policy name is a hard error, not a silent allow-all."""

    def test_unknown_policy_raises_value_error(self):
        with pytest.raises(ValueError, match="Unknown named policy"):
            resolve_named_policy("nonexistent-policy-xyz")

    def test_unknown_policy_raises_in_resolver(self):
        with pytest.raises(ValueError, match="Unknown named policy"):
            resolve_policy_allowlist("typo-strict")

    def test_unknown_policy_message_names_known_policies(self):
        """Error message lists the built-in policies so the user can fix the typo."""
        with pytest.raises(ValueError) as exc:
            resolve_named_policy("typo")
        msg = str(exc.value)
        for name in BUILT_IN_POLICIES:
            assert name in msg

    def test_unknown_policy_in_middleware_chain_raises(self):
        """Building the middleware chain with an unknown policy raises — never allow-all."""
        gov = normalize_governance(policy="allow-everything-please")
        with pytest.raises(ValueError, match="Unknown named policy"):
            default_model_middleware(governance=gov)


# ---------------------------------------------------------------------------
# 6. approval_required on the in-process path -> UserWarning (never silent no-op)
# ---------------------------------------------------------------------------


class TestApprovalRequiredInProcessWarning:
    """approval_required must never silently no-op on agent.run()."""

    @pytest.fixture
    def dummy_tool(self):
        from jamjet.tools.decorators import tool

        @tool
        async def noop(x: str) -> str:
            """Does nothing."""
            return x

        return noop

    def test_approval_required_true_warns_on_run(self, dummy_tool):
        """approval_required=True on agent.run() emits a UserWarning."""
        import asyncio
        from unittest.mock import AsyncMock, patch

        from jamjet.agents.agent import Agent
        from jamjet.runtime.local import LocalRuntime

        agent = Agent(
            "gated",
            model="anthropic/claude-sonnet-4-6",
            tools=[dummy_tool],
            approval_required=True,
        )

        # Mock LocalRuntime.execute so we don't need a real model.
        fake_result = AsyncMock(
            output="ok",
            tool_calls=[],
            duration_ms=1,
        )

        async def _run():
            with patch.object(LocalRuntime, "execute", return_value=fake_result):
                with warnings.catch_warnings(record=True) as w:
                    warnings.simplefilter("always")
                    await agent.run("test prompt")
            return w

        caught = asyncio.run(_run())
        user_warnings = [x for x in caught if issubclass(x.category, UserWarning)]
        assert len(user_warnings) >= 1
        msg = str(user_warnings[0].message)
        assert "approval_required" in msg
        assert "in-process" in msg.lower() or "run_durable" in msg

    def test_approval_required_list_warns_on_run(self, dummy_tool):
        """approval_required=[...] on agent.run() emits a UserWarning."""
        import asyncio
        from unittest.mock import AsyncMock, patch

        from jamjet.agents.agent import Agent
        from jamjet.runtime.local import LocalRuntime

        agent = Agent(
            "gated",
            model="anthropic/claude-sonnet-4-6",
            tools=[dummy_tool],
            approval_required=["delete_*"],
        )

        fake_result = AsyncMock(
            output="ok",
            tool_calls=[],
            duration_ms=1,
        )

        async def _run():
            with patch.object(LocalRuntime, "execute", return_value=fake_result):
                with warnings.catch_warnings(record=True) as w:
                    warnings.simplefilter("always")
                    await agent.run("test")
            return w

        caught = asyncio.run(_run())
        user_warnings = [x for x in caught if issubclass(x.category, UserWarning)]
        assert len(user_warnings) >= 1

    def test_approval_required_false_does_not_warn(self, dummy_tool):
        """approval_required=False (default) -> NO warning on agent.run()."""
        import asyncio
        from unittest.mock import AsyncMock, patch

        from jamjet.agents.agent import Agent
        from jamjet.runtime.local import LocalRuntime

        agent = Agent(
            "plain",
            model="anthropic/claude-sonnet-4-6",
            tools=[dummy_tool],
        )

        fake_result = AsyncMock(
            output="ok",
            tool_calls=[],
            duration_ms=1,
        )

        async def _run():
            with patch.object(LocalRuntime, "execute", return_value=fake_result):
                with warnings.catch_warnings(record=True) as w:
                    warnings.simplefilter("always")
                    await agent.run("test")
            return w

        caught = asyncio.run(_run())
        user_warnings = [x for x in caught if issubclass(x.category, UserWarning)]
        # No approval_required warnings
        approval_warns = [x for x in user_warnings if "approval" in str(x.message).lower()]
        assert len(approval_warns) == 0


# ---------------------------------------------------------------------------
# 7. IR compiler: string policy resolves to real model_allowlist in the IR
# ---------------------------------------------------------------------------


class TestStringPolicyInIr:
    """policy="strict" produces a real model_allowlist in the compiled IR (not empty)."""

    @pytest.fixture
    def weather_tool(self):
        from jamjet.tools.decorators import tool

        @tool
        async def get_weather(city: str) -> str:
            """Return weather."""
            return f"sunny in {city}"

        return get_weather

    def test_strict_policy_emits_model_allowlist_in_ir(self, weather_tool):
        from jamjet.agents.agent import Agent
        from jamjet.compiler.agent_ir import compile_agent_to_ir

        agent = Agent(
            "governed",
            model="anthropic/claude-sonnet-4-6",
            tools=[weather_tool],
            policy="strict",
        )
        ir = compile_agent_to_ir(agent, "hello")
        policy = ir.get("policy")
        assert policy is not None
        # "strict" -> model_allowlist: ["anthropic"] — not an empty list
        assert policy["model_allowlist"] == ["anthropic"]

    def test_open_policy_does_not_raise_in_ir(self, weather_tool):
        """policy="open" compiles without error (it resolves to empty rules -> no IR policy block)."""
        from jamjet.agents.agent import Agent
        from jamjet.compiler.agent_ir import compile_agent_to_ir

        agent = Agent(
            "governed",
            model="anthropic/claude-sonnet-4-6",
            tools=[weather_tool],
            policy="open",
        )
        # "open" resolves to empty blocked/approval/allowlist -> no policy block emitted.
        # The important invariant: it does NOT raise, and doesn't silently allow-all
        # without expressing intent.
        compile_agent_to_ir(agent, "hello")  # must not raise

    def test_unknown_string_policy_raises_in_ir_compiler(self, weather_tool):
        """Compiling an Agent with an unknown string policy raises ValueError."""
        from jamjet.agents.agent import Agent
        from jamjet.compiler.agent_ir import compile_agent_to_ir

        agent = Agent(
            "governed",
            model="anthropic/claude-sonnet-4-6",
            tools=[weather_tool],
            policy="nonexistent-policy",
        )
        with pytest.raises(ValueError, match="Unknown named policy"):
            compile_agent_to_ir(agent, "hello")


# ---------------------------------------------------------------------------
# 8. defaults.py: existing tests still pass (non-regression)
# ---------------------------------------------------------------------------


class TestDefaultsNonRegression:
    """Ensure T3-6 wiring doesn't regress the T3-1..4 defaults test invariants."""

    def test_no_governance_allowlist_is_none(self):
        """No governance -> allow-all (None allowlist) — the Track 1 default."""
        chain = default_model_middleware()
        mw = chain[0]
        assert isinstance(mw, ModelAllowlistMiddleware)
        assert mw._allowed is None

    def test_none_policy_keeps_allow_all(self):
        gov = normalize_governance(policy=None)
        chain = default_model_middleware(governance=gov)
        assert chain[0]._allowed is None

    def test_dict_policy_without_allowlist_keeps_allow_all(self):
        """Dict policy with only blocked_tools -> model allowlist stays None."""
        gov = normalize_governance(policy={"blocked_tools": ["rm_*"]})
        chain = default_model_middleware(governance=gov)
        assert chain[0]._allowed is None
