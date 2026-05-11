"""SQLite-backed step checkpoint log. One DB file per execution_id."""

from __future__ import annotations

from datetime import datetime
from pathlib import Path

import aiosqlite

from jamjet.runtime.types import StepRecord

_SCHEMA = """
CREATE TABLE IF NOT EXISTS steps (
    step_id      TEXT PRIMARY KEY,
    input_hash   TEXT NOT NULL,
    input_json   TEXT NOT NULL,
    output_json  TEXT,
    status       TEXT NOT NULL,
    error        TEXT,
    started_at   TIMESTAMP,
    completed_at TIMESTAMP,
    duration_ms  REAL
);
CREATE TABLE IF NOT EXISTS seeds (
    kind         TEXT PRIMARY KEY,
    seed_value   TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS meta (
    key          TEXT PRIMARY KEY,
    value        TEXT NOT NULL
);
"""


class CheckpointStore:
    def __init__(self, db_path: Path, *, ir_version: str, spec_hash: str) -> None:
        self.db_path = db_path
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._ir_version = ir_version
        self._spec_hash = spec_hash

    async def init(self) -> None:
        async with aiosqlite.connect(self.db_path) as conn:
            await conn.executescript(_SCHEMA)
            await conn.execute(
                "INSERT OR IGNORE INTO meta(key, value) VALUES (?, ?)",
                ("ir_version", self._ir_version),
            )
            await conn.execute(
                "INSERT OR IGNORE INTO meta(key, value) VALUES (?, ?)",
                ("spec_hash", self._spec_hash),
            )
            await conn.execute(
                "INSERT OR IGNORE INTO meta(key, value) VALUES (?, ?)",
                ("started_at", datetime.now().isoformat()),
            )
            await conn.commit()

    async def start_step(self, step_id: str, *, input_hash: str, input_json: str) -> None:
        async with aiosqlite.connect(self.db_path) as conn:
            await conn.execute(
                """INSERT OR REPLACE INTO steps
                   (step_id, input_hash, input_json, status, started_at)
                   VALUES (?, ?, ?, 'running', ?)""",
                (step_id, input_hash, input_json, datetime.now().isoformat()),
            )
            await conn.commit()

    async def complete_step(self, step_id: str, *, output_json: str, duration_ms: float) -> None:
        async with aiosqlite.connect(self.db_path) as conn:
            await conn.execute(
                """UPDATE steps SET status='completed', output_json=?, duration_ms=?, completed_at=?
                   WHERE step_id=?""",
                (output_json, duration_ms, datetime.now().isoformat(), step_id),
            )
            await conn.commit()

    async def fail_step(self, step_id: str, *, error: str) -> None:
        async with aiosqlite.connect(self.db_path) as conn:
            await conn.execute(
                "UPDATE steps SET status='failed', error=?, completed_at=? WHERE step_id=?",
                (error, datetime.now().isoformat(), step_id),
            )
            await conn.commit()

    async def get_step(self, step_id: str) -> StepRecord | None:
        async with aiosqlite.connect(self.db_path) as conn:
            cur = await conn.execute(
                "SELECT step_id, input_hash, output_json, status, error, duration_ms FROM steps WHERE step_id=?",
                (step_id,),
            )
            row = await cur.fetchone()
            if row is None:
                return None
            return StepRecord(
                step_id=row[0],
                input_hash=row[1],
                output_json=row[2],
                status=row[3],
                error=row[4],
                duration_ms=row[5],
            )

    async def get_step_if_match(self, step_id: str, *, input_hash: str) -> StepRecord | None:
        rec = await self.get_step(step_id)
        if rec is None or rec.status != "completed" or rec.input_hash != input_hash:
            return None
        return rec

    async def list_incomplete_steps(self) -> list[StepRecord]:
        async with aiosqlite.connect(self.db_path) as conn:
            cur = await conn.execute(
                "SELECT step_id, input_hash, output_json, status, error, duration_ms"
                " FROM steps WHERE status != 'completed' ORDER BY started_at",
            )
            rows = await cur.fetchall()
            return [
                StepRecord(
                    step_id=r[0],
                    input_hash=r[1],
                    output_json=r[2],
                    status=r[3],
                    error=r[4],
                    duration_ms=r[5],
                )
                for r in rows
            ]

    async def set_seed(self, kind: str, value: str) -> None:
        async with aiosqlite.connect(self.db_path) as conn:
            await conn.execute(
                "INSERT OR REPLACE INTO seeds(kind, seed_value) VALUES (?, ?)",
                (kind, value),
            )
            await conn.commit()

    async def get_seed(self, kind: str) -> str | None:
        async with aiosqlite.connect(self.db_path) as conn:
            cur = await conn.execute("SELECT seed_value FROM seeds WHERE kind=?", (kind,))
            row = await cur.fetchone()
            return row[0] if row else None
