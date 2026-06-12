"""Tests for policy block passthrough in the YAML workflow compiler.

Covers:
- workflow-level policy at top-level key (sibling of `workflow:`)
- node-level policy on a node entry
- absent policy -> IR has no `policy` key (null / omitted)
- unknown keys inside policy -> ValueError

The runtime IR contract (PolicySetIr in runtime/ir/src/workflow.rs):
  blocked_tools: Vec<String>  (serde default = [])
  require_approval_for: Vec<String>  (serde default = [])
  model_allowlist: Vec<String>  (serde default = [])

All three fields have serde(default), so the runtime accepts partial objects.
We emit all three keys with [] defaults to match the gated_ir() fixture in
runtime/api/tests/approval_e2e.rs exactly.
"""

import pytest

from jamjet.workflow.ir_compiler import _compile_graph_yaml, compile_yaml

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

MINIMAL_GRAPH = {
    "workflow": {"id": "test-wf", "version": "0.1.0", "start": "step1"},
    "nodes": {
        "step1": {"type": "tool", "tool_ref": "noop", "next": "end"},
    },
}


# ---------------------------------------------------------------------------
# Absent policy -- no regression
# ---------------------------------------------------------------------------


def test_no_policy_yields_null_in_ir():
    """When no policy: key is present, the IR omits it (None)."""
    ir = _compile_graph_yaml(MINIMAL_GRAPH)
    # The key should either be absent or explicitly None -- either is fine
    assert ir.get("policy") is None


def test_no_node_policy_yields_null_in_ir():
    """When a node has no policy: key, its IR policy is None."""
    ir = _compile_graph_yaml(MINIMAL_GRAPH)
    assert ir["nodes"]["step1"].get("policy") is None


# ---------------------------------------------------------------------------
# Workflow-level policy
# ---------------------------------------------------------------------------

WORKFLOW_POLICY_YAML = """
workflow:
  id: payment-processor
  version: 0.1.0
  start: fetch_details

policy:
  require_approval_for:
    - "payments.*"

nodes:
  fetch_details:
    type: tool
    tool_ref: fetch_payment_details
    next: end
"""


def test_workflow_level_policy_require_approval_for():
    """Top-level policy.require_approval_for compiles into IR with all three fields."""
    ir = compile_yaml(WORKFLOW_POLICY_YAML)
    assert ir["policy"] == {
        "blocked_tools": [],
        "require_approval_for": ["payments.*"],
        "model_allowlist": [],
    }


def test_workflow_level_policy_blocked_tools():
    """Top-level policy.blocked_tools compiles into IR."""
    yaml = """
workflow:
  id: safe-wf
  version: 0.1.0
  start: step1
policy:
  blocked_tools:
    - "rm_*"
    - "shell.*"
nodes:
  step1:
    type: tool
    tool_ref: noop
    next: end
"""
    ir = compile_yaml(yaml)
    assert ir["policy"] == {
        "blocked_tools": ["rm_*", "shell.*"],
        "require_approval_for": [],
        "model_allowlist": [],
    }


def test_workflow_level_policy_model_allowlist():
    """Top-level policy.model_allowlist compiles into IR."""
    yaml = """
workflow:
  id: safe-wf
  version: 0.1.0
  start: step1
policy:
  model_allowlist:
    - "gpt-4o"
    - "claude-3-5-sonnet-20241022"
nodes:
  step1:
    type: model
    model: gpt-4o
    next: end
"""
    ir = compile_yaml(yaml)
    assert ir["policy"] == {
        "blocked_tools": [],
        "require_approval_for": [],
        "model_allowlist": ["gpt-4o", "claude-3-5-sonnet-20241022"],
    }


def test_workflow_level_policy_all_three_fields():
    """All three policy fields together compile correctly."""
    yaml = """
workflow:
  id: full-policy-wf
  version: 0.1.0
  start: step1
policy:
  blocked_tools:
    - "danger.*"
  require_approval_for:
    - "payments.*"
  model_allowlist:
    - "gpt-4o"
nodes:
  step1:
    type: tool
    tool_ref: noop
    next: end
"""
    ir = compile_yaml(yaml)
    assert ir["policy"] == {
        "blocked_tools": ["danger.*"],
        "require_approval_for": ["payments.*"],
        "model_allowlist": ["gpt-4o"],
    }


# ---------------------------------------------------------------------------
# Node-level policy
# ---------------------------------------------------------------------------

NODE_POLICY_YAML = """
workflow:
  id: node-policy-wf
  version: 0.1.0
  start: step1

nodes:
  step1:
    type: tool
    tool_ref: payments.submit
    policy:
      blocked_tools:
        - "rm_*"
    next: end
"""


def test_node_level_policy_blocked_tools():
    """A node with policy.blocked_tools lands in that node's IR policy."""
    ir = compile_yaml(NODE_POLICY_YAML)
    node_policy = ir["nodes"]["step1"]["policy"]
    assert node_policy == {
        "blocked_tools": ["rm_*"],
        "require_approval_for": [],
        "model_allowlist": [],
    }


