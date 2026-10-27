"""The window: its sidebar and pages, the tablet in use, pairing, the paired tablets.

Needs a display: CI runs it on a headless Weston (tools/ci.sh sets
PLUSONE_GUI_TESTS=1); on a desktop it is skipped so no window pops up.
"""
import os
import threading
import types

import pytest

if not os.environ.get("PLUSONE_GUI_TESTS"):
    pytest.skip("GUI tests run in CI (set PLUSONE_GUI_TESTS=1)", allow_module_level=True)

import gi  # noqa: E402

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Adw, GLib, Gtk  # noqa: E402

from plusone import gui, link  # noqa: E402
from plusone.engine import Settings, State  # noqa: E402
from plusone.link import Tablets  # noqa: E402

KITCHEN, OFFICE = "aaaa" + "0" * 60, "bbbb" + "0" * 60


class Nearby:
    def __init__(self):
        self.found = []  # (app id, address, port, name)

    def apps(self):
        return list(self.found)

    def addresses(self, app_id):
        return [(a, p) for i, a, p, _n in self.found if i == app_id]

    def refresh(self):
        pass


def make(monkeypatch, tmp_path, tablets=()):
    monkeypatch.setattr(gui, "shell_extension_state", lambda callback: None)
    paired = link.Paired(str(tmp_path))
    for fingerprint, name in tablets:
        paired.add(fingerprint, name)
    app = Adw.Application(application_id="io.github.gabrielesantangelo.PlusOne.Test")
    app.register(None)
    app.settings = Settings()
    app.discovery = Nearby()
    app.tablets = Tablets(app.settings, discovery=app.discovery, paired=paired)
    app.controller = types.SimpleNamespace(state=State.IDLE, running=False, detail=("", {}),
                                           start=lambda: None, stop=lambda: None)
    app.listeners, app.log_listeners, app.log_lines = [], [], ["a line of the log"]
    app.status_text = lambda: ("Ready", "")
    app.save_settings = lambda: None
    app.window_closed = lambda: None
    app.set_sign_on_tablet = lambda on: setattr(app.settings, "sign_on_tablet", on)
    app.set_background = lambda on: setattr(app.settings, "background", on)
    app.set_microphone_noise = lambda noise: setattr(app.settings, "microphone_noise", noise)

    def changed():
        for listener in list(app.listeners):
            listener()
    app.devices = types.SimpleNamespace(camera=types.SimpleNamespace(state="off", users=[]),
                                        microphone=types.SimpleNamespace(state="off", users=[]),
                                        speakers=types.SimpleNamespace(state="off", users=[]))
    app.set_device = lambda kind, on: (setattr(app.settings, kind, on), changed())
    app.set_camera_options = lambda facing, quality: (setattr(app.settings, "camera_facing", facing),
                                                      setattr(app.settings, "camera_quality", quality))
    app.use_tablet = lambda fp: (app.tablets.use(fp), changed())
    app.forget_tablet = lambda fp: (app.tablets.forget(fp), changed())
    return gui.Window(app), app


def spin(until, timeout=5):
    import time
    end = time.monotonic() + timeout
    while time.monotonic() < end and not until():
        GLib.MainContext.default().iteration(False)
        time.sleep(0.005)
    return until()


def test_without_a_tablet_the_window_offers_pairing(monkeypatch, tmp_path):
    win, _app = make(monkeypatch, tmp_path)
    assert win.stack.get_visible_child_name() == "empty"


def test_the_tablet_in_use(monkeypatch, tmp_path):
    win, app = make(monkeypatch, tmp_path, [(OFFICE, "Office"), (KITCHEN, "Kitchen")])
    assert win.stack.get_visible_child_name() == "tablet"
    assert win.name_label.get_label() == "Kitchen" and win.status_label.get_label() == "Not found: open Plus One on the tablet"
    assert win.screen_button.get_label() == "Use as second screen"
    app.discovery.found = [(KITCHEN[:16], "10.0.0.2", 47236, "Kitchen")]
    win._refresh()
    assert win.status_label.get_label() == "Ready" and win.where_label.get_label() == "Ready"
    # The Tablets page: the paired ones, the one in use checked.
    assert [row.get_title() for row in win.tablet_rows] == ["Kitchen", "Office"]
    win.activate_action("win.use-tablet", GLib.Variant("s", OFFICE))
    assert app.settings.tablet == OFFICE and win.name_label.get_label() == "Office"


