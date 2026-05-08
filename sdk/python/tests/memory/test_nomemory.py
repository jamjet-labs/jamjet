import pytest

from jamjet.memory import NoMemory


@pytest.mark.asyncio
async def test_record_raises_clear_error():
    nm = NoMemory()
    with pytest.raises(RuntimeError, match="memory is disabled"):
        await nm.record("hi")


@pytest.mark.asyncio
async def test_recall_raises_clear_error():
    nm = NoMemory()
    with pytest.raises(RuntimeError, match="memory is disabled"):
        await nm.recall("q")


@pytest.mark.asyncio
async def test_ask_raises_clear_error():
    nm = NoMemory()
    with pytest.raises(RuntimeError, match="memory is disabled"):
        await nm.ask("q")


@pytest.mark.asyncio
async def test_context_raises():
    nm = NoMemory()
    with pytest.raises(RuntimeError, match="memory is disabled"):
        await nm.context("q")


@pytest.mark.asyncio
async def test_record_message_raises():
    nm = NoMemory()
    with pytest.raises(RuntimeError, match="memory is disabled"):
        await nm.record_message("hi")


@pytest.mark.asyncio
async def test_synthesize_raises():
    nm = NoMemory()
    with pytest.raises(RuntimeError, match="memory is disabled"):
        await nm.synthesize("q")