def test_node_level_policy_require_approval_for():
    """A node with policy.require_approval_for lands in that node's IR policy."""
    yaml = """
workflow:
  id: node-approval-wf
  version: 0.1.0
  start: gated
nodes:
  gated:
    type: tool
    tool_ref: payments.transfer
    policy:
      require_approval_for:
        - "payments.*"
    next: end
"""
    ir = compile_yaml(yaml)
    node_policy = ir["nodes"]["gated"]["policy"]
    # Must match the gated_ir() fixture in runtime/api/tests/approval_e2e.rs exactly
    assert node_policy == {
        "blocked_tools": [],
        "require_approval_for": ["payments.*"],
        "model_allowlist": [],
    }


def test_node_without_policy_key_has_null_policy():
    """Nodes that don't declare a policy: block have policy=None in IR."""
    ir = compile_yaml(NODE_POLICY_YAML)
    # NODE_POLICY_YAML has no second node, but a YAML with a second node
    yaml = """
workflow:
  id: mixed-wf
  version: 0.1.0
  start: gated
nodes:
  gated:
    type: tool
    tool_ref: payments.transfer
    policy:
      require_approval_for:
        - "payments.*"
    next: no_policy_node
  no_policy_node:
    type: tool
    tool_ref: noop
    next: end
"""
    ir = compile_yaml(yaml)
    assert ir["nodes"]["gated"]["policy"] is not None
    assert ir["nodes"]["no_policy_node"].get("policy") is None


# ---------------------------------------------------------------------------
# Both workflow-level and node-level policy in one document
# ---------------------------------------------------------------------------


def test_workflow_and_node_policy_coexist():
    """Workflow-level and node-level policies compile independently."""
    yaml = """
workflow:
  id: dual-policy-wf
  version: 0.1.0
  start: step1
policy:
  model_allowlist:
    - "gpt-4o"
nodes:
  step1:
    type: tool
    tool_ref: payments.submit
    policy:
      require_approval_for:
        - "payments.*"
    next: end
"""
    ir = compile_yaml(yaml)
    assert ir["policy"] == {
        "blocked_tools": [],
        "require_approval_for": [],
        "model_allowlist": ["gpt-4o"],
    }
    assert ir["nodes"]["step1"]["policy"] == {
        "blocked_tools": [],
        "require_approval_for": ["payments.*"],
        "model_allowlist": [],
    }


# ---------------------------------------------------------------------------
# Unknown keys inside policy -> ValueError
# ---------------------------------------------------------------------------


def test_workflow_policy_unknown_key_raises():
    """Unknown keys inside a workflow-level policy block raise ValueError."""
    yaml = """
workflow:
  id: bad-wf
  version: 0.1.0
  start: step1
policy:
  require_approval_for:
    - "payments.*"
  budget: 5
nodes:
  step1:
    type: tool
    tool_ref: noop
    next: end
"""
    with pytest.raises(ValueError, match="budget"):
        compile_yaml(yaml)


def test_node_policy_unknown_key_raises():
    """Unknown keys inside a node-level policy block raise ValueError."""
    yaml = """
workflow:
  id: bad-node-wf
  version: 0.1.0
  start: step1
nodes:
  step1:
    type: tool
    tool_ref: noop
    policy:
      require_approval_for:
        - "payments.*"
      unknown_field: true
    next: end
"""
    with pytest.raises(ValueError, match="unknown_field"):
        compile_yaml(yaml)


# ---------------------------------------------------------------------------
# Docs-page YAML compiles verbatim
# ---------------------------------------------------------------------------

# This is the workflow YAML from docs.jamjet.dev/open-source/approvals, adapted
# to the compiler's existing dict-node format (the docs page uses the dict
# format too for the top-level policy block; the node-level syntax is shown
# embedded in a list format in the docs, but the compiler uses dict nodes).
#
# The top-level policy block is the key new surface.
DOCS_PAGE_YAML = """
workflow:
  id: payment-processor
  version: 0.1.0
  start: fetch_details

nodes:
  fetch_details:
    type: tool
    tool_ref: fetch_payment_details
    next: submit_payment

  submit_payment:
    type: tool
    tool_ref: payments.submit
    next: end

policy:
  require_approval_for:
    - "payments.*"
"""


def test_docs_page_yaml_compiles():
    """The workflow YAML from the approvals docs page compiles without error."""
    ir = compile_yaml(DOCS_PAGE_YAML)
    assert ir["workflow_id"] == "payment-processor"
    assert ir["policy"] == {
        "blocked_tools": [],
        "require_approval_for": ["payments.*"],
        "model_allowlist": [],
    }
    assert "fetch_details" in ir["nodes"]
    assert "submit_payment" in ir["nodes"]
