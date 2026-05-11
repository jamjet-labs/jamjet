"""Block an unsafe tool call before execution. No API key required."""

from jamjet.cloud.policy import PolicyEvaluator


def main() -> None:
    evaluator = PolicyEvaluator()
    evaluator.add("block", "*delete*")

    tool = "database.delete_all_customers"
    decision = evaluator.evaluate(tool)

    print(f"Tool: {tool}")
    print(f"Policy: block '*delete*'")
    print(f"Decision: {'BLOCKED' if decision.blocked else 'ALLOWED'}")
    print(f"Executed: {'false' if decision.blocked else 'true'}")
    print("The model is mocked. The enforcement path is real.")


if __name__ == "__main__":
    main()
