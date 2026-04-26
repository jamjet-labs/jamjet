from __future__ import annotations

import atexit
import threading
from collections import deque
from typing import Any

import httpx

from .config import get_config


def _maybe_mint_aip_token() -> str | None:
    """Return a freshly-minted AIP token, or None if AIP isn't configured.
    Lazy import keeps the cryptography dep optional — no AIP, no overhead."""
    try:
        from .aip import mint_for_event
    except ImportError:
        return None
    return mint_for_event()


class EventQueue:
    """Thread-safe event queue with background batch flushing."""

    def __init__(self, flush_interval: float = 5.0, flush_size: int = 50, max_buffer: int = 10000) -> None:
        self._queue: deque[dict[str, Any]] = deque(maxlen=max_buffer)
        self._lock = threading.Lock()
        self._flush_interval = flush_interval
        self._flush_size = flush_size
        self._max_buffer = max_buffer
        self._stop_event = threading.Event()
        self._thread: threading.Thread | None = None
        self._consecutive_failures = 0
        self._max_retries = 5

    def start(self) -> None:
        """Start the background flush thread."""
        if self._thread is not None and self._thread.is_alive():
            return
        self._stop_event.clear()
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()
        atexit.register(self.stop)

    def stop(self) -> None:
        """Signal the flush thread to stop and do a final flush."""
        self._stop_event.set()
        if self._thread is not None and self._thread.is_alive():
            self._thread.join(timeout=10)
        self._flush()

    def push(self, event: dict[str, Any]) -> None:
        """Add an event to the queue. Triggers flush if batch size reached."""
        batch: list[dict[str, Any]] | None = None
        with self._lock:
            self._queue.append(event)
            if len(self._queue) >= self._flush_size:
                batch = self._drain()
        if batch is not None:
            self._send(batch)

    def _run(self) -> None:
        """Background thread loop: flush periodically."""
        while not self._stop_event.wait(timeout=self._flush_interval):
            self._flush()

    def _flush(self) -> None:
        """Drain queue and send batch."""
        with self._lock:
            batch = self._drain()
        if batch:
            self._send(batch)

    def _drain(self) -> list[dict[str, Any]]:
        """Drain all events from the queue. Caller must hold lock."""
        items = list(self._queue)
        self._queue.clear()
        return items

    def _send(self, batch: list[dict[str, Any]]) -> None:
        """POST a batch of events to the ingest endpoint."""
        cfg = get_config()
        if not cfg.api_key or not cfg.enabled:
            return
        url = f"{cfg.api_url}/v1/events/ingest"
        headers = {
            "Authorization": f"Bearer {cfg.api_key}",
            "Content-Type": "application/json",
        }
        # Attach a freshly-minted AIP token (Plan 5 Phase 1.6) when the
        # caller has registered an agent identity. One token is sufficient
        # for the whole batch — its TTL covers the network round-trip and
        # all events in the batch were emitted by the same process agent.
        token = _maybe_mint_aip_token()
        if token is not None:
            for event in batch:
                event.setdefault("aip_token", token)
        payload = {"project": cfg.project, "events": batch}
        try:
            resp = httpx.post(url, json=payload, headers=headers, timeout=10)
            resp.raise_for_status()
            self._consecutive_failures = 0
        except httpx.HTTPStatusError as e:
            if e.response.status_code >= 400 and e.response.status_code < 500:
                # Client error (bad request) — don't retry, drop the batch
                pass
            else:
                self._requeue_with_backoff(batch)
        except Exception:
            self._requeue_with_backoff(batch)

    def _requeue_with_backoff(self, batch: list[dict[str, Any]]) -> None:
        """Re-queue failed events with circuit breaker."""
        self._consecutive_failures += 1
        if self._consecutive_failures > self._max_retries:
            # Circuit open — drop events to prevent infinite retry
            return
        with self._lock:
            for event in reversed(batch):
                self._queue.appendleft(event)

    @property
    def pending(self) -> int:
        """Number of events waiting to be flushed."""
        with self._lock:
            return len(self._queue)


# ---------------------------------------------------------------------------
# Module-level singleton
# ---------------------------------------------------------------------------

_queue: EventQueue | None = None
_module_lock = threading.Lock()


def init_queue(flush_interval: float = 5.0, flush_size: int = 50) -> EventQueue:
    """Initialize and start the global event queue."""
    global _queue
    with _module_lock:
        if _queue is not None:
            _queue.stop()
        _queue = EventQueue(flush_interval=flush_interval, flush_size=flush_size)
        _queue.start()
    return _queue


def get_queue() -> EventQueue | None:
    """Return the global event queue (None if not initialized)."""
    return _queue


def emit(event: dict[str, Any]) -> None:
    """Push an event to the global queue. No-op if queue not initialized."""
    q = _queue
    if q is not None:
        q.push(event)
