from jamjet.runtime.local.replay import compute_input_hash, derive_step_id


def test_step_id_deterministic():
    a = derive_step_id(parent_step_id=None, call_site="agent.plan", invocation_index=0)
    b = derive_step_id(parent_step_id=None, call_site="agent.plan", invocation_index=0)
    assert a == b


def test_step_id_changes_with_invocation_index():
    a = derive_step_id(parent_step_id=None, call_site="agent.plan", invocation_index=0)
    b = derive_step_id(parent_step_id=None, call_site="agent.plan", invocation_index=1)
    assert a != b


def test_step_id_changes_with_parent():
    a = derive_step_id(parent_step_id="root", call_site="x", invocation_index=0)
    b = derive_step_id(parent_step_id=None, call_site="x", invocation_index=0)
    assert a != b


def test_input_hash_stable_across_dict_order():
    h1 = compute_input_hash({"a": 1, "b": 2})
    h2 = compute_input_hash({"b": 2, "a": 1})
    assert h1 == h2


def test_input_hash_changes_with_value():
    assert compute_input_hash({"a": 1}) != compute_input_hash({"a": 2})
