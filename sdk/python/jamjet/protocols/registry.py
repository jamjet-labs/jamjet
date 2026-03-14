"""
Protocol adapter registry.

Mirrors the Rust ``ProtocolRegistry`` in ``runtime/protocols/src/registry.rs``.
Register adapters by name and optional URL prefixes, then look them up by
either mechanism.
"""

from __future__ import annotations

from jamjet.protocols.adapter import ProtocolAdapter


class ProtocolRegistry:
    """A registry of named protocol adapters with URL-prefix dispatch."""

    def __init__(self) -> None:
        self._adapters: dict[str, ProtocolAdapter] = {}
        self._url_prefixes: list[tuple[str, str]] = []  # (prefix, protocol_name)

    def register(
        self,
        protocol_name: str,
        adapter: ProtocolAdapter,
        url_prefixes: list[str] | None = None,
    ) -> None:
        """Register an adapter under *protocol_name*.

        Optionally bind URL prefixes for automatic dispatch via
        :meth:`adapter_for_url`.
        """
        for prefix in url_prefixes or []:
            self._url_prefixes.append((prefix, protocol_name))
        self._adapters[protocol_name] = adapter

    def adapter(self, protocol_name: str) -> ProtocolAdapter | None:
        """Look up an adapter by protocol name."""
        return self._adapters.get(protocol_name)

    def adapter_for_url(self, url: str) -> ProtocolAdapter | None:
        """Look up an adapter by URL — longest matching prefix wins."""
        candidates = [(prefix, name) for prefix, name in self._url_prefixes if url.startswith(prefix)]
        if not candidates:
            return None
        # Longest prefix first.
        candidates.sort(key=lambda c: len(c[0]), reverse=True)
        _, proto = candidates[0]
        return self._adapters.get(proto)

    def protocols(self) -> list[str]:
        """All registered protocol names."""
        return list(self._adapters.keys())

    def __repr__(self) -> str:
        return f"ProtocolRegistry(protocols={self.protocols()})"


# ── Module-level singleton ───────────────────────────────────────────────────

_DEFAULT_REGISTRY = ProtocolRegistry()


def get_registry() -> ProtocolRegistry:
    """Return the module-level default protocol registry."""
    return _DEFAULT_REGISTRY
