"""Venue tools for the code-along.

Fake data: nothing is really booked or paid.

pay_deposit adds a line to payments.log each time it really runs, so
you can count how often the money moved. get_weather reads a made-up
forecast.
"""

import json
from pathlib import Path

from jamjet import tool

PAYMENTS = Path(__file__).with_name("payments.log")

VENUES = {
    "lisbon": [
        {"venue": "Casa do Rio", "capacity": 120, "price": 2400},
        {"venue": "Armazem 16", "capacity": 300, "price": 5200},
    ],
}

FREE_DATES = {
    "Casa do Rio": ["2026-12-04", "2026-12-11"],
    "Armazem 16": ["2026-12-18"],
}

FORECAST = {
    "lisbon": {
        "2026-12-04": "sunny, 17 C",
        "2026-12-11": "rain, 14 C",
        "2026-12-18": "cloudy, 15 C",
    },
}


@tool
def search_venues(city: str, date: str) -> str:
    """Find venues in a city that are free on a date (YYYY-MM-DD)."""
    venues = VENUES.get(city.strip().lower(), [])
    free = [v for v in venues if date in FREE_DATES[v["venue"]]]
    return json.dumps(free)


@tool
def check_dates(venue: str) -> str:
    """List the free dates for one venue."""
    return json.dumps(FREE_DATES.get(venue, []))


@tool
def get_weather(city: str, date: str) -> str:
    """Get the weather forecast for a city on a date (YYYY-MM-DD)."""
    days = FORECAST.get(city.strip().lower(), {})
    return days.get(date, "no forecast for that day")


@tool
def pay_deposit(venue: str, amount: float) -> str:
    """Pay a venue deposit. In production this moves real money."""
    with PAYMENTS.open("a") as f:
        f.write(f"paid {amount} for {venue}\n")
    return f"Deposit of {amount} paid for {venue}."
