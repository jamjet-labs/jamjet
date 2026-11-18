import asyncio
import json
from pathlib import Path

import pytest

from jamjet.model.scripted import ScriptedBackend, ScriptedModelError
from jamjet.model.types import ModelRequest, parse_model_ref

SCRIPT = Path(__file__).parent.parent / "fixtures" / "scripts" / "venue.yaml"
TOOLS = [
    {"type": "function", "function": {"name": n, "parameters": {"type": "object"}}}
    for n in ("search_venues", "check_dates", "pay_deposit")
]


def req(messages, tools=TOOLS):
    return ModelRequest(ref=parse_model_ref(f"scripted/{SCRIPT}"), messages=messages, tools=tools)


def run(r):
    return asyncio.run(ScriptedBackend().complete(r))


def test_first_turn_calls_the_first_tool():
    resp = run(req([{"role": "user", "content": "Book a venue in Lisbon for 40."}]))
    call = resp.message.tool_calls[0]
    assert (call.id, call.function.name) == ("call_0_0", "search_venues")
    assert json.loads(call.function.arguments) == {"city": "Lisbon", "guests": 40}
    assert resp.cost_usd == 0.0
    assert resp.message.finish_reason == "tool_calls"


def test_second_turn_needs_the_first_result():
    first = run(req([{"role": "user", "content": "go"}]))
    assistant = {
        "role": "assistant",
        "content": "",
        "tool_calls": [
            {"id": c.id, "type": "function", "function": {"name": c.function.name, "arguments": c.function.arguments}}
            for c in first.message.tool_calls
        ],
    }
    with pytest.raises(ScriptedModelError, match="no result for call_0_0"):
        run(req([{"role": "user", "content": "go"}, assistant]))
    ok = run(
        req(
            [
                {"role": "user", "content": "go"},
                assistant,
                {"role": "tool", "tool_call_id": "call_0_0", "content": "[]"},
            ]
        )
    )
    assert ok.message.tool_calls[0].function.name == "check_dates"


def test_past_the_end_says_so():
    msgs = [{"role": "user", "content": "go"}] + [{"role": "assistant", "content": "x"} for _ in range(3)]
    with pytest.raises(ScriptedModelError, match="the script has 3 turns"):
        run(req(msgs, tools=[]))


def test_unoffered_tool_is_refused():
    with pytest.raises(ScriptedModelError, match="search_venues"):
        run(req([{"role": "user", "content": "go"}], tools=TOOLS[1:]))


def test_runs_without_litellm(monkeypatch):
    import sys

    monkeypatch.setitem(sys.modules, "litellm", None)
    assert run(req([{"role": "user", "content": "go"}])).message.tool_calls


def test_missing_script_names_the_path():
    r = ModelRequest(ref=parse_model_ref("scripted/nope/missing.yaml"), messages=[], tools=None)
    with pytest.raises(ScriptedModelError, match="missing.yaml"):
        asyncio.run(ScriptedBackend().complete(r))


def test_final_turn_is_text_and_uses_script_usage():
    msgs = [{"role": "user", "content": "go"}]
    for i, name in enumerate(["search_venues", "check_dates"]):
        msgs.append(
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {"id": f"call_{i}_0", "type": "function", "function": {"name": name, "arguments": "{}"}}
                ],
            }
        )
        msgs.append({"role": "tool", "tool_call_id": f"call_{i}_0", "content": "ok"})
    resp = run(req(msgs))
    assert resp.message.tool_calls is None
    assert resp.message.content == "Casa do Rio is free on 4 December."
    assert resp.message.finish_reason == "stop"
    assert (resp.input_tokens, resp.output_tokens) == (900, 40)


def test_malformed_script_names_file_and_turn(tmp_path):
    bad = tmp_path / "bad.yaml"
    bad.write_text("turns:\n  - tool_calls:\n      - name: x\n        arguments: nope\n")
    r = ModelRequest(ref=parse_model_ref(f"scripted/{bad}"), messages=[{"role": "user", "content": "go"}], tools=None)
    with pytest.raises(ScriptedModelError, match=r"bad.yaml.*turn 0"):
        asyncio.run(ScriptedBackend().complete(r))


def test_stream_yields_content_as_one_chunk():
    msgs = [{"role": "user", "content": "go"}]
    for i, name in enumerate(["search_venues", "check_dates"]):
        msgs.append(
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {"id": f"call_{i}_0", "type": "function", "function": {"name": name, "arguments": "{}"}}
                ],
            }
        )
        msgs.append({"role": "tool", "tool_call_id": f"call_{i}_0", "content": "ok"})

    async def collect():
        return [c async for c in ScriptedBackend().stream(req(msgs))]

    chunks = asyncio.run(collect())
    assert [c.delta for c in chunks] == ["Casa do Rio is free on 4 December."]


def test_seam_routes_scripted_refs_without_litellm(monkeypatch):
    import sys

    monkeypatch.setitem(sys.modules, "litellm", None)
    from jamjet.model.seam import Model

    m = Model(ungoverned=True)
    resp = asyncio.run(m.complete(req([{"role": "user", "content": "go"}])))
    assert resp.message.tool_calls[0].function.name == "search_venues"


def test_sidecar_serves_scripted_models(monkeypatch):
    import sys

    from starlette.testclient import TestClient

    import jamjet.model.sidecar_server as sidecar
    from jamjet.model.seam import Model

    monkeypatch.setitem(sys.modules, "litellm", None)
    monkeypatch.setattr(sidecar, "_get_model", lambda: Model(ungoverned=True))
    client = TestClient(sidecar.app)
    resp = client.post(
        "/v1/complete",
        json={"model": f"scripted/{SCRIPT}", "messages": [{"role": "user", "content": "go"}], "tools": TOOLS},
    )
    assert resp.status_code == 200
    body = resp.json()
    assert body["tool_calls"][0]["name"] == "search_venues"
    assert body["finish_reason"] == "tool_calls"


