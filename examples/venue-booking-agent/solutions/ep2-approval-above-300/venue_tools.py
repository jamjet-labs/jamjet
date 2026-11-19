"""Venue tools for the code-along.

Fake data: nothing is really booked or paid.

Both payment tools add a line to payments.log each time they really
run, so you can count how often the money moved. pay_small_deposit
pays with no approval while a venue's total stays at 300 or less. Its
code refuses more, whatever the model asks for.
"""

import json
from pathlib import Path

from jamjet import tool

PAYMENTS = Path(__file__).with_name("payments.log")
SMALL_LIMIT = 300

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
def check_dates(venue: str) -> str:
    """List the free dates for one venue."""
    return json.dumps(FREE_DATES.get(venue, []))


def record_payment(venue: str, amount: float) -> str:
    with PAYMENTS.open("a") as f:
        f.write(f"paid {amount} for {venue}\n")
    return f"Deposit of {amount} paid for {venue}."


def paid_so_far(venue: str) -> float:
    """Add up what payments.log says this venue was paid."""
    if not PAYMENTS.exists():
        return 0.0
    lines = PAYMENTS.read_text().splitlines()
    mine = [line for line in lines if line.endswith(f" for {venue}")]
    return sum(float(line.split()[1]) for line in mine)


@tool
def pay_small_deposit(venue: str, amount: float) -> str:
    """Pay a venue deposit of 300 or less. It needs no approval."""
    total = paid_so_far(venue) + amount
    # Two payments of 250 add up to 500, so the limit is on the total.
    if 0 < amount and total <= SMALL_LIMIT:
        return record_payment(venue, amount)
    return (
        f"Refused: {amount} for {venue}. This tool pays a venue"
        f" up to {SMALL_LIMIT} in total. Use pay_deposit, which"
        " waits for approval."
    )


@tool
def pay_deposit(venue: str, amount: float) -> str:
    """Pay a deposit over 300. In production this moves real money."""
    return record_payment(venue, amount)
