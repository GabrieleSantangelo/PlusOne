"""The Files menu item: offered when Plus One says so, else per the switch."""
import importlib.util
import json
import os
import sys
import types

from gi.repository import GLib

EXTENSION = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                         "data", "nautilus", "plus-one-sign.py")


def load(monkeypatch):
    nautilus = types.ModuleType("gi.repository.Nautilus")
    nautilus.MenuProvider = type("MenuProvider", (), {})
    monkeypatch.setitem(sys.modules, "gi.repository.Nautilus", nautilus)
    spec = importlib.util.spec_from_file_location("plus_one_sign", EXTENSION)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Proxy:
    def __init__(self, owner, available):
        self.owner, self.available = owner, available

    def get_name_owner(self):
        return self.owner

    def get_cached_property(self, name):
        assert name == "SignAvailable"
        return None if self.available is None else GLib.Variant("b", self.available)


def test_plus_one_decides_while_running(monkeypatch, tmp_path):
    module = load(monkeypatch)
    monkeypatch.setattr(module, "SETTINGS", str(tmp_path / "missing.json"))
    offered = module.PlusOneSign._offered
    assert offered(types.SimpleNamespace(_proxy=Proxy(":1.42", False))) is False
    assert offered(types.SimpleNamespace(_proxy=Proxy(":1.42", True))) is True
    # Not running, or an older Plus One: the switch alone (on by default).
    assert offered(types.SimpleNamespace(_proxy=Proxy(None, False))) is True
    assert offered(types.SimpleNamespace(_proxy=Proxy(":1.42", None))) is True
    assert offered(types.SimpleNamespace(_proxy=None)) is True


def test_switched_off_in_the_settings(monkeypatch, tmp_path):
    module = load(monkeypatch)
    settings = tmp_path / "settings.json"
    settings.write_text(json.dumps({"sign_on_tablet": False}))
    monkeypatch.setattr(module, "SETTINGS", str(settings))
    assert module.PlusOneSign._offered(types.SimpleNamespace(_proxy=None)) is False
    settings.write_text("[]")
    assert module.PlusOneSign._offered(types.SimpleNamespace(_proxy=None)) is True
