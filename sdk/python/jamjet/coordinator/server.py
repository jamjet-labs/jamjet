"""HTTP adapter for coordinator strategies; requires ``jamjet[sidecar]``."""

from __future__ import annotations

from typing import Any

from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Route

from .default_strategy import DefaultCoordinatorStrategy
from .strategy import AgentCandidate, CoordinatorStrategy, DimensionScores, ScoringResult


class StrategyServer:
    """Expose discovery, scoring, and selection strategies over HTTP.

    Requests select a registered strategy with ``strategy_name``, defaulting to
    ``"default"``. The built-in default discovers no agents until a registry is
    supplied with ``set_registry()``. Construction configures routes; ``run()``
    starts the server.

    Attributes:
        host: Interface on which ``run()`` binds the server.
        port: TCP port used by ``run()``.
    """

    def __init__(self, host: str = "127.0.0.1", port: int = 4270) -> None:
        """Configure strategy and health routes without starting a listener.

        Args:
            host: Bind address, defaulting to the loopback interface.
            port: Bind port, defaulting to 4270.
        """
        self.host = host
        self.port = port
        self._registry = None
        self._strategies: dict[str, CoordinatorStrategy] = {
            "default": DefaultCoordinatorStrategy(registry=None),
        }
        self._app = Starlette(
            routes=[
                Route("/coordinator/discover", self._handle_discover, methods=["POST"]),
                Route("/coordinator/score", self._handle_score, methods=["POST"]),
                Route("/coordinator/decide", self._handle_decide, methods=["POST"]),
                Route("/health", self._handle_health, methods=["GET"]),
            ]
        )

    def register_strategy(self, name: str, strategy: CoordinatorStrategy) -> None:
        """Register or replace a strategy under the given request name.

        Args:
            name: Value clients send as ``strategy_name``.
            strategy: Strategy implementation. A non-None shared registry is
                assigned to the strategy's existing ``_registry`` attribute,
                when present.
        """
        self._strategies[name] = strategy
        if self._registry is not None and hasattr(strategy, "_registry"):
            strategy._registry = self._registry

    def set_registry(self, registry: Any) -> None:
        """Set the shared registry on existing strategies.

        Future registrations inherit it when it is not ``None``. Only strategies
        exposing a ``_registry`` attribute receive the object.

        Args:
            registry: Registry object. The default strategy expects an async
                ``list_agents()`` method returning agent mappings. Passing
                ``None`` disables discovery for that strategy.
        """
        self._registry = registry
        for strategy in self._strategies.values():
            if hasattr(strategy, "_registry"):
                strategy._registry = registry

    def _get_strategy(self, data: dict) -> CoordinatorStrategy:
        """Resolve ``strategy_name`` from a request, defaulting to ``"default"``.

        Args:
            data: Decoded request body.

        Returns:
            The registered strategy instance.

        Raises:
            ValueError: The requested strategy name is not registered.
        """
        name = data.get("strategy_name", "default")
        if name not in self._strategies:
            raise ValueError(f"Unknown strategy: {name}")
        return self._strategies[name]

    async def _handle_discover(self, request: Request) -> JSONResponse:
        """Handle discovery requests and serialize candidates and exclusions.

        Args:
            request: JSON request containing ``task`` and optional
                ``strategy_name``, ``required_skills``, ``preferred_skills``,
                ``trust_domain``, and ``context`` values.

        Returns:
            JSON with ``candidates`` and ``filtered_out``. Missing required fields or
            ``ValueError``/``KeyError`` from strategy lookup or discovery produce
            a 400 response containing ``error``. JSON decoding errors propagate.
        """
        data = await request.json()
        try:
            strategy = self._get_strategy(data)
            candidates, filtered = await strategy.discover(
                task=data["task"],
                required_skills=data.get("required_skills", []),
                preferred_skills=data.get("preferred_skills", []),
                trust_domain=data.get("trust_domain"),
                context=data.get("context", {}),
            )
            return JSONResponse(
                {
                    "candidates": [_candidate_to_dict(c) for c in candidates],
                    "filtered_out": filtered,
                }
            )
        except (ValueError, KeyError) as exc:
            return JSONResponse({"error": str(exc)}, status_code=400)

    async def _handle_score(self, request: Request) -> JSONResponse:
        """Handle scoring requests and serialize rankings and score spread.

        Args:
            request: JSON request containing ``task`` and optional
                ``strategy_name``, ``candidates``, ``weights``, and ``context``.
                Candidate mappings require ``uri``; other fields use defaults.

        Returns:
            JSON with ``rankings`` and ``spread``. Missing required fields or
            ``ValueError``/``KeyError`` during conversion or scoring produce a
            400 response containing ``error``. JSON decoding errors propagate.
        """
        data = await request.json()
        try:
            strategy = self._get_strategy(data)
            candidates = [_dict_to_candidate(c) for c in data.get("candidates", [])]
            rankings, spread = await strategy.score(
                task=data["task"],
                candidates=candidates,
                weights=data.get("weights", {}),
                context=data.get("context", {}),
            )
            return JSONResponse(
                {
                    "rankings": [
                        {"uri": r.agent_uri, "scores": _scores_to_dict(r.scores), "composite": r.composite}
                        for r in rankings
                    ],
                    "spread": spread,
                }
            )
        except (ValueError, KeyError) as exc:
            return JSONResponse({"error": str(exc)}, status_code=400)

    async def _handle_decide(self, request: Request) -> JSONResponse:
        """Handle final selection requests and serialize the strategy decision.

        Args:
            request: JSON request containing ``task`` and optional
                ``strategy_name``, ``top_candidates``, ``threshold`` (0.1),
                ``tiebreaker_model`` (empty string), and ``context``. Each
                candidate requires ``uri``; missing dimension scores default to
                0.5 and missing composites to zero.

        Returns:
            JSON with the selected URI, method, reasoning, confidence, rejected
            candidates, and tiebreaker usage. Missing required fields or
            ``ValueError``/``KeyError`` during conversion or selection produce a
            400 response containing ``error``. JSON decoding errors propagate.
        """
        data = await request.json()
        try:
            strategy = self._get_strategy(data)
            top = []
            for c in data.get("top_candidates", []):
                scores_data = c.get("scores", {})
                scores = DimensionScores(
                    capability_fit=scores_data.get("capability_fit", 0.5),
                    cost_fit=scores_data.get("cost_fit", 0.5),
                    latency_fit=scores_data.get("latency_fit", 0.5),
                    trust_compatibility=scores_data.get("trust_compatibility", 0.5),
                    historical_performance=scores_data.get("historical_performance", 0.5),
                )
                top.append(ScoringResult(agent_uri=c["uri"], scores=scores, composite=c.get("composite", 0)))
            decision = await strategy.decide(
                task=data["task"],
                top_candidates=top,
                threshold=data.get("threshold", 0.1),
                tiebreaker_model=data.get("tiebreaker_model", ""),
                context=data.get("context", {}),
            )
            return JSONResponse(
                {
                    "selected_uri": decision.selected_uri,
                    "method": decision.method,
                    "reasoning": decision.reasoning,
                    "confidence": decision.confidence,
                    "rejected": decision.rejected,
                    "tiebreaker_tokens": decision.tiebreaker_tokens,
                    "tiebreaker_cost": decision.tiebreaker_cost,
                }
            )
        except (ValueError, KeyError) as exc:
            return JSONResponse({"error": str(exc)}, status_code=400)

    async def _handle_health(self, request: Request) -> JSONResponse:
        """Return server liveness and registered names without calling strategies."""
        return JSONResponse({"status": "ok", "strategies": list(self._strategies.keys())})

    def run(self) -> None:
        """Run the HTTP server with Uvicorn, blocking until it stops.

        Install the ``jamjet[sidecar]`` extra for Starlette and Uvicorn.

        Raises:
            ImportError: Uvicorn is unavailable in the current environment.
        """
        import uvicorn

        uvicorn.run(self._app, host=self.host, port=self.port)