# ── what a learner sees when the script is wrong ─────────────────────────────


def _complete_with_path(path: str):
    r = ModelRequest(ref=parse_model_ref(f"scripted/{path}"), messages=[{"role": "user", "content": "go"}], tools=TOOLS)
    return run(r)


def test_only_yaml_paths_are_read(tmp_path):
    secret = tmp_path / "notes.txt"
    secret.write_text("password: hunter2\n", encoding="utf-8")
    with pytest.raises(ScriptedModelError, match=r"must end in \.yaml or \.yml") as info:
        _complete_with_path(str(secret))
    assert "hunter2" not in str(info.value)


def test_a_yml_path_is_accepted(tmp_path):
    script = tmp_path / "s.yml"
    script.write_text("turns:\n  - content: hi\n", encoding="utf-8")
    assert _complete_with_path(str(script)).message.content == "hi"


def test_a_binary_file_is_not_a_valid_script(tmp_path):
    bad = tmp_path / "bad.yaml"
    bad.write_bytes(b"\xff\xfe\x00 secret-bytes")
    with pytest.raises(ScriptedModelError, match="not a valid script") as info:
        _complete_with_path(str(bad))
    assert "secret-bytes" not in str(info.value)


def test_a_yaml_error_does_not_echo_the_file(tmp_path):
    bad = tmp_path / "bad.yaml"
    bad.write_text("turns: [unclosed, hunter2\n  : :\n", encoding="utf-8")
    with pytest.raises(ScriptedModelError, match="not a valid script") as info:
        _complete_with_path(str(bad))
    assert "hunter2" not in str(info.value)


def test_a_relative_path_error_does_not_name_the_working_directory(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    with pytest.raises(ScriptedModelError) as info:
        _complete_with_path("missing.yaml")
    assert "missing.yaml" in str(info.value)
    assert str(tmp_path) not in str(info.value)


def test_sidecar_returns_script_errors_as_400(monkeypatch):
    import sys

    from starlette.testclient import TestClient

    import jamjet.model.sidecar_server as sidecar
    from jamjet.model.seam import Model

    monkeypatch.setitem(sys.modules, "litellm", None)
    monkeypatch.setattr(sidecar, "_get_model", lambda: Model(ungoverned=True))
    client = TestClient(sidecar.app)
    messages = [{"role": "user", "content": "go"}] + [{"role": "assistant", "content": "x"} for _ in range(3)]
    resp = client.post("/v1/complete", json={"model": f"scripted/{SCRIPT}", "messages": messages, "tools": TOOLS})
    assert resp.status_code == 400
    assert "the script has 3 turns" in resp.json()["error"]


def _assistant(i, name):
    return {
        "role": "assistant",
        "content": "",
        "tool_calls": [{"id": f"call_{i}_0", "type": "function", "function": {"name": name, "arguments": "{}"}}],
    }


def test_an_earlier_missing_result_is_named():
    msgs = [
        {"role": "user", "content": "go"},
        _assistant(0, "search_venues"),
        _assistant(1, "check_dates"),
        {"role": "tool", "tool_call_id": "call_1_0", "content": "ok"},
    ]
    with pytest.raises(ScriptedModelError, match="no result for call_0_0"):
        run(req(msgs))


def _load(tmp_path, body):
    f = tmp_path / "s.yaml"
    f.write_text(body)
    from jamjet.model.scripted import load_script

    return load_script(str(f))


def test_an_unquoted_date_becomes_an_iso_string(tmp_path):
    turns = _load(
        tmp_path,
        "turns:\n  - tool_calls:\n      - name: check_dates\n"
        "        arguments: {date: 2026-12-11, when: [2026-12-12], q: '2026-12-13'}\n",
    )
    args = turns[0]["tool_calls"][0]["arguments"]
    assert args == {"date": "2026-12-11", "when": ["2026-12-12"], "q": "2026-12-13"}
    json.dumps(args)


def test_a_non_json_argument_names_file_turn_and_call(tmp_path):
    with pytest.raises(ScriptedModelError, match=r"s.yaml, turn 0, tool call 0.*set"):
        _load(tmp_path, "turns:\n  - tool_calls:\n      - name: x\n        arguments: {a: !!set {b, c}}\n")


@pytest.mark.parametrize(
    "usage", ["520", "{input_tokens: '900'}", "{output_tokens: -1}", "{input_tokens: true}", "{input_tokens: 1.5}"]
)
def test_a_bad_usage_is_refused_with_context(tmp_path, usage):
    with pytest.raises(ScriptedModelError, match=r"s.yaml, turn 0: usage"):
        _load(tmp_path, f"turns:\n  - content: hi\n    usage: {usage}\n")


def test_a_valid_explicit_usage_is_used(tmp_path):
    f = tmp_path / "s.yaml"
    f.write_text("turns:\n  - content: hi\n    usage: {input_tokens: 7, output_tokens: 0}\n")
    r = ModelRequest(ref=parse_model_ref(f"scripted/{f}"), messages=[{"role": "user", "content": "go"}], tools=None)
    resp = run(r)
    assert (resp.input_tokens, resp.output_tokens) == (7, 0)


def test_an_omitted_usage_keeps_the_estimate(tmp_path):
    f = tmp_path / "s.yaml"
    f.write_text("turns:\n  - content: hi\n")
    r = ModelRequest(ref=parse_model_ref(f"scripted/{f}"), messages=[{"role": "user", "content": "go"}], tools=None)
    assert run(r).input_tokens == len("go") // 4
