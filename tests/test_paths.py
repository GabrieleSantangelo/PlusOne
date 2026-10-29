"""What the tablet is connected by, from plus-one-link's events."""
import types

import pytest

import plusone_link
from plusone.link import paths

DEVICE = "6a2665bc" * 8


def test_events_say_the_path(monkeypatch):
    monkeypatch.setattr(paths, "plusone_link", None)  # no thread looking for plus-one-link
    monkeypatch.setattr(paths.GLib, "idle_add", lambda f: f())
    changes = []
    watched = paths.Paths(lambda: changes.append(1))
    assert watched.path(DEVICE[:16]) is None
    watched._event({"event": "up", "device": DEVICE, "path": "usb"})
    assert watched.path(DEVICE[:16]) == "cable"
    watched._event({"event": "detached", "device": DEVICE})
    assert watched.path(DEVICE) == paths.RECONNECTING
    watched._event({"event": "moved", "device": DEVICE, "path": "lan"})
    assert watched.path(DEVICE[:16]) == "Wi-Fi"
    watched._event({"event": "moved", "device": DEVICE, "path": "lan", "transport": "quic"})  # the same path: not told
    watched._event({"event": "moved", "device": DEVICE, "path": "bt", "transport": "stream"})
    assert watched.path(DEVICE[:16]) == "Bluetooth"
    watched._event({"event": "gone", "device": DEVICE})
    assert watched.path(DEVICE) is None
    assert len(changes) == 5


def looked_for_once(monkeypatch, devices):
    """Paths._run once (its wait for the next look ends it), with a
    plus-one-link whose devices() is devices: (Paths, what was closed, waits)."""
    monkeypatch.setattr(paths.GLib, "idle_add", lambda f: f())
    closed, waits = [], []

    class Watcher:
        thread = types.SimpleNamespace(join=lambda: None)

        def close(self):
            closed.append("watcher")

    class Link:
        def watch(self, on_event):
            on_event({"event": "up", "device": DEVICE, "path": "lan"})
            return Watcher()

        def devices(self):
            return devices()

        def close(self):
            closed.append("link")
    monkeypatch.setattr(paths, "plusone_link", types.SimpleNamespace(Link=Link, LinkError=plusone_link.LinkError))
    watched = paths.Paths.__new__(paths.Paths)
    watched.on_change, watched.on_link, watched._paths, watched._watcher = (lambda: None), (lambda link: None), {}, None
    watched._stopped = types.SimpleNamespace(is_set=lambda: bool(waits), wait=waits.append)
    watched._run()
    return watched, closed, waits


def broken_pipe():
    raise BrokenPipeError()


def a_bug():
    raise KeyError("id")


@pytest.mark.parametrize("devices", [broken_pipe, a_bug])
def test_looking_for_plus_one_link_again_after_any_failure_leaves_nothing_open(monkeypatch, devices):
    watched, closed, waits = looked_for_once(monkeypatch, devices)
    assert waits == [paths.AGAIN]  # back to its 15 s, not a thread gone for good
    assert "watcher" in closed and "link" in closed and watched._watcher is None
    assert watched.path(DEVICE) is None  # nothing heard any more: not shown as connected
