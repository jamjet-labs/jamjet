# Code review routing

This example subclasses `DefaultCoordinatorStrategy` and overrides `score()` to
route code reviews to mock Python, Rust, and frontend reviewers. It runs locally
without a JamJet server, model API key, or network access.

From the repository root, install the SDK and run:

```bash
cd sdk/python
uv sync --extra dev
uv run python ../../examples/code-review-routing/main.py
```

The four scenarios select `python-reviewer`, `python-deep-reviewer`,
`rust-reviewer`, and `frontend-reviewer`, respectively. No review is executed;
the example prints the routing decision and each candidate's score.

Language expertise is an explicit 0–1 score in each mock agent card and carries
the highest weight. The changed-line count is a simple proxy for diff complexity:
reviewers exceeding their advertised capacity receive a lower latency score.
This lets a fast Python reviewer handle a small diff and a slower reviewer with
more capacity handle a large one. Cost has a smaller weight. Pass dimension
weights to `score()` to override these defaults.

Real routing needs independently measured expertise, capacity, and review
outcomes. Line count alone does not capture semantic complexity. The inherited
`decide()` method is used with an empty tiebreaker model so these examples remain
deterministic and do not call an LLM.