def test_every_section_in_the_sidebar(monkeypatch, tmp_path):
    win, app = make(monkeypatch, tmp_path, [(KITCHEN, "Kitchen")])
    assert list(win.pages) == [key for key, _icon, _title in gui.SECTIONS]
    win.show_section("speakers")
    assert win.split.get_content() is win.pages["speakers"]
    assert win.sidebar_list.get_selected_row() is win.sidebar_rows["speakers"]
    win.name_button.emit("clicked")  # the tablet on top: its page
    assert win.split.get_content() is win.pages["tablets"]
    # A device switched on is marked in the sidebar; green while in use.
    win.device_rows["camera"].set_active(True)
    assert win.sidebar_marks["camera"].get_visible() and not win.sidebar_marks["microphone"].get_visible()
    app.devices.camera.state = "in use"
    win._refresh()
    assert "success" in win.sidebar_marks["camera"].get_css_classes()


def test_no_other_tablet_while_connected(monkeypatch, tmp_path):
    win, app = make(monkeypatch, tmp_path, [(OFFICE, "Office"), (KITCHEN, "Kitchen")])
    app.controller.running, app.controller.state = True, State.STREAMING
    win._refresh()
    assert not win.use_action.get_enabled() and win.screen_button.get_label() == "Disconnect"


def test_pairing_lists_the_tablets_with_the_app(monkeypatch, tmp_path):
    _win, app = make(monkeypatch, tmp_path, [(KITCHEN, "Kitchen")])
    app.discovery.found = [(KITCHEN[:16], "10.0.0.2", 47236, "Kitchen"), ("cccc" + "0" * 12, "10.0.0.3", 47236, "New")]
    dialog = gui.PairDialog(app)
    assert [(row.get_title(), row.get_subtitle()) for row in dialog.rows] == [("Kitchen", "Paired"), ("New", "")]


def test_the_first_answer_to_the_code_counts(monkeypatch, tmp_path):
    # "Same code", then closing the dialog: still paired.
    _win, app = make(monkeypatch, tmp_path)
    dialog = gui.PairDialog(app)
    dialog.stage = Gtk.Box()
    results = []
    asking = threading.Thread(target=lambda: results.append(dialog._ask(dialog.attempt, "Tab", "123456")))
    asking.start()
    assert spin(lambda: dialog.answer is not None)
    dialog._answered(True)
    dialog._answered(False)
    asking.join(5)
    assert results == [True]


def test_paired_with_the_tablet_chosen(monkeypatch, tmp_path):
    _win, app = make(monkeypatch, tmp_path)
    new = "cccc" + "0" * 60
    app.discovery.found = [(new[:16], "10.0.0.3", 47236, "New")]
    asked = []

    def pair(targets, ask):
        asked.append(targets)
        assert ask("New", "654321") is True
        app.tablets.paired.add(new, "New")
        return new, "New"
    monkeypatch.setattr(link, "pair", pair)
    paired = []
    dialog = gui.PairDialog(app, on_paired=paired.append)
    dialog.rows[0].emit("activated")
    assert spin(lambda: dialog.answer is not None)
    dialog._confirmed()
    assert spin(lambda: paired == ["New"])
    assert asked == [[("10.0.0.3", 47236)]] and app.settings.tablet == new


def test_forgetting_a_paired_tablet(monkeypatch, tmp_path):
    win, app = make(monkeypatch, tmp_path, [(OFFICE, "Office"), (KITCHEN, "Kitchen <b>&")])
    assert [row.get_title() for row in win.tablet_rows] == ["Kitchen <b>&", "Office"]
    win._ask_forget(KITCHEN, "Kitchen <b>&").emit("response", "cancel")
    assert len(app.tablets.all()) == 2
    win._ask_forget(KITCHEN, "Kitchen <b>&").emit("response", "forget")
    assert [row.get_title() for row in win.tablet_rows] == ["Office"]
    assert app.settings.tablet == OFFICE and win.name_label.get_label() == "Office"


