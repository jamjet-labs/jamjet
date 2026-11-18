# Episode 1 challenge: a weather tool

The challenge at the end of episode 1: add a weather tool, so the agent
can warn you if it will rain on the day. Start from
`step-03-pay-deposit`. Try it yourself before you read on.

## Install

    pip install "jamjet[sidecar]==0.13.0"

Set the signing key from the series README, under "Before you start".

## What changed from step 03

`venue_tools.py` has a third tool, with a made-up forecast:

    @tool
    def get_weather(city: str, date: str) -> str:
        """Get the weather forecast for a city on a date (YYYY-MM-DD)."""
        days = FORECAST.get(city.strip().lower(), {})
        return days.get(date, "no forecast for that day")

The model reads the docstring to decide when to call it.

`agent.py` has one more name in the tools list:

    tools=[search_venues, check_dates, get_weather, pay_deposit],

and the question asks about rain:

    " and pay a 500 deposit. Warn me if it will rain."

`script.yaml` has one more turn, for the scripted model.

## Run

    python agent.py

## Expected output

    model: scripted (no API key)
    Booked Casa do Rio in Lisbon for 11 December. The 500 deposit is paid. Rain is forecast that day, so plan for umbrellas.
    called search_venues(city='Lisbon', date='2026-12-11')
    called check_dates(venue='Casa do Rio')
    called get_weather(city='Lisbon', date='2026-12-11')
    called pay_deposit(venue='Casa do Rio', amount=500)
    payments.log: 1 line(s)

With a real model the words differ. On Claude Sonnet 5.5 it skipped
`check_dates`, called `get_weather` before it paid, and warned about
the rain, in both of my runs.

The payment still has no check. Episode 2 fixes that.
