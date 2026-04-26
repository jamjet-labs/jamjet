from __future__ import annotations

import threading
from dataclasses import dataclass


@dataclass
class CloudConfig:
    """Global configuration for the JamJet Cloud SDK."""

    api_key: str | None = None
    project: str = "default"
    api_url: str = "https://api.jamjet.dev"
    capture_io: bool = False
    auto_patch: bool = True
    flush_interval: float = 5.0
    flush_size: int = 50
    enabled: bool = True


_lock = threading.Lock()
_config: CloudConfig = CloudConfig()


def get_config() -> CloudConfig:
    """Return the current global config."""
    return _config


def set_config(**kwargs: object) -> CloudConfig:
    """Update global config fields and return the config."""
    global _config
    with _lock:
        for key, value in kwargs.items():
            if not hasattr(_config, key):
                raise AttributeError(f"CloudConfig has no field '{key}'")
            setattr(_config, key, value)
    return _config
