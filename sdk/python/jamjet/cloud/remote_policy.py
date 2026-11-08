"""Policies managed in JamJet Cloud, applied by the SDK.

Cloud has no endpoint that returns its rules with everything evaluation needs:
``GET /v1/policies?kind=...`` lists active rules (kind, pattern, environments)
but not their agent scope, cost band or dry-run flag, and
``POST /v1/policy/evaluate`` decides one tool at a time. So the SDK uses both:

1. The list of ``block`` and ``require_approval`` rules is fetched once (and
   again on :func:`jamjet.cloud.refresh_policies`). Other kinds cannot make a
   decision stricter, so they are not needed.
2. A tool name that one of those rules could match is sent to
   ``/v1/policy/evaluate``, and Cloud's answer is used (cached per tool,
   agent and environment until the next refresh). That keeps dry-run rules,
   agent-scoped rules and Cloud's own pattern matching exactly as Cloud
   applies them. Tool names no restrictive rule could match never leave the
   process.

Failures are logged, never swallowed. When the user asked for remote policies
(``configure(remote_policies=True)``) and the list cannot be loaded, every tool
is treated as blocked until a refresh succeeds. When the evaluate call fails,
the strictest rule that matched locally applies.
"""

from __future__ import annotations

import logging
import random
import re
import threading
from dataclasses import dataclass, field
from typing import Any

import httpx

from .models import PolicyDecision

logger = logging.getLogger("jamjet.cloud")

# Pattern reported on decisions made because remote policies are unavailable.
UNAVAILABLE_PATTERN = "<remote policies unavailable>"

_RESTRICTIVE_KINDS = ("block", "require_approval")
_RANK = {"allow": 0, "audit": 1, "require_approval": 2, "block": 3}


def rank(policy_kind: str) -> int:
    """How strict a decision kind is; higher wins when sources disagree."""
    return _RANK.get(policy_kind, 0)


def _cloud_glob_match(pattern: str, value: str) -> bool:
    """Cloud's matcher (jamjet-cloud api/src/routes/policy_eval.rs ``glob_match``)."""
    if pattern == "*":
        return True
    if pattern.endswith(".*"):
        prefix = pattern[:-2]
        return value == prefix or value.startswith(prefix + ".")
    if pattern.endswith("*"):
        return value.startswith(pattern[:-1])
    if pattern.startswith("*"):
        return value.endswith(pattern[1:])
    return pattern == value


@dataclass(frozen=True)
class RemoteRule:
    kind: str
    pattern: str
    environments: tuple[str, ...]
    regex: re.Pattern[str] = field(compare=False)


class RemotePolicyError(Exception):
    """Cloud's policies could not be loaded or read."""


