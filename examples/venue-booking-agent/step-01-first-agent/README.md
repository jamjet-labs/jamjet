# Step 01: a first agent

## Install

    pip install "jamjet[sidecar]==0.13.0"

## Run

    python agent.py

## Expected output

    model: scripted (no API key)
    Tell me the exact dates, whether you need a stage or just seats, and your budget. I can then look for venues in Lisbon that fit 40 people.

At this point an agent is a model plus instructions. It can answer, but it
cannot look anything up or do anything yet.
