"""Session abstraction + persistent SessionStore.

A Session is a stable user-owned conversation thread that persists across
runs.  The user ``session.id`` is DISTINCT from the engine's internal
``execution_id``/segment-lineage id (``latest_execution_id``).

Storage: a single SQLite file at ``~/.jamjet/sessions.db`` (one row per
session), reusing the same ``~/.jamjet/`` directory convention as the
CheckpointStore.  All public methods are synchronous; no async overhead
for simple session state.

T4-2 helpers
------------
``seed_messages_for_run`` and ``persist_session_turn`` are the SHARED
carried-state contract used by BOTH ``Agent.run()`` (in-process) and
``Agent.run_durable()`` (durable engine path) to ensure a session thread is
seeded and persisted identically regardless of which run path is used.
"""

from __future__ import annotations

import json
import sqlite3
import uuid
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from jamjet.agents.agent import Agent, AgentResult
    from jamjet.agents.artifacts import ArtifactStore

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
            ``{"role": str, "content": str}`` dicts.  System messages are
            **not** stored here; they are always re-injected from the
            agent's current ``instructions`` at seed time.

            Persistence asymmetry (I2 — the documented contract).  What lands
            in this thread depends on which run path produced the turn:

            - ``Agent.run()`` (in-process) persists the user prompt + the
              FINAL assistant output for the turn.  Strategy runners own their
              multi-phase message lists internally and do not surface a single
              tool-interleaved thread, so intermediate tool turns are NOT
              stored.
            - ``Agent.run_durable()`` (durable engine) persists the FULL
              tool-interleaved ledger from the engine's terminal
              ``current_state["messages"]`` (assistant tool-call turns + tool
              results + final answer), system messages stripped.

            Both paths always persist the user prompt and the final assistant
            turn, so re-seeding never DROPS a user/assistant turn; durable
            additionally carries the intermediate tool turns.  Within a single
            path the thread is consistent across runs.
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

    def attach_client(self, client: Any) -> ArtifactStore:
        """Bind a :class:`~jamjet.client.JamjetClient` (or compatible object)
        so :attr:`artifacts` stores/fetches through *client*.

        Returns the :class:`~jamjet.agents.artifacts.ArtifactStore` now backing
        :attr:`artifacts`.
        """
        from jamjet.agents.artifacts import ArtifactStore

        store = ArtifactStore(client)
        self._artifacts = store
        return store

    @property
    def artifacts(self) -> ArtifactStore:
        """Artifact namespace for this session.

        ``await session.artifacts.put(data, media_type)`` stores bytes and
        returns an :class:`~jamjet.client.ArtifactRef`;
        ``await session.artifacts.get(hash)`` fetches them back.

        Requires a client: call :meth:`attach_client` first to bind a
        :class:`~jamjet.client.JamjetClient` (or a compatible stub in tests).
        Accessing this property before a client is attached raises
        ``RuntimeError`` — rather than silently defaulting to a
        ``http://localhost:7700`` runtime, which would mask a missing-client
        misconfiguration as confusing connection errors at call time.
        """
        store = getattr(self, "_artifacts", None)
        if store is None:
            raise RuntimeError(
                f"Session {self.id!r}: no artifact client attached. Call "
                "session.attach_client(JamjetClient(<runtime_url>)) before using "
                "session.artifacts (it stores/fetches content-addressed bytes "
                "through that client)."
            )
        return store

    async def run(
        self,
        agent: Agent,
        prompt: str,
        *,
        durable: bool = False,
        **kwargs: Any,
    ) -> AgentResult:
        """Ergonomic form: run *agent* on *prompt* continuing this session thread.

        Equivalent to ``agent.run(prompt, session=self)`` (or
        ``agent.run_durable`` when ``durable=True``).

        Args:
            agent: The :class:`~jamjet.agents.agent.Agent` to invoke.
            prompt: The new user turn to add to this session.
            durable: When ``True`` use :meth:`~jamjet.agents.agent.Agent.run_durable`
                     instead of :meth:`~jamjet.agents.agent.Agent.run`.
            **kwargs: Forwarded to the chosen run method (e.g. ``max_turns``,
                      ``runtime_url`` for ``run_durable``).

        Returns:
            :class:`~jamjet.agents.agent.AgentResult` — same shape as the
            direct ``agent.run()`` / ``agent.run_durable()`` result.
        """
        if durable:
            return await agent.run_durable(prompt, session=self, **kwargs)
        return await agent.run(prompt, session=self, **kwargs)


