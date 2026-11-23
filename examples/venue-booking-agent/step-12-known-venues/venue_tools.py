"""Venue tools for the code-along.

Fake data: nothing is really booked or paid.

These are step 10's tools with one change. read_reply and pay_deposit
check what they are given before they act. A venue name must be one we
hold. An amount must be a number above zero. A refusal goes back to
the model as text, and nothing is written to payments.log.
"""

import json
import math
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


def known(venue: object) -> str | None:
    """The venue's name as we hold it, or None if we do not know it."""
    wanted = str(venue).strip().lower()
    for name in FREE_DATES:
        if name.lower() == wanted:
            return name
    return None


def no_such_venue(venue: object) -> str:
    return (
        f"Refused: there is no venue called {str(venue)!r}."
        " Use a name that search_venues returned."
    )


@tool
def search_venues(city: str, date: str) -> str:
    """Find venues in a city that are free on a date (YYYY-MM-DD)."""
    venues = VENUES.get(city.strip().lower(), [])
    free = [v for v in venues if date in FREE_DATES[v["venue"]]]
    return json.dumps(free)


@tool
def read_reply(venue: str) -> str:
    """Read the venue's emailed reply to our booking enquiry."""
    name = known(venue)
    if name is None:
        return no_such_venue(venue)
    reply = REPLIES / f"{name.lower().replace(' ', '-')}.txt"
    return reply.read_text() if reply.exists() else "No reply yet."


@tool
def pay_deposit(venue: str, amount: float) -> str:
    """Pay a venue deposit. In production this moves real money."""
    name = known(venue)
    if name is None:
        return no_such_venue(venue)
    number = type(amount) in (int, float)
    if not number or not math.isfinite(amount) or amount <= 0:
        return f"Refused: {amount!r} is not an amount I can pay."
    with PAYMENTS.open("a") as f:
        f.write(f"paid {amount} for {name}\n")
    return f"Deposit of {amount} paid for {name}."
