"""Tests for GovernanceConfig, Budget, normalize_governance, and Agent governance knobs.

Run with:
    uv run python -m pytest tests -k governance -v

Covers T3-1: typed config + threading.  No enforcement happens here.
"""

from __future__ import annotations

import pytest

from jamjet.agents.governance import Budget, GovernanceConfig, normalize_governance

# ---------------------------------------------------------------------------
# Budget coercions
# ---------------------------------------------------------------------------


class TestNormalizeGovernanceBudget:
    """normalize_governance(budget=…) coercion matrix."""

    def test_float_becomes_cost_usd(self):
        cfg = normalize_governance(budget=0.50)
        assert cfg.budget == Budget(cost_usd=0.50)

    def test_int_becomes_cost_usd(self):
        cfg = normalize_governance(budget=2)
        assert cfg.budget == Budget(cost_usd=2.0)

    def test_budget_object_preserved(self):
        b = Budget(tokens=1000)
        cfg = normalize_governance(budget=b)
        assert cfg.budget is b

    def test_budget_object_with_both_fields(self):
        b = Budget(tokens=5000, cost_usd=3.0)
        cfg = normalize_governance(budget=b)
        assert cfg.budget == Budget(tokens=5000, cost_usd=3.0)

    def test_dict_with_cost_usd_and_tokens(self):
        cfg = normalize_governance(budget={"cost_usd": 1.0, "tokens": 500})
        assert cfg.budget == Budget(cost_usd=1.0, tokens=500)

    def test_dict_cost_usd_only(self):
        cfg = normalize_governance(budget={"cost_usd": 0.25})
        assert cfg.budget == Budget(cost_usd=0.25)

    def test_dict_tokens_only(self):
        cfg = normalize_governance(budget={"tokens": 10_000})
        assert cfg.budget == Budget(tokens=10_000)

    def test_none_stays_none(self):
        cfg = normalize_governance(budget=None)
        assert cfg.budget is None

    def test_unknown_dict_key_raises(self):
        with pytest.raises(ValueError, match="Unknown budget keys"):
            normalize_governance(budget={"max_usd": 1.0})

    def test_bad_type_raises(self):
        with pytest.raises(TypeError, match="budget must be"):
            normalize_governance(budget="$2.00")  # type: ignore[arg-type]

    def test_budget_zero_raises(self):
        with pytest.raises(ValueError, match="positive"):
            normalize_governance(budget=0.0)

    def test_budget_negative_raises(self):
        with pytest.raises(ValueError, match="positive"):
            normalize_governance(budget=-5.0)


# ---------------------------------------------------------------------------
# approval_required coercions
# ---------------------------------------------------------------------------


class TestNormalizeGovernanceApproval:
    """normalize_governance(approval_required=…) coercion matrix."""

    def test_true_stored(self):
        cfg = normalize_governance(approval_required=True)
        assert cfg.approval_required is True

    def test_false_stored(self):
        cfg = normalize_governance(approval_required=False)
        assert cfg.approval_required is False

    def test_list_stored(self):
        globs = ["delete_*", "send_*"]
        cfg = normalize_governance(approval_required=globs)
        assert cfg.approval_required == globs

    def test_empty_list_stored(self):
        cfg = normalize_governance(approval_required=[])
        assert cfg.approval_required == []

    def test_bad_list_entry_raises(self):
        with pytest.raises(TypeError, match="list entries must be strings"):
            normalize_governance(approval_required=[True])  # type: ignore[list-item]

    def test_bad_type_raises(self):
        with pytest.raises(TypeError, match="approval_required must be bool or list"):
            normalize_governance(approval_required="all")  # type: ignore[arg-type]


# ---------------------------------------------------------------------------
# GovernanceConfig defaults
# ---------------------------------------------------------------------------


class TestGovernanceConfigDefaults:
    """A bare GovernanceConfig() must have all safety defaults ON."""

    def test_default_pii_on(self):
        assert GovernanceConfig().pii is True

    def test_default_audit_on(self):
        assert GovernanceConfig().audit is True

    def test_default_receipts_on(self):
        assert GovernanceConfig().receipts is True

    def test_default_approval_required_false(self):
        assert GovernanceConfig().approval_required is False

    def test_default_policy_none(self):
        assert GovernanceConfig().policy is None

    def test_default_budget_none(self):
        assert GovernanceConfig().budget is None

    def test_frozen(self):
        cfg = GovernanceConfig()
        with pytest.raises((TypeError, AttributeError)):
            cfg.pii = False  # type: ignore[misc]


# ---------------------------------------------------------------------------
# normalize_governance defaults
# ---------------------------------------------------------------------------


class TestNormalizeGovernanceDefaults:
    """normalize_governance() with no args == GovernanceConfig() defaults."""

    def test_defaults_match_config_defaults(self):
        cfg = normalize_governance()
        assert cfg.pii is True
        assert cfg.audit is True
        assert cfg.receipts is True
        assert cfg.approval_required is False
        assert cfg.policy is None
        assert cfg.budget is None


# ---------------------------------------------------------------------------
# policy field
# ---------------------------------------------------------------------------


class TestNormalizeGovernancePolicy:
    def test_str_policy(self):
        cfg = normalize_governance(policy="strict")
        assert cfg.policy == "strict"

    def test_dict_policy(self):
        p = {"blocked_tools": ["rm_*"], "model_allowlist": ["claude-*"]}
        cfg = normalize_governance(policy=p)
        assert cfg.policy == p

    def test_none_policy(self):
        cfg = normalize_governance(policy=None)
        assert cfg.policy is None


