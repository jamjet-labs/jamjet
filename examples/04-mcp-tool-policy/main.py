"""Evaluate a policy against an MCP-shaped request envelope. Preview of JamJet Gateway."""

from jamjet.cloud.policy import PolicyEvaluator


def main() -> None:
    evaluator = PolicyEvaluator()
    evaluator.add("block", "*delete*")

    envelope = {
        "server": "postgres-mcp",
        "tool": "postgres/database.delete_all_customers",
        "arguments": {"confirm": True},
    }
    decision = evaluator.evaluate(envelope["tool"])

    print(f"Server: {envelope['server']}")
    print(f"Tool: {envelope['tool']}")
    print("Policy: block '*delete*'")
    print(f"Decision: {'BLOCKED' if decision.blocked else 'ALLOWED'}")
    print("This demo uses an MCP-shaped request envelope to show policy evaluation.")
    print(
        "It is not yet an MCP proxy. Full MCP proxy support is planned for JamJet Gateway."
    )
    print("The model is mocked. The enforcement path is real.")


if __name__ == "__main__":
    main()
