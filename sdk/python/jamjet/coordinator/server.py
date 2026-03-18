from __future__ import annotations

from typing import Any

from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Route

from .default_strategy import DefaultCoordinatorStrategy
from .strategy import CoordinatorStrategy, AgentCandidate, ScoringResult, DimensionScores


class StrategyServer:
    """REST server that serves coordinator strategies to the Rust runtime."""

    def __init__(self, host: str = "127.0.0.1", port: int = 4270):
        self.host = host
        self.port = port
        self._registry = None
        self._strategies: dict[str, CoordinatorStrategy] = {
            "default": DefaultCoordinatorStrategy(registry=None),
        }
        self._app = Starlette(routes=[
            Route("/coordinator/discover", self._handle_discover, methods=["POST"]),
            Route("/coordinator/score", self._handle_score, methods=["POST"]),
            Route("/coordinator/decide", self._handle_decide, methods=["POST"]),
            Route("/health", self._handle_health, methods=["GET"]),
        ])

    def register_strategy(self, name: str, strategy: CoordinatorStrategy) -> None:
        self._strategies[name] = strategy
        if self._registry is not None and hasattr(strategy, "_registry"):
            strategy._registry = self._registry

    def set_registry(self, registry: Any) -> None:
        self._registry = registry
        for strategy in self._strategies.values():
            if hasattr(strategy, "_registry"):
                strategy._registry = registry

    def _get_strategy(self, data: dict) -> CoordinatorStrategy:
        name = data.get("strategy_name", "default")
        if name not in self._strategies:
            raise ValueError(f"Unknown strategy: {name}")
        return self._strategies[name]

    async def _handle_discover(self, request: Request) -> JSONResponse:
        data = await request.json()
        strategy = self._get_strategy(data)
        candidates, filtered = await strategy.discover(
            task=data["task"],
            required_skills=data.get("required_skills", []),
            preferred_skills=data.get("preferred_skills", []),
            trust_domain=data.get("trust_domain"),
            context=data.get("context", {}),
        )
        return JSONResponse({
            "candidates": [_candidate_to_dict(c) for c in candidates],
            "filtered_out": filtered,
        })

    async def _handle_score(self, request: Request) -> JSONResponse:
        data = await request.json()
        strategy = self._get_strategy(data)
        candidates = [_dict_to_candidate(c) for c in data.get("candidates", [])]
        rankings, spread = await strategy.score(
            task=data["task"],
            candidates=candidates,
            weights=data.get("weights", {}),
            context=data.get("context", {}),
        )
        return JSONResponse({
            "rankings": [
                {"uri": r.agent_uri, "scores": _scores_to_dict(r.scores), "composite": r.composite}
                for r in rankings
            ],
            "spread": spread,
        })

    async def _handle_decide(self, request: Request) -> JSONResponse:
        data = await request.json()
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
        return JSONResponse({
            "selected_uri": decision.selected_uri,
            "method": decision.method,
            "reasoning": decision.reasoning,
            "confidence": decision.confidence,
            "rejected": decision.rejected,
            "tiebreaker_tokens": decision.tiebreaker_tokens,
            "tiebreaker_cost": decision.tiebreaker_cost,
        })

    async def _handle_health(self, request: Request) -> JSONResponse:
        return JSONResponse({"status": "ok", "strategies": list(self._strategies.keys())})

    def run(self) -> None:
        import uvicorn
        uvicorn.run(self._app, host=self.host, port=self.port)


def _candidate_to_dict(c: AgentCandidate) -> dict:
    return {
        "uri": c.uri,
        "agent_card": c.agent_card,
        "skills": c.skills,
        "latency_class": c.latency_class,
        "cost_class": c.cost_class,
        "trust_domain": c.trust_domain,
    }


def _dict_to_candidate(d: dict) -> AgentCandidate:
    return AgentCandidate(
        uri=d["uri"],
        agent_card=d.get("agent_card", {}),
        skills=d.get("skills", []),
        latency_class=d.get("latency_class"),
        cost_class=d.get("cost_class"),
        trust_domain=d.get("trust_domain"),
    )


def _scores_to_dict(s) -> dict:
    return {
        "capability_fit": s.capability_fit,
        "cost_fit": s.cost_fit,
        "latency_fit": s.latency_fit,
        "trust_compatibility": s.trust_compatibility,
        "historical_performance": s.historical_performance,
    }
