# team-multi-agent

Compose several `Agent`s into a coordinated multi-agent workflow with the `Team`
API. Two patterns over the same two specialists (a researcher and a writer):

- A coordinator `Team`: a router agent picks one specialist to handle each
  request.
- A `Sequential` pipeline: the researcher's output feeds the writer.

## How it works (Path A)

A team is plain Python orchestration over the single-agent path. Each sub-agent
runs as its own independent execution via `Agent.run` (in-process) or
`Agent.run_durable` (durable), and the team composes the results. There is no
custom orchestration to write and no Rust involved. Because each sub-agent is its
own execution, a failing sub-agent is isolated: its error lands in
`TeamResult.per_agent`, and the team does not crash.

```python
from jamjet import Team, Sequential

desk = Team([researcher, writer], coordinator=router)   # router picks one
result = await desk.run("Find the latest on agent runtimes")
print(result.output)            # the chosen specialist's answer
print(list(result.per_agent))   # ['router', 'researcher']

pipeline = Sequential([researcher, writer])             # research then write
result = await pipeline.run("agent runtimes")
```

The coordinator's `coordinator=` is either a router `Agent` whose output names the
specialist (as here), or a plain Python routing callable
`(input, agents) -> Agent | name | index`.

## Files

- `specialists.py` - the `@tool` functions, the three Agent factories
  (`researcher`, `writer`, `router`), and the two team factories (`build_desk`,
  `build_pipeline`). Imported by both the runner and, on the durable path, the
  worker.
- `main.py` - runs the coordinator team and the sequential pipeline with `.run()`.

## Governance inheritance

`build_desk()` sets a team governance default (`governance={"budget": 0.50}`).
The specialists set no budget of their own, so they inherit the team cap and each
one enforces it. A team never bypasses governance: a sub-agent that sets its own
budget, policy, PII, or allowlist keeps it, and a governed sub-agent in a team
still denies an over-budget call.

## Sessions

Pass `session=` to a team to give each sub-agent its own persistent thread. The
team namespaces a distinct child session per sub-agent, so concurrent sub-agents
never write the same row.

## Run it (in-process)

The in-process path needs only a model provider key.

```bash
export ANTHROPIC_API_KEY=sk-...
python main.py
```

## Run it durable

Swap `.run(...)` for `.run_durable(...)` in `main.py`. The durable path runs each
sub-agent as a durable, event-sourced execution, so every sub-agent gets the event
log, replay, idempotency, and artifacts. It needs the engine, a `jamjet worker`
draining the tool queue, and the model sidecar running. See the
`react-agent-durable` example for the four-terminal setup; a team simply starts
one such execution per sub-agent.
