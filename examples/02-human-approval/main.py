"""Pause for human approval on a risky action. No API key required."""

from jamjet.cloud.policy import PolicyEvaluator


def main() -> None:
    evaluator = PolicyEvaluator()
    evaluator.add("require_approval", "payments.*")

    tool = "payments.refund"
    decision = evaluator.evaluate(tool)

    print(f"Tool: {tool}")
    print("Policy: payments.* requires approval")
    if decision.policy_kind == "require_approval":
        print("Decision: WAITING_FOR_APPROVAL")
        print("Approve via your control plane, then resume.")
    else:
        print("Decision: ALLOWED")
    print("The model is mocked. The enforcement path is real.")


if __name__ == "__main__":
    main()
