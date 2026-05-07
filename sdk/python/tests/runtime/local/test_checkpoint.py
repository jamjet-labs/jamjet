from pathlib import Path

import pytest

from jamjet.runtime.local.checkpoint import CheckpointStore


@pytest.fixture
def store(tmp_path: Path) -> CheckpointStore:
    return CheckpointStore(tmp_path / "ckpt.db", ir_version="1.0", spec_hash="h")


@pytest.mark.asyncio
async def test_init_creates_schema(store: CheckpointStore) -> None:
    await store.init()
    assert store.db_path.exists()


@pytest.mark.asyncio
async def test_record_and_lookup_step(store: CheckpointStore) -> None:
    await store.init()
    await store.start_step("s1", input_hash="h1", input_json='{"q":"x"}')
    record = await store.get_step("s1")
    assert record is not None
    assert record.status == "running"

    await store.complete_step("s1", output_json='"done"', duration_ms=12.0)
    record = await store.get_step("s1")
    assert record is not None
    assert record.status == "completed"
    assert record.output_json == '"done"'


@pytest.mark.asyncio
async def test_input_hash_mismatch_returns_none(store: CheckpointStore) -> None:
    await store.init()
    await store.start_step("s1", input_hash="h1", input_json="{}")
    await store.complete_step("s1", output_json="1", duration_ms=1.0)
    record = await store.get_step_if_match("s1", input_hash="DIFFERENT")
    assert record is None


@pytest.mark.asyncio
async def test_input_hash_match_returns_record(store: CheckpointStore) -> None:
    await store.init()
    await store.start_step("s1", input_hash="h1", input_json="{}")
    await store.complete_step("s1", output_json="1", duration_ms=1.0)
    record = await store.get_step_if_match("s1", input_hash="h1")
    assert record is not None
    assert record.output_json == "1"


@pytest.mark.asyncio
async def test_seed_persistence(store: CheckpointStore) -> None:
    await store.init()
    await store.set_seed("random", "12345")
    assert await store.get_seed("random") == "12345"


@pytest.mark.asyncio
async def test_resume_picks_first_incomplete(store: CheckpointStore) -> None:
    await store.init()
    await store.start_step("s1", input_hash="a", input_json="{}")
    await store.complete_step("s1", output_json="1", duration_ms=1.0)
    await store.start_step("s2", input_hash="b", input_json="{}")
    incomplete = await store.list_incomplete_steps()
    assert [s.step_id for s in incomplete] == ["s2"]


@pytest.mark.asyncio
async def test_fail_step(store: CheckpointStore) -> None:
    await store.init()
    await store.start_step("s1", input_hash="h", input_json="{}")
    await store.fail_step("s1", error="boom")
    record = await store.get_step("s1")
    assert record is not None
    assert record.status == "failed"
    assert record.error == "boom"
