"""Session abstraction + persistent SessionStore.

A Session is a stable user-owned conversation thread that persists across
runs.  The user ``session.id`` is DISTINCT from the engine's internal
``execution_id``/segment-lineage id (``latest_execution_id``).

Storage: a single SQLite file at ``~/.jamjet/sessions.db`` (one row per
session), reusing the same ``~/.jamjet/`` directory convention as the
CheckpointStore.  All public methods are synchronous; no async overhead
for simple session state.
"""

from __future__ import annotations

import json
import sqlite3
import uuid
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

_SCHEMA = """
CREATE TABLE IF NOT EXISTS sessions (
    session_id          TEXT PRIMARY KEY,
    messages_json       TEXT NOT NULL DEFAULT '[]',
    latest_execution_id TEXT,
    metadata_json       TEXT NOT NULL DEFAULT '{}',
    updated_at          TEXT NOT NULL
);
"""

_DEFAULT_DB_PATH = Path.home() / ".jamjet" / "sessions.db"


@dataclass
class Session:
    """A persistent conversation thread.

    Attributes:
        id: Stable user-assigned or auto-generated session identifier.
            This is NOT the engine's internal ``execution_id``; the two
            are explicitly separate (see ``latest_execution_id``).
        messages: The running conversation thread — a list of
            ``{"role": str, "content": str}`` dicts, the same shape that
            ``build_initial_state`` emits.
        latest_execution_id: The engine's internal lineage id for the most
            recent run started under this session.  Distinct from ``id``.
        metadata: Arbitrary caller-supplied key/value data.
    """

    id: str
    messages: list[dict] = field(default_factory=list)
    latest_execution_id: str | None = None
    metadata: dict = field(default_factory=dict)

    def append_message(self, role: str, content: str) -> None:
        """Append a ``{"role", "content"}`` dict to the message thread."""
        self.messages.append({"role": role, "content": content})


class SessionStore:
    """SQLite-backed persistent store for :class:`Session` objects.

    Uses a single shared DB file (default ``~/.jamjet/sessions.db``), one
    row per session.  Multiple ``SessionStore`` instances pointing at the
    same path share the same data — a fresh instance sees sessions saved by
    a previous instance (survives restart).

    Args:
        path: Path to the SQLite database file.  ``None`` uses the default
              ``~/.jamjet/sessions.db``.
    """

    def __init__(self, path: str | None = None) -> None:
        db_path = Path(path) if path is not None else _DEFAULT_DB_PATH
        db_path.parent.mkdir(parents=True, exist_ok=True)
        self._db_path = str(db_path)
        self._init_db()

    # ------------------------------------------------------------------
    # Internal
    # ------------------------------------------------------------------

    def _init_db(self) -> None:
        with sqlite3.connect(self._db_path) as conn:
            conn.executescript(_SCHEMA)
            conn.commit()

    @staticmethod
    def _now() -> str:
        return datetime.now(UTC).isoformat()

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def create(self, id: str | None = None) -> Session:
        """Create and persist a new empty :class:`Session`.

        Args:
            id: Optional session id.  If ``None`` a UUID4 is generated.

        Returns:
            The freshly-created :class:`Session` (already saved).
        """
        session = Session(id=id if id is not None else str(uuid.uuid4()))
        self.save(session)
        return session

    def load(self, id: str) -> Session | None:
        """Load a :class:`Session` by id.

        Returns:
            The :class:`Session`, or ``None`` if no session with that id
            exists in the store.
        """
        with sqlite3.connect(self._db_path) as conn:
            cur = conn.execute(
                "SELECT session_id, messages_json, latest_execution_id, metadata_json"
                " FROM sessions WHERE session_id = ?",
                (id,),
            )
            row = cur.fetchone()
        if row is None:
            return None
        return Session(
            id=row[0],
            messages=json.loads(row[1]),
            latest_execution_id=row[2],
            metadata=json.loads(row[3]),
        )

    def save(self, session: Session) -> None:
        """Upsert a :class:`Session` (persist messages, latest_execution_id, metadata).

        Idempotent — calling ``save`` twice with the same session updates
        the existing row.
        """
        with sqlite3.connect(self._db_path) as conn:
            conn.execute(
                """INSERT OR REPLACE INTO sessions
                   (session_id, messages_json, latest_execution_id, metadata_json, updated_at)
                   VALUES (?, ?, ?, ?, ?)""",
                (
                    session.id,
                    json.dumps(session.messages),
                    session.latest_execution_id,
                    json.dumps(session.metadata),
                    self._now(),
                ),
            )
            conn.commit()

    def list(self) -> list[str]:
        """Return all known session ids ordered by last-updated time."""
        with sqlite3.connect(self._db_path) as conn:
            cur = conn.execute("SELECT session_id FROM sessions ORDER BY updated_at")
            return [row[0] for row in cur.fetchall()]