def _candidate_to_dict(c: AgentCandidate) -> dict:
    """Serialize all candidate fields used by the strategy HTTP API."""
    return {
        "uri": c.uri,
        "agent_card": c.agent_card,
        "skills": c.skills,
        "latency_class": c.latency_class,
        "cost_class": c.cost_class,
        "trust_domain": c.trust_domain,
        "reasoning_modes": c.reasoning_modes,
    }


def _dict_to_candidate(d: dict) -> AgentCandidate:
    """Read a candidate mapping, using empty or ``None`` optional defaults.

    Args:
        d: Candidate mapping containing ``uri`` and optional metadata.

    Returns:
        The corresponding candidate without validating its field values.

    Raises:
        KeyError: The mapping has no ``uri`` field.
    """
    return AgentCandidate(
        uri=d["uri"],
        agent_card=d.get("agent_card", {}),
        skills=d.get("skills", []),
        latency_class=d.get("latency_class"),
        cost_class=d.get("cost_class"),
        trust_domain=d.get("trust_domain"),
        reasoning_modes=d.get("reasoning_modes", []),
    )


def _scores_to_dict(s: DimensionScores) -> dict:
    """Serialize the five individual scores without computing a composite."""
    return {
        "capability_fit": s.capability_fit,
        "cost_fit": s.cost_fit,
        "latency_fit": s.latency_fit,
        "trust_compatibility": s.trust_compatibility,
        "historical_performance": s.historical_performance,
    }
