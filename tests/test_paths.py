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


def test_each_path_s_state_is_kept_and_its_trouble_logged_not_shown(monkeypatch):
    monkeypatch.setattr(paths, "plusone_link", None)
    monkeypatch.setattr(paths.GLib, "idle_add", lambda f: f())
    logged = []
    monkeypatch.setattr(paths, "log", lambda *parts: logged.append(" ".join(map(str, parts))))
    changes = []
    watched = paths.Paths(lambda: changes.append(1))
    watched._event({"event": "up", "device": DEVICE, "path": "bt"})
    path = {"event": "path", "device": DEVICE, "id": 1, "kind": "bt", "transport": "stream", "state": "active",
            "rtt_ms": None, "silent_ms": 0, "budget_bps": 900000, "budget_from": "prior", "said later": [1]}
    watched._event(path)
    assert [p["budget_bps"] for p in watched.states(DEVICE)] == [900000] and logged == []
    watched._event(dict(path, state="suspect", silent_ms=1500))
    assert [p["state"] for p in watched.states(DEVICE)] == ["suspect"]
    assert logged == ["link: the Bluetooth path (1) is suspect, nothing heard for 1.5 s"]
    watched._event(dict(path, silent_ms=2))
    assert logged[-1] == "link: the Bluetooth path (1) is active again"
    watched._event(dict(path, state="failed", silent_ms=5000))
    assert watched.states(DEVICE) == [] and logged[-1] == "link: the Bluetooth path (1) is failed, nothing heard for 5.0 s"
    watched._event({"event": "path", "device": DEVICE, "state": "suspect"})  # no id: not one
    assert watched.states(DEVICE) == [] and len(logged) == 3
    assert watched.path(DEVICE) == "Bluetooth" and changes == [1]  # nothing shown changed
    watched._event(dict(path, id=2))
    watched._event({"event": "gone", "device": DEVICE})
    assert watched.states(DEVICE) == []


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
    monkeypatch.setattr(paths, "plusone_link", None)  # no thread of its own: _run here
    watched = paths.Paths(lambda: None)
    monkeypatch.setattr(paths, "plusone_link", types.SimpleNamespace(Link=Link, LinkError=plusone_link.LinkError))
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


class FakeLink:
    """plus-one-link while there[0]: each watch() hung up at once, after
    the events of events() (on a watch, during it); devices() answers()."""

    def __init__(self, there, answers, events=lambda: []):
        if not there[0]:
            raise plusone_link.LinkError("plus-one-link is not there")
        self.answers, self.events = answers, events

    def watch(self, on_event):
        for event in self.events():
            on_event(event)
        return types.SimpleNamespace(thread=types.SimpleNamespace(join=lambda: None), close=lambda: None)

    def devices(self):
        return self.answers()

    def close(self):
        pass

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


def watched_by(monkeypatch, link, rounds, on_wait=lambda watched, waits: None):
    """Paths._run until it waited rounds times, on_wait(Paths, its waits) at each: (Paths, its waits, the
    paths shown at each change)."""
    monkeypatch.setattr(paths.GLib, "idle_add", lambda f: f())
    monkeypatch.setattr(paths, "plusone_link", None)  # no thread of its own: _run here
    shown, waits = [], []
    watched = paths.Paths(lambda: shown.append(dict(watched._paths)))
    monkeypatch.setattr(paths, "plusone_link", types.SimpleNamespace(Link=link, LinkError=plusone_link.LinkError))
    watched._stopped = types.SimpleNamespace(is_set=lambda: len(waits) >= rounds,
                                             wait=lambda seconds: waits.append(seconds) or on_wait(watched, waits))
    watched._run()
    return watched, waits, shown


def test_a_watch_hung_up_is_made_again_at_once_while_plus_one_link_answers(monkeypatch):
    # plus-one-link shuts a watch it could not tell an event, and goes on:
    # watched again at once (a second after the last at the soonest: no busy
    # loop), the path shown all along. Gone, it is looked for each AGAIN.
    there, watches = [True], []

    def link():
        watches.append(1)
        return FakeLink(there, lambda: [{"id": DEVICE, "connected": True, "path": "usb"}])

    def on_wait(_watched, waits):
        if len(waits) == 2:
            there[0] = False  # plus-one-link goes
    _watched, waits, shown = watched_by(monkeypatch, link, 3, on_wait)
    assert all(0.9 < wait <= 1.0 for wait in waits[:2]) and waits[2] == paths.AGAIN
    assert shown == [{DEVICE: "usb"}, {}]  # shown until plus-one-link went, and no longer


def test_a_watch_made_again_tells_what_changed_while_nothing_watched(monkeypatch):
    other, third = "0b" * 32, "0c" * 32
    answers = [[{"id": DEVICE, "connected": True, "path": "usb"}, {"id": other, "connected": True, "path": "lan"}],
               # Meanwhile DEVICE moved to the Wi-Fi and other went; third
               # came after this answer, as its event says.
               [{"id": DEVICE, "connected": True, "path": "lan"}, {"id": other, "connected": False},
                {"id": third, "connected": False}]]
    events = [[], [{"event": "up", "device": third, "path": "usb"}, {"event": "budget", "device": DEVICE, "kbps": 9}]]
    rounds = []
    watched_by(
        monkeypatch, lambda: FakeLink([True], lambda: answers[len(rounds)], lambda: events[len(rounds)]), 2,
        lambda watched, _waits: rounds.append(dict(watched._paths)))
    assert rounds == [{DEVICE: "usb", other: "lan"}, {DEVICE: "lan", third: "usb"}]


def test_each_watch_takes_the_paths_states_listed_but_those_told_of_since(monkeypatch):
    # devices()' "paths" at each watch: a path listed no more ended while
    # nothing watched; one an event told of since is as the event says (it
    # may have come after the answer); a path still in trouble is not logged
    # again. plus-one-link gone: no path's state left.
    logged = []
    monkeypatch.setattr(paths, "log", lambda *parts: logged.append(" ".join(map(str, parts))))
    usb = {"id": 1, "kind": "usb", "transport": "stream", "state": "active", "silent_ms": 0}
    lan = {"id": 2, "kind": "lan", "transport": "quic", "state": "suspect", "silent_ms": 1600}
    answers = [[{"id": DEVICE, "connected": True, "path": "usb", "paths": [usb, lan]}],
               [{"id": DEVICE, "connected": True, "path": "lan", "paths": [dict(lan, silent_ms=2600)]}],
               [{"id": DEVICE, "connected": True, "path": "lan", "paths": [dict(lan, silent_ms=3600)]}]]
    events = [[], [], [dict(lan, event="path", device=DEVICE, state="active", silent_ms=3)]]
    there, rounds = [True], []

    def on_wait(watched, waits):
        rounds.append([(path["id"], path["state"]) for path in watched.states(DEVICE)])
        if len(waits) == 3:
            there[0] = False  # plus-one-link goes
    watched_by(monkeypatch, lambda: FakeLink(there, lambda: answers[len(rounds)], lambda: events[len(rounds)]), 4,
               on_wait)
    assert rounds == [[(1, "active"), (2, "suspect")], [(2, "suspect")], [(2, "active")], []]
    assert logged == ["link: the Wi-Fi path (2) is suspect, nothing heard for 1.6 s",
                      "link: the Wi-Fi path (2) is active again"]
