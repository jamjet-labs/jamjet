from jamjet.cloud.middleware.context import (
    call_context_from_openai_kwargs,
    openai_kwargs_from_call_context,
)


def test_openai_kwargs_to_context():
    kwargs = {
        "model": "gpt-4o-mini",
        "messages": [
            {"role": "system", "content": "be helpful"},
            {"role": "user", "content": "hi alice@example.com"},
        ],
        "tools": [{"type": "function", "function": {"name": "lookup"}}],
        "temperature": 0.7,
        "max_tokens": 256,
    }
    ctx = call_context_from_openai_kwargs(kwargs)
    assert ctx.provider == "openai"
    assert ctx.model == "gpt-4o-mini"
    assert ctx.system == "be helpful"  # extracted from system message
    assert len(ctx.messages) == 1  # system stripped from messages
    assert ctx.messages[0]["role"] == "user"
    assert ctx.tools == kwargs["tools"]
    assert ctx.extra_kwargs == {"temperature": 0.7, "max_tokens": 256}
    assert ctx.identifier == "openai:gpt-4o-mini"


def test_context_round_trips_back_to_openai_kwargs():
    original = {
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [],
        "stream": False,
    }
    ctx = call_context_from_openai_kwargs(original)
    rebuilt = openai_kwargs_from_call_context(ctx)
    assert rebuilt["model"] == "gpt-4o"
    assert rebuilt["messages"] == [{"role": "user", "content": "hi"}]
    assert rebuilt["stream"] is False


def test_round_trip_preserves_redacted_messages():
    """The whole point: middleware mutates ctx.messages; the rebuilt kwargs
    must reflect that mutation so the LLM only ever sees redacted content."""
    ctx = call_context_from_openai_kwargs(
        {
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "email me at alice@example.com"}],
        }
    )
    ctx.messages = [{"role": "user", "content": "email me at [REDACTED:EMAIL]"}]
    rebuilt = openai_kwargs_from_call_context(ctx)
    assert rebuilt["messages"][0]["content"] == "email me at [REDACTED:EMAIL]"


def test_no_system_message_yields_none():
    ctx = call_context_from_openai_kwargs(
        {
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
        }
    )
    assert ctx.system is None
    assert len(ctx.messages) == 1


from jamjet.cloud.middleware.context import (  # noqa: E402  (staged mid-file import — pattern used throughout this file)
    anthropic_kwargs_from_call_context,
    call_context_from_anthropic_kwargs,
)


def test_anthropic_kwargs_to_context():
    kwargs = {
        "model": "claude-haiku-4-5",
        "system": "be helpful",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{"name": "lookup", "description": "look up"}],
        "max_tokens": 1024,
    }
    ctx = call_context_from_anthropic_kwargs(kwargs)
    assert ctx.provider == "anthropic"
    assert ctx.model == "claude-haiku-4-5"
    assert ctx.system == "be helpful"
    assert len(ctx.messages) == 1
    assert ctx.tools[0]["name"] == "lookup"
    assert ctx.extra_kwargs == {"max_tokens": 1024}
    assert ctx.identifier == "anthropic:claude-haiku-4-5"


def test_anthropic_context_round_trip_preserves_mutation():
    ctx = call_context_from_anthropic_kwargs(
        {
            "model": "claude-haiku-4-5",
            "system": "be helpful",
            "messages": [{"role": "user", "content": "ssn 123-45-6789"}],
        }
    )
    ctx.messages = [{"role": "user", "content": "ssn [REDACTED:US_SSN]"}]
    rebuilt = anthropic_kwargs_from_call_context(ctx)
    assert rebuilt["model"] == "claude-haiku-4-5"
    assert rebuilt["system"] == "be helpful"
    assert rebuilt["messages"][0]["content"] == "ssn [REDACTED:US_SSN]"


def test_anthropic_no_system_omits_key():
    ctx = call_context_from_anthropic_kwargs(
        {
            "model": "claude-haiku-4-5",
            "messages": [{"role": "user", "content": "hi"}],
        }
    )
    rebuilt = anthropic_kwargs_from_call_context(ctx)
    assert "system" not in rebuilt
