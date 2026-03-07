# Eval Node Guide

> **Status:** Planned — Phase 3. This guide will be written when the `eval` node type is implemented.

---

## Overview

JamJet treats evaluation as a **first-class workflow primitive** via the `eval` node type. Rather than running evaluations separately after the fact, eval nodes live inside your workflow graph — making scoring durable, replayable, and auditable alongside execution.

## Planned capabilities

- **LLM-as-judge** — configurable rubrics, multi-dimensional scoring
- **Deterministic assertions** — Python expressions evaluated against node output
- **Latency and cost scorers** — assert performance thresholds
- **Custom scorers** — pluggable via Python entry points
- **Eval datasets** — `jamjet eval run --dataset qa_pairs.jsonl` for batch scoring
- **CI integration** — eval nodes run as regression tests in CI pipelines
- **Feedback loops** — on failure, feed scores back to agent for retry

## Planned YAML shape

```yaml
nodes:
  evaluate:
    type: eval
    scorers:
      - type: llm_judge
        model: gpt-4o
        rubric: "Rate completeness and accuracy 1-5"
      - type: assertion
        checks:
          - "'sources' in output"
          - "len(output.sources) >= 3"
      - type: latency
        threshold_ms: 5000
      - type: cost
        threshold_usd: 0.50
    on_fail: retry_with_feedback
    max_retries: 2
    next: deliver
```

## Tracking

Follow progress in `progress-tracker.md` under Phase 3, tasks 3.13–3.17.
