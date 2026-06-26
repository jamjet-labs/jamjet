"""Token/cost metering as a post-call middleware. Every completion emits a record."""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass

from jamjet.model.middleware import BaseModelMiddleware
from jamjet.model.types import ModelRequest, ModelResponse


@dataclass(frozen=True)
class ModelCallRecord:
    provider: str
    model: str
    input_tokens: int
    output_tokens: int
    cost_usd: float


MeterSink = Callable[[ModelCallRecord], None]


class MeteringMiddleware(BaseModelMiddleware):
    """Append a ``ModelCallRecord`` per completion; forward to an optional sink."""

    def __init__(self, sink: MeterSink | None = None) -> None:
        self._sink = sink
        self.records: list[ModelCallRecord] = []

    async def after(self, request: ModelRequest, response: ModelResponse) -> ModelResponse:
        record = ModelCallRecord(
            provider=request.ref.provider,
            model=request.ref.model,
            input_tokens=response.input_tokens,
            output_tokens=response.output_tokens,
            cost_usd=response.cost_usd,
        )
        self.records.append(record)
        if self._sink is not None:
            self._sink(record)
        return response
