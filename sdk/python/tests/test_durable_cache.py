"""Tests for jamjet.durable.cache.SqliteCache."""

import pytest

from jamjet.durable.cache import SqliteCache


def test_get_missing_returns_none(tmp_path):
    cache = SqliteCache(tmp_path / "cache.db")
    assert cache.get("absent") is None


def test_put_then_get_roundtrips(tmp_path):
    cache = SqliteCache(tmp_path / "cache.db")
    cache.put("key1", {"foo": "bar", "n": 42})
    assert cache.get("key1") == {"foo": "bar", "n": 42}


def test_put_overwrites_existing(tmp_path):
    cache = SqliteCache(tmp_path / "cache.db")
    cache.put("key1", "first")
    cache.put("key1", "second")
    assert cache.get("key1") == "second"


def test_persistence_across_instances(tmp_path):
    """A new Cache instance pointing at the same path sees prior writes."""
    db = tmp_path / "cache.db"
    SqliteCache(db).put("key1", [1, 2, 3])
    assert SqliteCache(db).get("key1") == [1, 2, 3]


def test_cache_creates_parent_dir(tmp_path):
    deep = tmp_path / "a" / "b" / "c" / "cache.db"
    cache = SqliteCache(deep)
    cache.put("k", 1)
    assert cache.get("k") == 1


def test_unpicklable_value_raises(tmp_path):
    cache = SqliteCache(tmp_path / "cache.db")
    with pytest.raises(TypeError, match="not picklable"):
        cache.put("k", lambda x: x)


def test_concurrent_open_safe(tmp_path):
    """SQLite in WAL mode handles concurrent open without lock errors."""
    db = tmp_path / "cache.db"
    a = SqliteCache(db)
    b = SqliteCache(db)
    a.put("k", "val-a")
    assert b.get("k") == "val-a"


def test_get_or_compute_returns_cached_without_calling_compute(tmp_path):
    cache = SqliteCache(tmp_path / "cache.db")
    cache.put("key1", "stored")
    calls = []

    def compute():
        calls.append(1)
        return "computed"

    result = cache.get_or_compute("key1", compute)
    assert result == "stored"
    assert calls == []  # compute was not called


def test_get_or_compute_calls_compute_on_miss_and_caches(tmp_path):
    cache = SqliteCache(tmp_path / "cache.db")
    calls = []

    def compute():
        calls.append(1)
        return {"new": "value"}

    r1 = cache.get_or_compute("missing", compute)
    r2 = cache.get_or_compute("missing", compute)

    assert r1 == r2 == {"new": "value"}
    assert calls == [1]  # compute called exactly once
