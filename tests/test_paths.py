"""What the tablet is connected by, from plus-one-link's events."""
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
