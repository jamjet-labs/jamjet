"""
Cache backends for @durable.

The default backend is SqliteCache — a file-backed key/value store using
SQLite in WAL mode for safe concurrent reads. The interface is intentionally
small: get(key) and put(key, value). Future backends (Engram-native, Redis,
Postgres) implement the same Protocol.
"""

from __future__ import annotations

import sqlite3
import threading
from collections.abc import Callable
from pathlib import Path
from typing import Any, Protocol

from jamjet.durable.serialization import dumps, loads


class Cache(Protocol):
    """A keyed cache for @durable function results."""

    def get(self, key: str) -> Any | None:
        """Return the cached value for `key`, or None if missing."""
        ...

    def put(self, key: str, value: Any) -> None:
        """Store `value` under `key`. Overwrites any prior value."""
        ...

    def get_or_compute(self, key: str, compute: Callable[[], Any]) -> Any:
        """
        Atomic get-or-set: if `key` is cached, return it; otherwise call
        `compute()`, store the result under `key`, and return it. Concurrent
        callers requesting the same key must serialize so that `compute()` is
        invoked at most once per key.
        """
        ...


class SqliteCache:
    """File-backed cache using SQLite in WAL mode."""

    _SCHEMA = """
    CREATE TABLE IF NOT EXISTS durable_cache (
        key TEXT PRIMARY KEY,
        value BLOB NOT NULL,
        created_at REAL NOT NULL DEFAULT (julianday('now'))
    );
    """

    def __init__(self, path: str | Path) -> None:
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self._lock = threading.Lock()
        with self._connect() as conn:
            conn.executescript(self._SCHEMA)
            conn.execute("PRAGMA journal_mode=WAL;")

    def _connect(self) -> sqlite3.Connection:
        return sqlite3.connect(self.path, isolation_level=None, timeout=5.0)

    def get(self, key: str) -> Any | None:
        with self._connect() as conn:
            row = conn.execute("SELECT value FROM durable_cache WHERE key = ?", (key,)).fetchone()
        if row is None:
            return None
        return loads(row[0])

    def put(self, key: str, value: Any) -> None:
        # dumps() raises TypeError if value isn't picklable.
        blob = dumps(value)
        with self._lock, self._connect() as conn:
            conn.execute(
                "INSERT OR REPLACE INTO durable_cache (key, value) VALUES (?, ?)",
                (key, blob),
            )

    def get_or_compute(self, key: str, compute: Callable[[], Any]) -> Any:
        """
        Atomic get-or-set: if key is cached, return it; otherwise call compute(),
        store the result under key, and return it. Uses a SQLite transaction so
        concurrent callers within the same execution_id serialize on the same key.
        """
        with self._lock, self._connect() as conn:
            conn.execute("BEGIN IMMEDIATE")
            try:
                row = conn.execute(
                    "SELECT value FROM durable_cache WHERE key = ?", (key,)
                ).fetchone()
                if row is not None:
                    return loads(row[0])
                value = compute()
                blob = dumps(value)
                conn.execute(
                    "INSERT OR REPLACE INTO durable_cache (key, value) VALUES (?, ?)",
                    (key, blob),
                )
                return value
            except Exception:
                conn.execute("ROLLBACK")
                raise
