from __future__ import annotations

import atexit
import threading
from collections import deque
from typing import Any

import httpx

from .config import get_config


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
        from .redaction import _config as _r_cfg
        from .redaction import _redact_dict

        if _r_cfg.get("enabled"):
            batch = [_scrub_event(event, _redact_dict) for event in batch]

        cfg = get_config()
        if not cfg.api_key or not cfg.enabled:
            return
        url = f"{cfg.api_url}/v1/events/ingest"
        headers = {
            "Authorization": f"Bearer {cfg.api_key}",
            "Content-Type": "application/json",
        }
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


def _scrub_event(event: dict[str, Any], redact_dict: Any) -> dict[str, Any]:
    result = dict(event)
    if result.get("payload") is not None:
        result["payload"] = redact_dict(result["payload"])
    email = result.get("end_user_email")
    if isinstance(email, str) and email:
        from .redaction import redact as _redact

        result["end_user_email"] = _redact(email)
    return result


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
    _capture_local(event)


# ---------------------------------------------------------------------------
# Local capture (Phase 6.1 — capture_io=True)
# ---------------------------------------------------------------------------

import json as _json  # noqa: E402 — placed after class defs to avoid circular import

_capture_lock = threading.Lock()
_capture_path: str | None = None


def set_capture_path(path: str) -> None:
    """Override the local capture file path (default: .jamjet-replay.jsonl)."""
    global _capture_path
    with _capture_lock:
        _capture_path = path


def _capture_local(event: dict[str, Any]) -> None:
    """Append event to .jamjet-replay.jsonl when capture_io=True."""
    cfg = get_config()
    if not cfg.capture_io:
        return
    line = _json.dumps(event, ensure_ascii=False) + "\n"
    # Hold lock across both path read and write so concurrent emit() calls
    # never interleave partial lines in the cassette file.
    try:
        with _capture_lock:
            path = _capture_path or ".jamjet-replay.jsonl"
            with open(path, "a", encoding="utf-8") as fh:
                fh.write(line)
    except OSError:
        pass  # Never crash user code over local capture failures.