# ---------------------------------------------------------------------------
# Agent integration — governance knobs thread through correctly
# ---------------------------------------------------------------------------


@pytest.fixture
def dummy_tool(tmp_path):
    """A minimal @tool function for constructing test Agents."""
    from jamjet.tools.decorators import tool

    @tool
    async def noop(x: str) -> str:
        """Does nothing."""
        return x

    return noop


class TestAgentGovernanceKnobs:
    """Agent(policy=, budget=, approval_required=).governance is correct."""

    def test_explicit_knobs(self, dummy_tool):
        from jamjet.agents.agent import Agent

        agent = Agent(
            "test-agent",
            model="claude-sonnet-4-6",
            tools=[dummy_tool],
            policy="strict",
            budget=0.25,
            approval_required=["delete_*"],
        )
        gov = agent.governance
        assert gov.policy == "strict"
        assert gov.budget == Budget(cost_usd=0.25)
        assert gov.approval_required == ["delete_*"]
        # defaults ON
        assert gov.pii is True
        assert gov.audit is True
        assert gov.receipts is True

    def test_default_agent_carries_all_default_governance(self, dummy_tool):
        from jamjet.agents.agent import Agent

        agent = Agent(
            "plain-agent",
            model="claude-sonnet-4-6",
            tools=[dummy_tool],
        )
        gov = agent.governance
        assert gov.pii is True
        assert gov.audit is True
        assert gov.receipts is True
        assert gov.approval_required is False
        assert gov.policy is None
        assert gov.budget is None

    def test_approval_required_true(self, dummy_tool):
        from jamjet.agents.agent import Agent

        agent = Agent(
            "gated-agent",
            model="claude-sonnet-4-6",
            tools=[dummy_tool],
            approval_required=True,
        )
        assert agent.governance.approval_required is True

    def test_approval_required_list(self, dummy_tool):
        from jamjet.agents.agent import Agent

        globs = ["send_*", "transfer_*"]
        agent = Agent(
            "gated-agent",
            model="claude-sonnet-4-6",
            tools=[dummy_tool],
            approval_required=globs,
        )
        assert agent.governance.approval_required == globs

    def test_pii_off(self, dummy_tool):
        from jamjet.agents.agent import Agent

        agent = Agent(
            "no-pii-agent",
            model="claude-sonnet-4-6",
            tools=[dummy_tool],
            pii=False,
        )
        assert agent.governance.pii is False

    def test_budget_object_accepted(self, dummy_tool):
        from jamjet.agents.agent import Agent

        b = Budget(tokens=5000, cost_usd=1.0)
        agent = Agent(
            "budgeted-agent",
            model="claude-sonnet-4-6",
            tools=[dummy_tool],
            budget=b,
        )
        assert agent.governance.budget == b

    def test_governance_is_governance_config(self, dummy_tool):
        from jamjet.agents.agent import Agent

        agent = Agent(
            "typed-agent",
            model="claude-sonnet-4-6",
            tools=[dummy_tool],
        )
        assert isinstance(agent.governance, GovernanceConfig)

    def test_max_cost_usd_folds_into_budget_when_budget_not_set(self, dummy_tool):
        """Non-default max_cost_usd is mirrored into governance.budget.cost_usd."""
        from jamjet.agents.agent import Agent

        agent = Agent(
            "legacy-budget-agent",
            model="claude-sonnet-4-6",
            tools=[dummy_tool],
            max_cost_usd=0.75,
        )
        assert agent.governance.budget == Budget(cost_usd=0.75)

    def test_default_max_cost_usd_does_not_create_budget(self, dummy_tool):
        """The sentinel default (1.0) must NOT create a governance budget."""
        from jamjet.agents.agent import Agent

        agent = Agent(
            "default-budget-agent",
            model="claude-sonnet-4-6",
            tools=[dummy_tool],
            # max_cost_usd defaults to 1.0 — should not be mirrored
        )
        assert agent.governance.budget is None

    def test_explicit_budget_wins_over_max_cost_usd(self, dummy_tool):
        """When budget= is explicit it takes precedence; max_cost_usd is ignored."""
        from jamjet.agents.agent import Agent

        agent = Agent(
            "dual-budget-agent",
            model="claude-sonnet-4-6",
            tools=[dummy_tool],
            max_cost_usd=2.0,
            budget=0.50,
        )
        # Explicit budget wins
        assert agent.governance.budget == Budget(cost_usd=0.50)


# ---------------------------------------------------------------------------
# Budget dataclass constraints
# ---------------------------------------------------------------------------


class TestBudgetConstraints:
    def test_both_fields_none_is_valid(self):
        b = Budget()
        assert b.tokens is None
        assert b.cost_usd is None

    def test_negative_tokens_raises(self):
        with pytest.raises(ValueError, match="positive"):
            Budget(tokens=-1)

    def test_zero_cost_usd_raises(self):
        with pytest.raises(ValueError, match="positive"):
            Budget(cost_usd=0.0)

    def test_frozen_budget(self):
        b = Budget(cost_usd=1.0)
        with pytest.raises((TypeError, AttributeError)):
            b.cost_usd = 2.0  # type: ignore[misc]
