# Eval Harness Example

Demonstrates `jamjet eval run` for batch scoring a workflow against a Q&A dataset.

## Dataset format (`qa_pairs.jsonl`)

One JSON object per line. Required field: `input`. Optional: `expected`, `metadata`, `id`.

## Run

```bash
# Score with LLM judge
jamjet eval run qa_pairs.jsonl \
  --workflow my_workflow \
  --rubric "Rate factual accuracy and completeness 1-5" \
  --min-score 3

# Add deterministic assertions
jamjet eval run qa_pairs.jsonl \
  --workflow rag_workflow \
  --assert "'answer' in output" \
  --assert "len(output.get('sources', [])) >= 1" \
  --latency-ms 5000

# CI: fail if pass rate < 80%
jamjet eval run qa_pairs.jsonl \
  --workflow my_workflow \
  --rubric "Rate quality 1-5" \
  --fail-below 80 \
  --output results.json
```

## Python API

```python
from jamjet.eval import EvalDataset, EvalRunner, LlmJudgeScorer, AssertionScorer

dataset = EvalDataset.from_file("qa_pairs.jsonl")
runner = EvalRunner(
    workflow_id="my_workflow",
    scorers=[
        LlmJudgeScorer(rubric="Rate accuracy and completeness 1-5", min_score=3),
        AssertionScorer(checks=["'answer' in output"]),
    ],
)
results = await runner.run(dataset)
EvalRunner.print_summary(results)
```
