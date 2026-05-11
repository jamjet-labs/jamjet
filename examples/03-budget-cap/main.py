"""Stop a runaway agent loop at a $0.05 budget cap."""

CAP_USD = 0.05
STEPS = [("search.web", 0.02), ("search.web", 0.02), ("search.web", 0.02)]


def main() -> None:
    spent = 0.0
    blocked = None
    for i, (tool, cost) in enumerate(STEPS, start=1):
        if spent + cost > CAP_USD:
            blocked = i
            print(f"Step {i}: {tool} ${cost:.2f}  BUDGET_EXCEEDED")
            break
        spent += cost
        print(f"Step {i}: {tool} ${cost:.2f}  ALLOWED")
    print(f"Spent: ${spent:.2f} of ${CAP_USD:.2f} cap")
    print("Decision: BUDGET_EXCEEDED" if blocked else "Decision: WITHIN_BUDGET")
    print("The model is mocked. The enforcement path is real.")


if __name__ == "__main__":
    main()
