# Fleet example

One YAML file declaring two strategy agents and one explicit workflow, each on
its own cron schedule, runnable on the local runtime with no cloud.

```bash
jamjet dev                          # local runtime; embeds the cron scheduler in dev
jamjet deploy examples/fleet/fleet.yaml
jamjet run examples/fleet/fleet.yaml researcher --input '{"topic": "agents"}'
```

Offline: start Ollama (`ollama serve`) and set the agents' `model:` to a local
model name (e.g. `llama3.1`); no API key or internet required.
