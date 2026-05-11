def test_public_surface_complete():
    import jamjet

    expected = {
        "DurableAgent",
        "workflow",
        "task",
        "tool",
        "AgentSpec",
        "DurableAgentSpec",
        "WorkflowSpec",
        "MemoryConfig",
        "LLMConfig",
        "ToolSpec",
        "DurabilityConfig",
        "IR_VERSION",
        "Agent",
        "AgentResult",
        "Workflow",
        "Runtime",
        "LocalRuntime",
        "RuntimeResult",
        "RuntimeEvent",
        "AgentMemory",
        "Scope",
        "run",
        "resume",
        "deploy",
    }
    public_attrs = {n for n in dir(jamjet) if not n.startswith("_")}
    missing = expected - public_attrs
    assert not missing, f"Missing exports: {missing}"
