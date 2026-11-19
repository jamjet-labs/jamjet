# Venue-booking agent

The code for the JamJet code-along series. You build one agent that books
event venues, one small step at a time. Each step has its own folder, so you
can run any step on its own.

Nothing is really booked or paid. The "payments" are lines in a log file.

Watch it built: [Build an AI Agent with JamJet (Python Code-Along)](https://www.youtube.com/playlist?list=PLD5Bg9vUNgik)
on YouTube. Part 1 covers steps 1 to 3.

## Install

Python 3.11 or newer (checked on 3.12).

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
    VENUE_MODEL=anthropic/claude-sonnet-5-5 python agent.py

The first line the agent prints says which model it is using.

Every step was checked on `anthropic/claude-sonnet-5-5`. A real model
words its answers its own way and can call the tools in a different
order, so its output will not match the expected output in each README.
What runs, what waits for approval and what is blocked stay the same.

## Steps

| Step | Episode | What it adds | Runs |
|---|---|---|---|
| [step-01-first-agent](step-01-first-agent/) | 1 | An agent: a model and instructions | in-process |
| [step-02-tools](step-02-tools/) | 1 | Two tools: search venues and check dates | in-process |
| [step-03-pay-deposit](step-03-pay-deposit/) | 1 | A pay_deposit tool that pays with no checks | in-process |
| [step-04-budget](step-04-budget/) | 2 | A token budget that stops a looping run | in-process |
| [step-05-ask-first](step-05-ask-first/) | 2 | Approval before pay_deposit; the run pauses until you approve | durable |
| [step-06-block](step-06-block/) | 2 | A blocked cancel_booking tool; the run fails and the audit says why | durable |
| [step-07-engine-crash](step-07-engine-crash/) | 3 | Kill the engine while a payment waits; it still pays once | durable |
| [step-08-what-it-remembers](step-08-what-it-remembers/) | 3 | Read the event log the engine resumed from | durable, read-only |
| [step-09-audit](step-09-audit/) | 3 | Read the audit: who approved this? | durable, read-only |

In-process steps run with `python agent.py`. Durable steps need
`jamjet dev` running, and their READMEs say how.

## Challenge solutions

Each episode ends with a challenge. Try it first, then compare.

| Episode | Challenge | Solution |
|---|---|---|
| 1 | Add a weather tool | [solutions/ep1-weather-tool](solutions/ep1-weather-tool/) |
| 2 | Ask for approval only above 300 | [solutions/ep2-approval-above-300](solutions/ep2-approval-above-300/) |
| 3 | Kill the engine at a different moment | [step 08, "Try it yourself"](step-08-what-it-remembers/README.md#try-it-yourself) |

## If you fall behind

Copy the folder for the step you are on and run it from inside that folder.
Every folder is complete on its own.
