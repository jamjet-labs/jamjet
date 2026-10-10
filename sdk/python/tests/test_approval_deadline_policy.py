"""Approval deadlines in compiled policies.

The runtime's PolicySetIr has ``approval_timeout_secs`` and
``escalate_after_secs`` (runtime/ir/src/workflow.rs). The YAML compiler used
to refuse them as unknown keys, and the Agent compiler dropped them, so a
deadline could only be set in hand-written IR or through ``PUT /tenants``.
Both compilers now carry them, and emit them only when set: the runtime binds
an approval to the node's serialized definition, so an absent setting must
not add keys.
"""

import pytest

from jamjet.agents.governance import GovernanceConfig
from jamjet.compiler.agent_ir import _compile_agent_policy_ir
from jamjet.workflow.ir_compiler import _compile_graph_yaml


def graph(workflow_policy=None, node_policy=None):
    g = {
        "workflow": {"id": "wf", "version": "0.1.0", "start": "pay"},
        "nodes": {"pay": {"type": "tool", "tool_ref": "payments.send", "next": "end"}},
    }
    if workflow_policy is not None:
        g["policy"] = workflow_policy
    if node_policy is not None:
        g["nodes"]["pay"]["policy"] = node_policy
    return g


def test_yaml_policies_carry_deadlines():
    ir = _compile_graph_yaml(
        graph(
            {"require_approval_for": ["*"], "approval_timeout_secs": 3600, "escalate_after_secs": 900},
            {"approval_timeout_secs": 600},
        )
    )
    assert ir["policy"]["approval_timeout_secs"] == 3600
    assert ir["policy"]["escalate_after_secs"] == 900
    assert ir["nodes"]["pay"]["policy"]["approval_timeout_secs"] == 600
    assert "escalate_after_secs" not in ir["nodes"]["pay"]["policy"]


def test_an_unset_deadline_adds_no_keys():
    ir = _compile_graph_yaml(graph({"require_approval_for": ["*"]}))
    assert ir["policy"] == {"blocked_tools": [], "require_approval_for": ["*"], "model_allowlist": []}


@pytest.mark.parametrize(
    "bad",
    [
        {"approval_timeout_secs": 0},
        {"approval_timeout_secs": -5},
        {"approval_timeout_secs": "600"},
        {"approval_timeout_secs": True},
        {"approval_timeout_secs": 1.5},
        {"approval_timeout_secs": 31_536_001},
        {"escalate_after_secs": 10},
        {"approval_timeout_secs": 60, "escalate_after_secs": 60},
        {"approval_timeout_secs": 60, "escalate_after_secs": 0},
    ],
)
def test_an_invalid_deadline_is_refused_at_compile_time(bad):
    with pytest.raises(ValueError):
        _compile_graph_yaml(graph(bad))


def test_agent_policy_carries_deadlines():
    gov = GovernanceConfig(
        policy={"require_approval_for": ["send_*"], "approval_timeout_secs": 3600, "escalate_after_secs": 900}
    )
    ir = _compile_agent_policy_ir(gov)
    assert ir["approval_timeout_secs"] == 3600
    assert ir["escalate_after_secs"] == 900
    plain = _compile_agent_policy_ir(GovernanceConfig(approval_required=True))
    assert "approval_timeout_secs" not in plain


def test_agent_policy_refuses_an_invalid_deadline():
    with pytest.raises(ValueError):
        _compile_agent_policy_ir(GovernanceConfig(policy={"approval_timeout_secs": 0}))
