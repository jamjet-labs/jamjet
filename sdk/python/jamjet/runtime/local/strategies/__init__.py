"""Strategy registry. get_strategy_runner(name) -> StrategyRunner."""
from jamjet.runtime.local.strategies import (
    consensus,
    critic,
    debate,
    plan_and_execute,
    react,
    reflection,
)
from jamjet.runtime.local.strategies.base import StrategyRunner

_RUNNERS: dict[str, StrategyRunner] = {
    "plan-and-execute": plan_and_execute.run,
    "react": react.run,
    "critic": critic.run,
    "reflection": reflection.run,
    "consensus": consensus.run,
    "debate": debate.run,
}


def get_strategy_runner(name: str) -> StrategyRunner:
    runner = _RUNNERS.get(name)
    if runner is None:
        raise ValueError(f"Unknown strategy {name!r}. Valid: {list(_RUNNERS)}")
    return runner


__all__ = ["StrategyRunner", "get_strategy_runner"]
