# Step 02: tools

## Install

    pip install "jamjet[sidecar]==0.13.0"

## Run

    python agent.py

## Expected output

    model: scripted (no API key)
    Casa do Rio in Lisbon is free on 11 December. It is also free on 4 December.
    called search_venues(city='Lisbon', date='2026-12-11')
    called check_dates(venue='Casa do Rio')

The agent now has two tools in `venue_tools.py`: `search_venues` and
`check_dates`. The model asks for them, JamJet runs them, and the model
answers from what they return. The `called` lines come from
`result.tool_calls`. The data is fake.
