from jamjet.agent_tool import agent_tool


class TestAgentTool:
    def test_creates_sync_tool(self):
        tool = agent_tool(
            agent="jamjet://org/classifier",
            mode="sync",
            description="Classifies text",
        )
        assert tool.agent_uri == "jamjet://org/classifier"
        assert tool.mode == "sync"
        assert tool.description == "Classifies text"

    def test_creates_streaming_tool(self):
        tool = agent_tool(
            agent="jamjet://org/researcher",
            mode="streaming",
            description="Research agent",
            budget={"max_cost_usd": 1.00},
        )
        assert tool.mode == "streaming"
        assert tool.budget["max_cost_usd"] == 1.00

    def test_creates_auto_tool(self):
        tool = agent_tool(
            agent="auto",
            mode="sync",
            description="Auto-routed tool",
        )
        assert tool.agent_uri == "auto"

    def test_default_mode_is_sync(self):
        tool = agent_tool(agent="jamjet://org/test", description="Test")
        assert tool.mode == "sync"

    def test_compiles_to_ir_node(self):
        tool = agent_tool(
            agent="jamjet://org/classifier",
            description="Classifies text",
        )
        ir = tool.to_ir_kind()
        assert ir["type"] == "agent_tool"
        assert ir["agent"]["explicit"] == "jamjet://org/classifier"
        assert ir["mode"] == "sync"

    def test_auto_compiles_to_ir_with_auto_flag(self):
        tool = agent_tool(agent="auto", description="Auto")
        ir = tool.to_ir_kind()
        assert ir["agent"]["auto"] is True

    def test_conversational_mode_includes_max_turns(self):
        tool = agent_tool(
            agent="jamjet://org/negotiator",
            mode="conversational",
            description="Negotiator",
            max_turns=5,
        )
        ir = tool.to_ir_kind()
        assert ir["mode"] == {"conversational": {"max_turns": 5}}
