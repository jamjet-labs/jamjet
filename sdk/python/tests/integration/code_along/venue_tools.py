"""Venue tools for the code-along. Fake data: nothing is really booked or paid.

pay_deposit and cancel_booking add one line to a log next to this file each
time they really run, so you can count how often the money moved.
"""

import json
import os
from pathlib import Path

from jamjet import tool

PAYMENTS = Path(__file__).with_name("payments.log")
CANCELLATIONS = Path(__file__).with_name("cancellations.log")

VENUES = {
    "lisbon": [
        {"venue": "Casa do Rio", "capacity": 120, "price_per_day": 2400},
        {"venue": "Armazem 16", "capacity": 300, "price_per_day": 5200},
    ],
}

FREE_DATES = {
    "Casa do Rio": ["2026-12-04", "2026-12-11"],
    "Armazem 16": ["2026-12-18"],
}


@tool
def search_venues(city: str, date: str) -> str:
    """Find event venues in a city that are free on a date (YYYY-MM-DD)."""
    venues = VENUES.get(city.strip().lower(), [])
    return json.dumps([v for v in venues if date in FREE_DATES[v["venue"]]])


@tool
def check_dates(venue: str) -> str:
    """List the free dates for one venue."""
    return json.dumps(FREE_DATES.get(venue, []))


@tool
def pay_deposit(venue: str, amount: float) -> str:
    """Pay a booking deposit for a venue. In production this moves real money."""
    _log(PAYMENTS, f"paid {amount} for {venue} (pid {os.getpid()})")
    return f"Deposit of {amount} paid for {venue}."


@tool
def cancel_booking(venue: str) -> str:
    """Cancel the booking at a venue."""
    _log(CANCELLATIONS, f"cancelled {venue} (pid {os.getpid()})")
    return f"Booking at {venue} cancelled."


def _log(path: Path, line: str) -> None:
    # fsync, so the line is on disk even if the process is killed right after.
    with path.open("a") as f:
        f.write(line + "\n")
        f.flush()
        os.fsync(f.fileno())
