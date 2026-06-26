import jamjet


@jamjet.tool
async def noop(x: str) -> str:
    return x


def test_anthropic_model_string_compiles_to_anthropic_provider():
    agent = jamjet.Agent("a", model="anthropic/claude-opus-4-8", tools=[noop])
    spec = agent.compile()
    assert spec.llm.provider == "anthropic"
    assert spec.llm.model == "anthropic/claude-opus-4-8"
    assert spec.llm.api_key_env == "ANTHROPIC_API_KEY"


def test_bare_model_string_stays_openai():
    agent = jamjet.Agent("a", model="gpt-4o", tools=[noop])
    spec = agent.compile()
    assert spec.llm.provider == "openai"
    assert spec.llm.model == "gpt-4o"


def test_gemini_maps_to_google_literal():
    agent = jamjet.Agent("a", model="gemini/gemini-1.5-pro", tools=[noop])
    spec = agent.compile()
    assert spec.llm.provider == "google"