class RemotePolicies:
    """Cloud-managed block / require_approval rules for one configured process."""

    def __init__(self, api_key: str, api_url: str, environment: str | None, required: bool) -> None:
        self.api_key = api_key
        self.api_url = api_url.rstrip("/")
        self.environment = environment
        self.required = required
        self._lock = threading.Lock()
        self._rules: list[RemoteRule] = []
        self._loaded = False
        self._unavailable = False
        self._decisions: dict[tuple[str, str, str | None], str | None] = {}
        self._stopped = threading.Event()

    # -- loading -----------------------------------------------------------

    def _headers(self) -> dict[str, str]:
        return {"Authorization": f"Bearer {self.api_key}"}

    def _fetch(self) -> list[RemoteRule]:
        from .policy import glob_to_regex  # policy.py imports this module

        rules: list[RemoteRule] = []
        for kind in _RESTRICTIVE_KINDS:
            try:
                resp = httpx.get(
                    f"{self.api_url}/v1/policies",
                    params={"kind": kind},
                    headers=self._headers(),
                    timeout=10,
                )
            except httpx.HTTPError as exc:
                raise RemotePolicyError(f"GET /v1/policies?kind={kind} failed: {exc}") from exc
            if resp.status_code != 200:
                raise RemotePolicyError(f"GET /v1/policies?kind={kind} returned HTTP {resp.status_code}")
            try:
                body: Any = resp.json()
            except ValueError as exc:
                raise RemotePolicyError(f"GET /v1/policies?kind={kind} returned invalid JSON") from exc
            items = body.get("policies") if isinstance(body, dict) else None
            if not isinstance(items, list):
                raise RemotePolicyError(f"GET /v1/policies?kind={kind} returned no `policies` list")
            for item in items:
                pattern = item.get("pattern") if isinstance(item, dict) else None
                if not isinstance(pattern, str) or not pattern:
                    raise RemotePolicyError(f"GET /v1/policies?kind={kind} returned a rule without a pattern")
                envs = item.get("environments") or []
                if not isinstance(envs, list) or not all(isinstance(e, str) for e in envs):
                    raise RemotePolicyError(f"GET /v1/policies?kind={kind} returned malformed environments")
                rules.append(
                    RemoteRule(kind=kind, pattern=pattern, environments=tuple(envs), regex=glob_to_regex(pattern))
                )
        return rules

    def load(self) -> bool:
        """Fetch the rules. Returns True on success; logs and returns False on failure.

        A failure after an earlier success keeps the last rules that loaded.
        """
        try:
            rules = self._fetch()
        except RemotePolicyError as exc:
            with self._lock:
                had_rules = self._loaded
                if self.required and not had_rules:
                    # Fail closed: the user asked for Cloud's policies and they
                    # have never been read, so nothing is assumed allowed.
                    self._unavailable = True
            if had_rules:
                logger.warning(
                    "jamjet.cloud: reloading Cloud policies failed (%s); keeping the last rules loaded.", exc
                )
            elif self.required:
                logger.error(
                    "jamjet.cloud: could not load Cloud policies (%s). Every tool call is blocked "
                    "until jamjet.cloud.refresh_policies() succeeds.",
                    exc,
                )
            else:
                logger.warning(
                    "jamjet.cloud: could not load Cloud policies (%s). Only local policies apply. "
                    "Pass remote_policies=True to configure() to fail closed instead.",
                    exc,
                )
            return False
        with self._lock:
            self._rules = rules
            self._decisions.clear()
            self._loaded = True
            self._unavailable = False
        logger.info("jamjet.cloud: loaded %d Cloud policy rules", len(rules))
        return True

    def start_refresh(self, interval_seconds: float, jitter: float = 0.1) -> None:
        """Reload every ``interval_seconds`` (plus or minus ``jitter``) on a daemon
        thread until :meth:`stop`. A non-positive interval does nothing."""
        if interval_seconds <= 0:
            return

        def run() -> None:
            while not self._stopped.wait(interval_seconds * (1 + random.uniform(-jitter, jitter))):
                self.load()

        threading.Thread(target=run, daemon=True, name="jamjet-policy-refresh").start()

    def stop(self) -> None:
        """Stop the refresh timer."""
        self._stopped.set()

    def active(self) -> bool:
        """True when remote rules can affect a decision."""
        with self._lock:
            return self._unavailable or bool(self._rules)

    # -- deciding ----------------------------------------------------------

    def _candidates(self, tool_name: str) -> list[RemoteRule]:
        out = []
        for rule in self._rules:
            # Environment scope as Cloud applies it: an empty list is every
            # environment, and a process with no environment set is checked
            # against every rule.
            if rule.environments and self.environment and self.environment not in rule.environments:
                continue
            # Superset of what Cloud could match, so Cloud gets asked whenever
            # it might say no.
            if _cloud_glob_match(rule.pattern, tool_name) or rule.regex.fullmatch(tool_name):
                out.append(rule)
        return out

    def _ask_cloud(self, tool_name: str, agent: str) -> str | None:
        body: dict[str, Any] = {"tool": tool_name, "agent": agent}
        if self.environment:
            body["environment"] = self.environment
        resp = httpx.post(f"{self.api_url}/v1/policy/evaluate", json=body, headers=self._headers(), timeout=10)
        if resp.status_code != 200:
            raise RemotePolicyError(f"POST /v1/policy/evaluate returned HTTP {resp.status_code}")
        decision = resp.json().get("decision")
        if decision in ("block", "require_approval"):
            return str(decision)
        if decision in ("allow", "audit_only"):
            return None
        raise RemotePolicyError(f"POST /v1/policy/evaluate returned unknown decision {decision!r}")

    def decide(self, tool_name: str, agent: str) -> PolicyDecision | None:
        """Cloud's decision for ``tool_name``, or None when Cloud does not restrict it."""
        with self._lock:
            if self._unavailable:
                return PolicyDecision(
                    blocked=True, policy_kind="block", pattern=UNAVAILABLE_PATTERN, tool_name=tool_name
                )
            candidates = self._candidates(tool_name)
            key = (tool_name, agent, self.environment)
            cached = key in self._decisions
            kind = self._decisions.get(key)
        if not candidates:
            return None
        strictest = max(candidates, key=lambda r: rank(r.kind))
        if not cached:
            try:
                kind = self._ask_cloud(tool_name, agent)
            except (httpx.HTTPError, ValueError, AttributeError, RemotePolicyError) as exc:
                logger.warning(
                    "jamjet.cloud: Cloud policy check for %r failed (%s); applying %s rule %r.",
                    tool_name,
                    exc,
                    strictest.kind,
                    strictest.pattern,
                )
                kind = strictest.kind  # fail closed; not cached, so the next call asks again
            else:
                with self._lock:
                    self._decisions[key] = kind
        if kind is None:
            return None
        return PolicyDecision(
            blocked=kind == "block",
            policy_kind=kind,
            pattern=strictest.pattern,
            tool_name=tool_name,
        )
