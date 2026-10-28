"""The controller: the desktop in Plus One's app, and what is said when it
cannot show, or ends."""
import types

import pytest

from plusone import engine, link
from plusone.engine import Controller, Settings, State


class FakeTablets:
    """The tablet in use (tablets.py), and where its app is."""

    def __init__(self, active=True, app_at=()):
        self.tablet = types.SimpleNamespace(name="Tab", app_id="0123456789abcdef") if active else None
        self.app_at = list(app_at)

    def targets(self, tablet):
        return self.app_at

    def screen_seen(self, screen):
        self.screen = screen

    def active(self):
        return self.tablet


class FakeAppDisplay:
    made = []

    def __init__(self, targets, app_id, mode, bitrate, encoder, **callbacks):
        self.targets, self.mode, self.callbacks, self.stopped = targets, mode, callbacks, False
        self.gated = None
        FakeAppDisplay.made.append(self)

    def gate(self, closed, active=True):
        self.gated = (closed, active)

    def start(self):
        pass

    def stop(self):
        self.stopped = True


def app_controller(monkeypatch, settings=None, app_at=(("10.0.0.7", 47236),), active=True):
    from plusone import appdisplay
    FakeAppDisplay.made = []
    monkeypatch.setattr(appdisplay, "AppDisplay", FakeAppDisplay)
    monkeypatch.setattr(engine.media, "pick_encoder", lambda preferred: "x264")
    monkeypatch.setattr(engine.media, "warm_up", lambda name: None)
    return Controller(settings or Settings(), tablets=FakeTablets(active=active, app_at=app_at))


def test_no_screen_without_a_paired_tablet(monkeypatch):
    controller = app_controller(monkeypatch, active=False)
    controller.start()
    assert controller.state == State.ERROR and controller.detail[0] == "Pair a tablet first"
    assert not controller.running and FakeAppDisplay.made == []


def test_no_screen_without_the_app_on_the_network(monkeypatch):
    controller = app_controller(monkeypatch, app_at=())
    controller.start()
    assert controller.state == State.ERROR and controller.detail == (
        "{name} is not on the network: open Plus One on it", {"name": "Tab"})
    assert FakeAppDisplay.made == []


def test_the_app_shows_the_desktop(monkeypatch):
    controller = app_controller(monkeypatch)
    controller.start()
    assert len(FakeAppDisplay.made) == 1
    shown = FakeAppDisplay.made[0]
    # The size once the tablet says its screen (about 1920x1200 at its proportions).
    assert shown.targets == [("10.0.0.7", 47236)] and shown.mode == (None, None, 60)
    assert controller.state == State.CONNECTING
    shown.callbacks["on_started"]((1920, 1200, 60), (2880, 1800))
    assert controller.state == State.STREAMING and controller.tablets.screen == (2880, 1800)
    shown.callbacks["on_paused"](True)
    assert "paused" in controller.detail[0] and controller.state == State.STREAMING
    controller.stop()
    assert shown.stopped and controller.state == State.IDLE and not controller.running


@pytest.mark.parametrize("error, said", [
    (link.Refused("background"), "On {name}, let Plus One display over other apps, or open it"),
    (link.PairingRefused("Tab"), "{name} no longer knows this PC: pair it again"),
    (link.Incompatible("v2"), "Plus One on {name} and on this PC are different versions: update them"),
    (ConnectionRefusedError(), "{name} did not answer: is Plus One open on it?"),
])
def test_why_the_app_did_not_show_it_is_said(monkeypatch, error, said):
    controller = app_controller(monkeypatch)
    controller.start()
    FakeAppDisplay.made[0].callbacks["on_ended"](error, False)
    assert controller.state == State.ERROR and controller.detail[0] == said and not controller.running


def test_the_tablet_ending_it_ends_it(monkeypatch):
    controller = app_controller(monkeypatch)
    controller.start()
    shown = FakeAppDisplay.made[0]
    shown.callbacks["on_started"]((1920, 1200, 60))
    shown.callbacks["on_ended"](EOFError(), True)
    assert controller.state == State.IDLE and not controller.running
    assert controller.detail[0] == "The tablet closed the desktop"


def test_why_the_desktop_in_the_app_ended_is_said(monkeypatch):
    for error, said in ((TimeoutError(110, "timed out"), "The tablet is no longer reachable"),
                        (RuntimeError("enc: failed"), "The desktop stopped: {error}")):
        controller = app_controller(monkeypatch)
        controller.start()
        shown = FakeAppDisplay.made[0]
        shown.callbacks["on_started"]((1920, 1200, 60))
        shown.callbacks["on_ended"](error, True)
        assert controller.state == State.IDLE and controller.detail[0] == said


def test_a_late_callback_of_an_ended_session_changes_nothing(monkeypatch):
    controller = app_controller(monkeypatch)
    controller.start()
    old = FakeAppDisplay.made[0]
    controller.stop()
    old.callbacks["on_started"]((1920, 1200, 60))
    old.callbacks["on_ended"](OSError(), False)
    assert controller.state == State.IDLE and controller.detail[0] == ""


def test_a_size_chosen_is_asked_for(monkeypatch):
    settings = Settings()
    settings.app_size = "2880x1800"
    controller = app_controller(monkeypatch, settings=settings)
    controller.start()
    assert FakeAppDisplay.made[0].mode == (2880, 1800, 60)


def test_sizes_are_read_with_care():
    assert engine.parse_size("2880x1800") == (2880, 1800)
    for nonsense in ("auto", None, "", "12x9", "axb", "99999x2"):
        assert engine.parse_size(nonsense) is None


def test_the_desktop_locked_is_told_to_the_display_now_and_to_one_started_later(monkeypatch):
    controller = app_controller(monkeypatch)
    controller.gate(True, True)
    controller.start()
    display = FakeAppDisplay.made[-1]
    assert display.gated == (True, True)  # started while locked: gated from the first frame
    controller.gate(False, True)
    assert display.gated == (False, True)
