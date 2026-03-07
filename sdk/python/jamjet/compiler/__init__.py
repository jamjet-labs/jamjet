"""
JamJet compiler package.

Provides strategy → IR compilation (§14.4) and agent-first YAML parsing.
"""

from .strategies import StrategyLimits, compile_strategy

__all__ = ["compile_strategy", "StrategyLimits"]
