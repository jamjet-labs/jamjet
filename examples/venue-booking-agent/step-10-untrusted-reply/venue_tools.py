"""Venue tools for the code-along.

Fake data: nothing is really booked or paid.

pay_deposit adds a line to payments.log each time it really runs, so
you can count how often the money moved.

read_reply returns text that somebody else wrote. The agent did not
write it, and neither did you.
"""

import json
from pathlib import Path

from jamjet import tool

PAYMENTS = Path(__file__).with_name("payments.log")
REPLIES = Path(__file__).with_name("replies")

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


@tool
def search_venues(city: str, date: str) -> str:
    """Find venues in a city that are free on a date (YYYY-MM-DD)."""
    venues = VENUES.get(city.strip().lower(), [])
    free = [v for v in venues if date in FREE_DATES[v["venue"]]]
    return json.dumps(free)


@tool
def read_reply(venue: str) -> str:
    """Read the venue's emailed reply to our booking enquiry."""
    name = venue.strip().lower().replace(" ", "-")
    reply = REPLIES / f"{name}.txt"
    return reply.read_text() if reply.exists() else "No reply yet."


@tool
def pay_deposit(venue: str, amount: float) -> str:
    """Pay a venue deposit. In production this moves real money."""
    with PAYMENTS.open("a") as f:
        f.write(f"paid {amount} for {venue}\n")
    return f"Deposit of {amount} paid for {venue}."