def test_settings_and_log(monkeypatch, tmp_path):
    win, app = make(monkeypatch, tmp_path, [(KITCHEN, "Kitchen")])
    win.quality_row.set_selected(1)  # "Sharper text"
    assert app.settings.bitrate == 40000
    app.controller.state = State.STREAMING
    win._refresh()
    assert not win.mode_row.get_sensitive()
    log = gui.LogDialog(app)
    log._append("another")
    assert log._text() == "a line of the log\nanother\n"


def test_the_tablets_camera_and_microphone(monkeypatch, tmp_path):
    win, app = make(monkeypatch, tmp_path, [(KITCHEN, "Kitchen")])
    camera = win.device_rows["camera"]
    assert not camera.get_active() and camera.get_subtitle() == "As a webcam for this PC's programs"
    camera.set_active(True)
    assert app.settings.camera
    app.devices.camera.state = "tablet-permission"
    win._refresh()
    assert camera.get_subtitle() == "On the tablet, allow Plus One to use the camera"
    win.quality_row_camera.set_selected(1)
    assert app.settings.camera_quality == "1080p"


def test_a_closed_pairing_dialog_answers_no_at_once(monkeypatch, tmp_path):
    # Closed before the code came: the tablet is not left asking for two minutes.
    _win, app = make(monkeypatch, tmp_path)
    dialog = gui.PairDialog(app)
    dialog.closed = True
    assert dialog._ask(dialog.attempt, "Tab", "123456") is False


def test_an_older_attempt_does_not_answer_the_new_one(monkeypatch, tmp_path):
    _win, app = make(monkeypatch, tmp_path)
    dialog = gui.PairDialog(app)
    dialog.attempt = 2  # another tablet chosen since attempt 1
    assert dialog._ask(1, "Old", "111111") is False


def test_the_quick_settings_toggle_is_offered_once(monkeypatch, tmp_path):
    win, app = make(monkeypatch, tmp_path, [(KITCHEN, "Kitchen")])
    turned = []
    monkeypatch.setattr(gui, "set_shell_extension", turned.append)
    toasts = []
    monkeypatch.setattr(win.toasts, "add_toast", toasts.append)
    win._offer_toggle(False)  # the shell has it, off
    assert app.settings.toggle_offered and len(toasts) == 1 and turned == []  # not on by itself
    toasts[0].emit("button-clicked")
    assert turned == [True]
    win._offer_toggle(False)  # asked once only
    assert len(toasts) == 1


def test_the_speakers_say_what_they_need():
    # Their "setup" is python3-cryptography too, not the microphone's text.
    assert "python3-cryptography" in gui.device_status("speakers", "setup")
    assert "python3-cryptography" not in gui.device_status("microphone", "setup")


def test_device_texts_are_not_markup(monkeypatch, tmp_path):
    # A program named "A&B" or the reload hint's "&&" must show, not fail to parse.
    win, app = make(monkeypatch, tmp_path, [(KITCHEN, "Kitchen")])
    app.devices.speakers.state, app.devices.speakers.users = "in use", ["Tom & Jerry <3>"]
    win._refresh()
    row = win.device_rows["speakers"]
    assert not row.get_use_markup() and row.get_subtitle() == "Playing Tom & Jerry <3>"


def test_what_changes_elsewhere_shows(monkeypatch, tmp_path):
    win, app = make(monkeypatch, tmp_path, [(KITCHEN, "Kitchen")])
    app.settings.background = not app.settings.background  # Quick Settings' toggle
    win._refresh()
    assert win.background_row.get_active() == app.settings.background
    # The first desktop through the app: its sizes offered from then on.
    assert win.mode_row.get_model().get_n_items() == 1
    app.settings.app_screen = "2880x1800"
    win._refresh()
    assert win.mode_row.get_model().get_n_items() > 1


def test_pairing_another_tablet_while_connected_keeps_the_one_in_use(monkeypatch, tmp_path):
    _win, app = make(monkeypatch, tmp_path, [(KITCHEN, "Kitchen")])
    app.controller.running = True
    new = "cccc" + "0" * 60
    app.tablets.paired.add(new, "New")
    paired = []
    dialog = gui.PairDialog(app, on_paired=paired.append)
    dialog._done(dialog.attempt, new, "New")
    assert paired == ["New"] and app.settings.tablet != new
