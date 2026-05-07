from jamjet.runtime.local.seed import SeededClock, SeededRandom, SeededUuidGen


def test_seeded_random_deterministic():
    a = SeededRandom("exec123")
    b = SeededRandom("exec123")
    assert a.random() == b.random()
    assert a.randint(0, 100) == b.randint(0, 100)


def test_seeded_random_diverges_for_different_seeds():
    a = SeededRandom("exec123")
    b = SeededRandom("exec999")
    assert a.random() != b.random()


def test_seeded_uuid_deterministic():
    a = SeededUuidGen("exec123")
    b = SeededUuidGen("exec123")
    assert a.uuid4() == b.uuid4()


def test_seeded_clock_advances_monotonically():
    clk = SeededClock(start_iso="2026-05-07T12:00:00+00:00")
    t1 = clk.now()
    t2 = clk.now()
    assert t2 >= t1
