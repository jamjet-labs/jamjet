"""LLMAdapter over the governed Model seam. Drop-in for strategy runners.

Strategy runners call ``adapter.generate(messages, tools=...)`` and read
``msg.content`` / ``msg.tool_calls``. We return the seam's OpenAI-shaped message
unchanged so those runners need no edits.

Governance threading (T3-7)
---------------------------
``agent.run()`` is the in-process path: ``LocalRuntime`` builds one
``SeamAdapter`` per execution and the strategy runner drives every model call
through ``adapter.generate`` -> ``Model.complete`` -> the seam middleware chain.
Passing the agent's :class:`~jamjet.agents.governance.GovernanceConfig` here
builds that chain with ``default_model_middleware(governance=...)`` so the
budget / allowlist / PII knobs ENFORCE on ``agent.run()`` exactly as on the
durable path — closing the deferred-from-T3-2 in-process budget gap.  Omitting
``governance`` keeps the Track-1 default (allow-all allowlist, no budget, PII on)
so callers that construct a bare adapter are unchanged.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from jamjet.model.defaults import default_model_middleware
from jamjet.model.seam import Model
from jamjet.model.types import ModelRequest, parse_model_ref
from jamjet.spec import LLMConfig

if TYPE_CHECKING:
    from jamjet.agents.governance import GovernanceConfig


class SeamAdapter:
    config: LLMConfig

    def __init__(
        self,
        config: LLMConfig,
        *,
        model: Model | None = None,
        governance: GovernanceConfig | None = None,
    ) -> None:
        self.config = config
        self._ref = parse_model_ref(config.model)
        # Build the governed seam from the agent's GovernanceConfig (T3-7). An
        # explicit ``model`` (tests) wins; otherwise the chain is built with
        # ``governance`` so budget/allowlist/PII enforce on the in-process path.
        self._model = model or Model(middleware=default_model_middleware(governance))

    async def generate(
        self,
        messages: list[dict[str, Any]],
        *,
        tools: list[dict[str, Any]] | None = None,
    ) -> Any:
        request = ModelRequest(
            ref=self._ref,
            messages=messages,
            tools=tools,
            temperature=self.config.temperature,
            max_tokens=self.config.max_tokens,
        )
        response = await self._model.complete(request)
        return response.message
