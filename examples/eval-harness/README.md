# Eval Harness Example

Demonstrates `jamjet eval run` for batch scoring a workflow against a Q&A dataset.

## Dataset format (`qa_pairs.jsonl`)

One JSON object per line. Required field: `input`. Optional: `expected`, `metadata`, `id`.

## The workflow under test (`workflow.yaml`)

`eval_qa` is one model node that answers the `query` field of each row.
`jamjet eval run` scores a workflow the runtime already has, so register it
first. Running it once does that:

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
jamjet dev                                   # terminal 1
jamjet run workflow.yaml --input '{"query": "What is the capital of France?"}'
```

## Run

```bash
# Score with LLM judge
jamjet eval run qa_pairs.jsonl \
  --workflow eval_qa \
  --rubric "Rate factual accuracy and completeness 1-5" \
  --min-score 3

# Add a deterministic assertion and a latency budget
# (a model node writes its answer to the run state as last_model_output)
jamjet eval run qa_pairs.jsonl \
  --workflow eval_qa \
  --assert "'last_model_output' in output" \
  --latency-ms 5000

# CI: fail if pass rate < 80%
jamjet eval run qa_pairs.jsonl \
  --workflow eval_qa \
  --rubric "Rate quality 1-5" \
  --fail-below 80 \
  --output results.json
```

## Python API

```python
from jamjet.eval import EvalDataset, EvalRunner, LlmJudgeScorer, AssertionScorer

dataset = EvalDataset.from_file("qa_pairs.jsonl")
runner = EvalRunner(
    workflow_id="eval_qa",
    scorers=[
        LlmJudgeScorer(rubric="Rate accuracy and completeness 1-5", min_score=3),
        AssertionScorer(checks=["'last_model_output' in output"]),
    ],
)
results = await runner.run(dataset)
EvalRunner.print_summary(results)
```
