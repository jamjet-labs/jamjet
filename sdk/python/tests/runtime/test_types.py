from datetime import datetime

from jamjet.runtime import (
    LLMCallRecord,
    RuntimeEvent,
    RuntimeResult,
    Scope,
    StepRecord,
    ToolCallRecord,
)


def test_step_record_minimal():
    s = StepRecord(step_id="s1", input_hash="h", status="completed", output_json="42")
    assert s.status == "completed"


def test_runtime_event_kinds():
    e = RuntimeEvent(
        kind="step_start",
        workflow_id="w1",
        step_id="s1",
        timestamp=datetime.now(),
        payload={},
    )
    assert e.kind == "step_start"


def test_runtime_result():
    r = RuntimeResult(
        output="hi",
        execution_id="e1",
        duration_ms=1.0,
        steps=[],
        tool_calls=[],
        llm_calls=[],
    )
    assert r.output == "hi"


def test_tool_call_record():
    t = ToolCallRecord(tool="search", input={"q": "x"}, output="y", duration_us=10.0)
    assert t.tool == "search"


def test_llm_call_record():
    c = LLMCallRecord(
        provider="openai",
        model="gpt-4o",
        prompt_tokens=10,
        completion_tokens=5,
        duration_ms=100.0,
    )
    assert c.provider == "openai"


def test_scope_defaults():
    s = Scope()
    assert s.user_id == "default"
    assert s.org_id == "default"
