import sys
import types

from jamjet.model import litellm_backend


def test_import_turns_off_litellm_debug_banner(monkeypatch):
    monkeypatch.delenv("JAMJET_LITELLM_DEBUG", raising=False)
    fake = types.SimpleNamespace(suppress_debug_info=False, set_verbose=True)
    monkeypatch.setitem(sys.modules, "litellm", fake)
    litellm_backend._import_litellm()
    assert fake.suppress_debug_info is True
    assert fake.set_verbose is False


def test_debug_env_leaves_litellm_settings_alone(monkeypatch):
    monkeypatch.setenv("JAMJET_LITELLM_DEBUG", "1")
    fake = types.SimpleNamespace(suppress_debug_info=False, set_verbose=True)
    monkeypatch.setitem(sys.modules, "litellm", fake)
    litellm_backend._import_litellm()
    assert fake.suppress_debug_info is False
    assert fake.set_verbose is True
