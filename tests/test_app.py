"""Background application helpers: settings file and autostart entry."""
import json

import pytest

from plusone.app import APP_ID, load_settings, set_autostart
from plusone.engine import Settings


def test_settings_from_older_versions(tmp_path):
    # 0.2 wrote "stay_ready"; unknown keys must not make the file unreadable.
    path = tmp_path / "settings.json"
    path.write_text(json.dumps({"bitrate": 15000, "stay_ready": False}))
    settings = load_settings(str(path))
    assert settings.bitrate == 15000
    assert settings.background is True


def test_unreadable_settings_fall_back_to_defaults(tmp_path):
    path = tmp_path / "settings.json"
    path.write_text("{not json")
    assert load_settings(str(path)) == Settings()


def test_autostart_entry(tmp_path):
    set_autostart(True, config_dir=tmp_path)
    entry = (tmp_path / "autostart" / f"{APP_ID}.desktop").read_text()
    assert "--gapplication-service" in entry
    assert "NoDisplay=true" in entry
    set_autostart(False, config_dir=tmp_path)
    assert not (tmp_path / "autostart" / f"{APP_ID}.desktop").exists()


def test_sign_entry_hides_the_system_one(tmp_path):
    from plusone.app import SIGN_ENTRY, set_sign_entry
    path = tmp_path / "applications" / SIGN_ENTRY
    set_sign_entry(True, data_dir=tmp_path)
    assert not path.exists()  # the system entry shows as it is
    set_sign_entry(False, data_dir=tmp_path)
    text = path.read_text()
    assert "Type=Application" in text and "Hidden=true" in text
    set_sign_entry(False, data_dir=tmp_path)
    assert path.read_text() == text
    set_sign_entry(True, data_dir=tmp_path)
    assert not path.exists()


def test_sign_entry_of_a_per_user_install_comes_back(tmp_path):
    from gi.repository import GLib

    from plusone.app import SIGN_ENTRY, set_sign_entry
    path = tmp_path / "applications" / SIGN_ENTRY
    path.parent.mkdir()
    path.write_text("[Desktop Entry]\nType=Application\nName=Sign on Tablet\nName[it]=Firma sul tablet\n"
                    "Exec=/home/me/plus-one/bin/plus-one --sign %F\nMimeType=application/pdf;\n")
    set_sign_entry(False, data_dir=tmp_path)
    entry = GLib.KeyFile()
    entry.load_from_file(str(path), GLib.KeyFileFlags.KEEP_TRANSLATIONS)
    assert entry.get_boolean("Desktop Entry", "Hidden")
    assert entry.get_string("Desktop Entry", "Exec") == "/home/me/plus-one/bin/plus-one --sign %F"
    set_sign_entry(True, data_dir=tmp_path)
    entry.load_from_file(str(path), GLib.KeyFileFlags.KEEP_TRANSLATIONS)
    assert "Hidden" not in entry.get_keys("Desktop Entry")[0]
    assert entry.get_locale_string("Desktop Entry", "Name", "it") == "Firma sul tablet"


def test_a_broken_settings_file_is_not_trusted(tmp_path):
    from plusone.engine import Settings
    path = tmp_path / "settings.json"
    for content in ("[]", "null", "3"):
        path.write_text(content)
        assert load_settings(str(path)) == Settings()
    # (fps, display_via: settings of Second Screen, gone since 0.9, ignored)
    path.write_text('{"background": "no", "bitrate": "9999; rm", "fps": true, "encoder": 3, '
                    '"tablet": 7, "display_via": "second-screen", "touch": false}')
    settings = load_settings(str(path))
    assert settings.background is True and settings.bitrate == Settings().bitrate
    assert settings.encoder == "auto" and settings.tablet is None and settings.touch is False


def test_settings_are_private(tmp_path, monkeypatch):
    import os
    from plusone import app
    from plusone.engine import Settings
    monkeypatch.setattr(app, "CONFIG", str(tmp_path / "plus-one" / "settings.json"))
    app.save_settings(Settings())
    assert os.stat(app.CONFIG).st_mode & 0o077 == 0


def test_settings_saved_by_two_threads_at_once_are_both_whole(tmp_path, monkeypatch):
    import os
    from plusone import app
    from plusone.engine import Settings
    monkeypatch.setattr(app, "CONFIG", str(tmp_path / "plus-one" / "settings.json"))
    replace = os.replace

    def another_thread_saves_meanwhile(src, dst):
        monkeypatch.setattr(os, "replace", replace)
        app.save_settings(Settings(bitrate=9000))
        replace(src, dst)
    monkeypatch.setattr(os, "replace", another_thread_saves_meanwhile)
    app.save_settings(Settings(bitrate=12000))
    assert load_settings(app.CONFIG).bitrate == 12000  # the last one replaced it, whole
    assert '\n  "bitrate": 12000' in open(app.CONFIG).read()
    assert os.listdir(tmp_path / "plus-one") == ["settings.json"]


