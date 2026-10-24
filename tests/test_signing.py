"""Signing on the tablet: requests, windows and the connection opened for them."""
from gi.repository import GLib

from plusone import i18n
from plusone.engine import NOT_ANSWERING, State
from plusone.signing import Signing


class FakeController:
    def __init__(self):
        self.state, self.detail, self.running = State.IDLE, ("", {}), False
        self.starts = self.stops = 0

    def start(self):
        self.running, self.starts = True, self.starts + 1

    def stop(self):
        self.running, self.stops = False, self.stops + 1


class FakeApp:
    def __init__(self):
        self.controller = FakeController()

    def add_action(self, action):
        pass

    def send_notification(self, id, note):
        pass

    def withdraw_notification(self, id):
        pass


class FakeWindow:
    def __init__(self, path):
        self.path, self.presented = path, 0

    def present(self):
        self.presented += 1


def make(monkeypatch):
    app = FakeApp()
    signing = Signing(app)
    opened, notes = [], []
    monkeypatch.setattr(signing, "_open_on_tablet", opened.append)
    monkeypatch.setattr(signing, "_notify_waiting", lambda path, body: notes.append((path, body)))
    return app, signing, opened, notes


def test_asking_again_while_waiting_opens_one_window(monkeypatch, tmp_path):
    # Files was clicked three times while the tablet took its time: Done must
    # still close the only window and the connection opened for it.
    app, signing, opened, _ = make(monkeypatch)
    path = str(tmp_path / "form.pdf")
    for _ in range(3):
        signing.request(path)
    assert signing.queue == [path] and app.controller.starts == 1
    app.controller.state = State.STREAMING
    signing.on_state(State.STREAMING)
    assert opened == [path]
    window = FakeWindow(path)
    signing.windows.append(window)
    signing.request(path)
    assert window.presented == 1 and opened == [path]
    signing._done(window, path, None)
    while GLib.MainContext.default().iteration(False):
        pass
    assert app.controller.stops == 1


def test_no_disconnect_while_another_document_waits_for_the_monitor(monkeypatch, tmp_path):
    app, signing, _, _ = make(monkeypatch)
    signing.connected_for_us = True
    signing.opening.add(str(tmp_path / "other.pdf"))
    signing._maybe_disconnect()
    assert app.controller.stops == 0


def test_not_answering_reaches_the_notification(monkeypatch, tmp_path):
    app, signing, _, notes = make(monkeypatch)
    path = str(tmp_path / "form.pdf")
    signing.request(path)
    app.controller.state, app.controller.detail = State.CONNECTING, (NOT_ANSWERING, {})
    assert signing.on_state(State.CONNECTING) is True
    assert notes[-1] == (path, i18n._(NOT_ANSWERING))
