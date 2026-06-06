# Fleet example: an ops/monitoring fleet

One YAML file (`fleet.yaml`) declares three scheduled units that run on the local
runtime, no cloud:

| Unit | Kind | Schedule | Trigger shown |
| --- | --- | --- | --- |
| `morning_briefing` | agent (plan-and-execute) | `0 8 * * *` (daily 08:00 UTC) | cron + manual |
| `reconciler` | agent (react) | `30 14 * * 1-5` (weekdays 14:30 UTC) | cron + sibling ref |
| `healthcheck` | workflow (explicit graph) | `*/15 * * * *` (every 15 min) | cron |

It exercises all three ways a unit can run: a cron schedule, an on-demand
`jamjet run`, and one agent referencing another (`reconciler` uses
`agent:morning_briefing`).

## Prerequisites

```bash
pip install "jamjet>=0.9.0"
export ANTHROPIC_API_KEY="sk-ant-..."   # or use Ollama, see "Run offline" below
```

## 1. Start the local runtime

```bash
jamjet dev
```

In dev mode this also starts the in-process cron scheduler, so deployed schedules
fire locally. State is a local SQLite file under `.jamjet/`.

## 2. Deploy the fleet

```bash
jamjet deploy fleet.yaml
```

This registers all three units and installs a cron job for each. You will see a
summary like:

```
registered morning_briefing v0.1.0
registered reconciler v0.1.0
registered healthcheck v0.1.0
scheduled morning_briefing [0 8 * * *] next=2026-06-07T08:00:00+00:00
scheduled reconciler [30 14 * * 1-5] next=2026-06-09T14:30:00+00:00
scheduled healthcheck [*/15 * * * *] next=2026-06-06T12:15:00+00:00
```

## 3. Inspect the installed schedules

```bash
curl -s http://localhost:7700/cron | jq
```

Each job shows its `cron_expression` and an advancing `next_run_at`. The
`healthcheck` job (every 15 min) is the quickest way to watch the scheduler fire
an execution:

```bash
curl -s http://localhost:7700/executions | jq '.executions[].workflow_id'
```

## 4. Run one unit on demand

You do not have to wait for a schedule. Run any unit by name:

```bash
jamjet run fleet.yaml morning_briefing --input '{"focus": "infrastructure"}'
```

`jamjet run` registers every unit in the file, then starts the one you name. For
a single-unit file the name is optional; with multiple units you must pick one
(omitting it lists the available units).

## Run offline (Ollama)

No API key, no internet:

```bash
ollama serve
ollama pull llama3.1
```

Set `model:` in `fleet.yaml` (in `defaults` and the `healthcheck` node) to a local
model name such as `llama3.1`, then `jamjet deploy fleet.yaml` as above. State,
worker, and scheduler are all local, so the whole fleet runs air-gapped.

## What runs today vs what is deferred

This example is honest about the current (0.9.0) runtime:

- **Runs today:** authoring the fleet, `jamjet deploy`, `jamjet run <unit>`, cron
  scheduling and firing, and each agent's LLM reasoning. Cron is 5-field and
  **UTC only**.
- **Deferred:** tool calls (`tool:web_search`, `tool:ledger_query`) are surfaced
  to the agent by **name** in its prompt; they are validated at compile time but
  are not executed by the runtime yet. Sibling references (`agent:morning_briefing`)
  are likewise validated but not invoked synchronously yet. The embedded cron
  scheduler is dev-only and cron jobs are global (single-tenant); a dedicated
  production cron process is a follow-up.

See the guide at https://docs.jamjet.dev/open-source/multi-agent-fleets for the
full reference.
