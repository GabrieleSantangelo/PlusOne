"""Plus One kept up to date: this PC (started again after apt replaced it)
and the tablet's app (handed this PC's version)."""
import hashlib
import os
import types

import pytest

from plusone import __version__, updates

APK = f"plus-one-companion_v{__version__}.apk"


def test_version_codes_as_the_app_s():
    assert updates.version_code("0.8.0") == 800
    assert updates.version_code("1.2.3") == 10203
    assert updates.version_code("0.7.0") < updates.version_code("0.7.1") < updates.version_code("0.10.0")


def release(apk=b"an app", listed=None):
    """get() of a release with this APK, its SHA256SUMS listing listed (or the right sum)."""
    sums = f"{listed or hashlib.sha256(apk).hexdigest()}  {APK}\n0000  other.deb\n".encode()
    asked = []

    def get(url, limit):
        asked.append(url)
        return sums if url.endswith("/SHA256SUMS") else apk
    return get, asked


def test_the_app_is_downloaded_once_and_checked(tmp_path):
    get, asked = release()
    path = updates.app_apk(__version__, str(tmp_path), get=get)
    assert open(path, "rb").read() == b"an app" and os.path.basename(path) == APK
    assert asked == [f"{updates.RELEASES}/v{__version__}/SHA256SUMS", f"{updates.RELEASES}/v{__version__}/{APK}"]
    assert updates.app_apk(__version__, str(tmp_path), get=get) == path and len(asked) == 2  # kept


def test_an_app_that_is_not_the_release_s_is_refused(tmp_path):
    get, _asked = release(listed="00" * 32)
    with pytest.raises(ValueError):
        updates.app_apk(__version__, str(tmp_path), get=get)
    assert not os.listdir(tmp_path)


class App:
    def __init__(self, version, busy=False):
        tablet = types.SimpleNamespace(app_id="00ff", name="Tab")
        self.settings = types.SimpleNamespace(update_tablet=True)
        self.tablets = types.SimpleNamespace(active=lambda: tablet, targets=lambda t: [("10.0.0.7", 47236)])
        self.discovery = types.SimpleNamespace(app_version=lambda app_id: version)
        self.busy = lambda: busy
        self.notes = []
        self.notify = lambda *args, **kwargs: self.notes.append(args[1])


def run(app):
    sent = []

    def send(targets, app_id, apk, code):
        sent.append((targets, app_id, apk, code))
        return None
    tablet = updates.TabletUpdates(app, folder="/nowhere", fetch=lambda version, folder: "/nowhere/app.apk",
                                   send=send)
    tablet.check()
    while tablet.working:  # its thread, then its answer on the main loop
        from gi.repository import GLib
        GLib.MainContext.default().iteration(True)
    return tablet, sent


def test_an_older_app_gets_this_pc_s_version():
    code = updates.version_code(__version__)
    app = App(code - 1)
    tablet, sent = run(app)
    assert sent == [([("10.0.0.7", 47236)], "00ff", "/nowhere/app.apk", code)]
    assert app.notes == ["Updating Plus One on {tablet}"]
    tablet.check()  # asked once, not again and again
    assert len(sent) == 1


@pytest.mark.parametrize("version, busy", [
    (updates.version_code(__version__), False),  # the same
    (updates.version_code(__version__) + 1, False),  # newer (never back)
    (0, False),  # before 0.8: cannot take it
    (None, False),  # not on the network
    (1, True),  # the PC uses it now
])
def test_no_update_when_it_cannot_or_need_not(version, busy):
    _tablet, sent = run(App(version, busy))
    assert sent == []


def test_this_pc_s_files_replaced_are_noticed(tmp_path, monkeypatch):
    (tmp_path / "__init__.py").write_text('__version__ = "9.9.9"\n')
    started = []
    app = types.SimpleNamespace(busy=lambda: True, get_windows=lambda: [], notes=[])
    app.notify = lambda *args, **kwargs: app.notes.append(kwargs.get("version"))
    watcher = updates.SelfUpdate(app, package_dir=str(tmp_path))
    monkeypatch.setattr(watcher, "restart", lambda: started.append(watcher.pending))
    watcher._look()
    assert watcher.pending == "9.9.9" and app.notes == ["9.9.9"] and started == []  # in use: later
    app.busy = lambda: False
    watcher.when_idle()
    assert started == ["9.9.9"]
    (tmp_path / "__init__.py").write_text(f'__version__ = "{__version__}"\n')
    watcher.pending = None
    watcher._look()  # the same version: nothing
    assert watcher.pending is None