class SessionStore:
    """SQLite-backed persistent store for :class:`Session` objects.

    Uses a single shared DB file (default ``~/.jamjet/sessions.db``), one
    row per session.  Multiple ``SessionStore`` instances pointing at the
    same path share the same data — a fresh instance sees sessions saved by
    a previous instance (survives restart).

    Concurrency contract (v1): SINGLE WRITER PER SESSION ID.  :meth:`save` is a
    whole-row ``INSERT OR REPLACE`` (last-writer-wins), so two runs racing on the
    SAME ``session.id`` would clobber each other's thread — the loser's turn is
    lost.  Concurrent runs on ONE session are NOT supported in v1; serialize them
    (or use distinct session ids).  Different session ids are independent rows and
    are safe to run concurrently.

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
        """Get-or-create a :class:`Session` by id (never clobbers an existing one).

        :meth:`save` is an ``INSERT OR REPLACE``, so creating over an existing id
        would WIPE that session's thread + metadata.  To avoid silent data loss
        ``create`` is get-or-create: if a session with *id* already exists it is
        loaded and RETURNED UNCHANGED (no overwrite).  Only a genuinely new id is
        persisted as an empty session.  To deliberately reset an existing session,
        call :meth:`save` with a fresh :class:`Session` (the explicit overwrite).

        Args:
            id: Optional session id.  If ``None`` a UUID4 is generated (always new).

        Returns:
            The existing :class:`Session` if *id* is already known, otherwise a
            freshly-created empty one (already saved).
        """
        if id is not None:
            existing = self.load(id)
            if existing is not None:
                return existing
        session = Session(id=id if id is not None else str(uuid.uuid4()))
        self.save(session)
        # Tag the originating store so a later run persists back HERE, not to the
        # agent's default store (see Agent._store_for_session).
        session._store = self
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
        session = Session(
            id=row[0],
            messages=json.loads(row[1]),
            latest_execution_id=row[2],
            metadata=json.loads(row[3]),
        )
        # Tag the originating store so a run that mutates this session persists it
        # back HERE, not to the agent's default store (see Agent._store_for_session).
        session._store = self
        return session

    def save(self, session: Session) -> None:
        """Upsert a :class:`Session` (persist messages, latest_execution_id, metadata).

        Idempotent — calling ``save`` twice with the same session updates
        the existing row.

        Last-writer-wins: this replaces the whole row, so it assumes a SINGLE
        WRITER PER SESSION ID (see the class docstring).  Two concurrent runs on
        the same session would overwrite each other's thread; serialize runs on a
        given session id in v1.
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


# ---------------------------------------------------------------------------
# T4-2 shared carried-state helpers
# ---------------------------------------------------------------------------
# These two functions are the SINGLE source of truth for how a session thread
# is seeded into a run and how the completed turn is persisted back.  Both
# ``Agent.run()`` (in-process) and ``Agent.run_durable()`` (durable engine)
# call the same helpers so the thread is consistent across the two paths.


def seed_messages_for_run(
    session: Session | None,
    system_instructions: str,
    prompt: str,
) -> list[dict[str, Any]]:
    """Build the initial messages list for a run.

    When *session* is ``None`` (or empty), returns the default scratch seed::

        [{"role": "system", "content": system_instructions},
         {"role": "user",   "content": prompt}]

    When *session* is provided, returns the carried thread (prior turns) with
    the current agent's system instruction and the new user prompt appended::

        [{"role": "system",    "content": system_instructions},   # always fresh
         {"role": "user",      "content": "<turn 1>"},            # from session
         {"role": "assistant", "content": "<reply 1>"},           # from session
         ...                                                       # more turns
         {"role": "user",      "content": prompt}]                # new turn

    System messages stored in *session.messages* are stripped here; the
    current agent's instructions always win as the single system message.
    This keeps session threads portable across agent versions.

    Args:
        session: The :class:`Session` carrying prior turns, or ``None`` for a
                 fresh run (default behaviour, no change to callers that omit
                 ``session=``).
        system_instructions: The agent's current ``instructions`` string.
        prompt: The new user turn to add.

    Returns:
        The ``messages`` list ready to pass to a model / strategy runner.
    """
    msgs: list[dict[str, Any]] = []
    if system_instructions:
        msgs.append({"role": "system", "content": system_instructions})

    if session is not None and session.messages:
        # Re-inject prior turns, skipping any stale system messages.
        for m in session.messages:
            if m.get("role") != "system":
                msgs.append(dict(m))

    msgs.append({"role": "user", "content": prompt})
    return msgs


def persist_session_turn(
    session: Session,
    prompt: str,
    output: str,
    execution_id: str | None,
    full_messages: list[dict[str, Any]] | None,
    store: SessionStore,
) -> None:
    """Append the completed turn to *session* and persist via *store*.

    Called after BOTH ``Agent.run()`` and ``Agent.run_durable()`` so the
    session thread is updated regardless of which path ran.

    Persistence asymmetry (I2 — the documented contract).  The two paths persist
    threads of DIFFERENT granularity, and this asymmetry is intentional and
    contractual (see :class:`Session`'s ``messages`` docstring):

    - When *full_messages* is provided (the durable engine's
      ``current_state["messages"]``), the session thread is REPLACED with those
      messages minus any system messages — the full, tool-interleaved ledger
      (assistant tool-call turns + tool results + final answer).
    - When *full_messages* is ``None`` (in-process path; strategy runners own
      their multi-phase message lists internally and do not surface a single
      tool-interleaved thread), the turn is reconstructed from *prompt* +
      *output* and appended to the existing thread — user prompt + final
      assistant output only, no intermediate tool turns.

    Both paths always persist the user prompt and the final assistant turn, so
    re-seeding the thread on the next run never drops a user/assistant turn;
    durable additionally carries the intermediate tool turns.  Aligning the
    in-process path to also emit the tool-interleaved thread would require every
    strategy runner to surface its internal messages (tracked as a follow-up);
    until then this documented asymmetry IS the contract.

    Args:
        session: The :class:`Session` to update in-place and persist.
        prompt: The user prompt that was just run.
        output: The assistant's final output for this turn.
        execution_id: The engine's ``execution_id`` for the run (may be
                      ``None`` for the in-process path which generates one
                      internally).
        full_messages: The full ``messages`` list from the durable engine's
                       terminal state, or ``None`` for the in-process path.
        store: :class:`SessionStore` to call ``save()`` on after updating.
    """
    if full_messages is not None:
        # Durable path: replace thread with full message ledger, strip system.
        session.messages = [m for m in full_messages if m.get("role") != "system"]
    else:
        # In-process path: append the new user+assistant turn.
        session.append_message("user", prompt)
        session.append_message("assistant", output)

    if execution_id is not None:
        session.latest_execution_id = execution_id

    store.save(session)