def test_forgetting_the_tablet_in_use_ends_its_session(tmp_path):
    import types
    from plusone import app, link
    from plusone.engine import Settings
    from plusone.link import Tablets
    paired = link.Paired(str(tmp_path))
    kitchen, office = "abcd" * 16, "ef" * 32
    paired.add(kitchen, "Kitchen")
    paired.add(office, "Office")
    stopped = []
    fake = types.SimpleNamespace(tablets=Tablets(Settings(tablet=kitchen), paired=paired),
                                 controller=types.SimpleNamespace(stop=lambda: stopped.append(True)),
                                 devices=types.SimpleNamespace(apply=lambda: None), _on_nearby=lambda: None)
    app.PlusOneApp.forget_tablet(fake, office)
    assert stopped == [] and paired.all() == {kitchen: "Kitchen"}
    app.PlusOneApp.forget_tablet(fake, kitchen)
    assert stopped == [True] and paired.all() == {} and fake.tablets.active() is None


def test_exec_lines_survive_the_key_file(tmp_path):
    # A launcher path with what the Desktop Entry spec and key files escape.
    from gi.repository import GLib
    from plusone.app import exec_arg
    command = "/home/a b/$x`y\\z\"w%/bin/plus-one"
    keyfile = GLib.KeyFile()
    data = f"[Desktop Entry]\nExec={exec_arg(command)} --sign %F\n"
    keyfile.load_from_data(data, len(data.encode()), GLib.KeyFileFlags.NONE)
    exec_line = keyfile.get_string("Desktop Entry", "Exec")
    ok, argv = GLib.shell_parse_argv(exec_line.replace("%%", "%"))
    assert ok and argv[0] == command and argv[1:] == ["--sign", "%F"]


def replacing(monkeypatch, version, busy=None, state="idle"):
    """replace_older_instance with a Plus One of version running (pid 4242),
    no bus: what its Busy() and State answer (None: not known to it;
    app.BUSY: no answer in time). The signals it was sent, what it was asked."""
    import types
    from plusone import app
    owner, signals, asked = [4242], [], []

    def answer(name):
        asked.append(name)
        return {"Version": version, "Busy": busy, "State": state}[name]

    def kill(pid, number):
        signals.append((pid, number))
        owner.clear()
    monkeypatch.setattr(app, "IN_FLATPAK", False)
    monkeypatch.setattr(app, "Gio", types.SimpleNamespace(bus_get_sync=lambda *a: "bus", BusType=app.Gio.BusType))
    monkeypatch.setattr(app, "_owner_pid", lambda bus: owner[0] if owner else None)
    monkeypatch.setattr(app, "_running_property", lambda bus, name: answer(name))
    monkeypatch.setattr(app, "_running_call", lambda bus, interface, method, args=None: answer(method))
    monkeypatch.setattr(app.os, "kill", kill)
    app.replace_older_instance()
    return signals, asked


def test_an_older_plus_one_with_nothing_in_use_is_replaced(monkeypatch):
    import signal
    assert replacing(monkeypatch, "0.9.2", busy=False)[0] == [(4242, signal.SIGTERM)]
    # 0.3 said no version, and older ones nothing: their connection is all they tell.
    assert replacing(monkeypatch, None, busy=None, state="idle")[0] == [(4242, signal.SIGTERM)]


@pytest.mark.parametrize("version, busy, state", [
    ("0.9.2", True, "idle"),          # the camera, a PDF out for signing...: in use
    ("0.9.2", None, "streaming"),     # one before Busy(): connected
    ("0.9.2", "no answer", "idle"),
    ("0.10.0", False, "idle"),        # this version
    ("0.11.0", False, "idle"),        # a newer one: never replaced by an older
    ("0.10.0~rc1", False, "idle"),
])
def test_a_plus_one_newer_or_in_use_is_left_running(monkeypatch, version, busy, state):
    from plusone import app
    if busy == "no answer":
        busy = app.BUSY
    signals, asked = replacing(monkeypatch, version, busy, state)
    assert signals == []
    if not version.startswith("0.9"):
        assert asked == ["Version"]  # not even asked whether it is busy


def test_busy_is_told_over_d_bus():
    import types
    from plusone import app
    for busy in (True, False):
        returned = []
        running = types.SimpleNamespace(busy=lambda: busy)
        invocation = types.SimpleNamespace(return_value=returned.append)
        app.PlusOneApp._dbus_method(running, None, None, app.OBJECT_PATH, app.INTERFACE, "Busy", None, invocation)
        assert [value.unpack() for value in returned] == [(busy,)]
