# Venue-booking agent

The code for the JamJet code-along series. You build one agent that books
event venues, one small step at a time. Each step has its own folder, so you
can run any step on its own.

Nothing is really booked or paid. The "payments" are lines in a log file.

## Install

    pip install "jamjet[sidecar]==0.13.0"

## Before you start

Set a signing key for the audit log:

    export JAMJET_AUDIT_SIGNING_KEY=$(python -c "import secrets; print(secrets.token_hex(32))")

It signs the agent's audit log. Without it, JamJet prints a warning.

## Pick a model

By default every step runs on a scripted model. It answers from a YAML file
next to the code, so you need no API key and no network.

To use a real model, install the model extra and set `VENUE_MODEL` and the
provider key:

    pip install "jamjet[sidecar,model]==0.13.0"
    export ANTHROPIC_API_KEY=...
    VENUE_MODEL=anthropic/claude-sonnet-4-6 python agent.py

The first line the agent prints says which model it is using.

## Steps

| Step | Episode | What it adds |
|---|---|---|
| [step-01-first-agent](step-01-first-agent/) | 1 | An agent: a model and instructions |

More steps are added as the series goes on.

## If you fall behind

Copy the folder for the step you are on and run it from inside that folder.
Every folder is complete on its own.
