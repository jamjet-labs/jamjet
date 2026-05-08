from jamjet.spec import DurabilityConfig


def test_defaults():
    cfg = DurabilityConfig()
    assert cfg.checkpoint_every_step is True
    assert cfg.seed is None
    assert cfg.db_path is None


def test_explicit_values():
    cfg = DurabilityConfig(checkpoint_every_step=False, seed=42, db_path="/tmp/x.db")
    assert cfg.seed == 42


def test_round_trip_json():
    cfg = DurabilityConfig(seed=7)
    assert DurabilityConfig.model_validate_json(cfg.model_dump_json()) == cfg
