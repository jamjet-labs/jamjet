from jamjet.model.metering import MeteringMiddleware, ModelCallRecord
from jamjet.model.types import ModelRequest, ModelResponse, parse_model_ref


def _req() -> ModelRequest:
    return ModelRequest(ref=parse_model_ref("anthropic/claude-opus-4-8"), messages=[])


def _resp() -> ModelResponse:
    return ModelResponse(message=object(), input_tokens=11, output_tokens=7, cost_usd=0.002)


async def test_records_token_and_cost():
    mw = MeteringMiddleware()
    out = await mw.after(_req(), _resp())
    assert len(mw.records) == 1
    rec = mw.records[0]
    assert rec == ModelCallRecord(
        provider="anthropic", model="claude-opus-4-8", input_tokens=11, output_tokens=7, cost_usd=0.002
    )
    assert out.input_tokens == 11  # after() returns the response unchanged


async def test_calls_sink_when_provided():
    seen: list[ModelCallRecord] = []
    mw = MeteringMiddleware(sink=seen.append)
    await mw.after(_req(), _resp())
    assert seen and seen[0].provider == "anthropic"
