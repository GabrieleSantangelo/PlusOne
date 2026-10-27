"""Plus One kept up to date, on both sides.

This PC: the .deb's source (packaging/apt) makes apt, Software Updater and
unattended-upgrades install each release. A process running meanwhile
still has the old code (and would load new modules into it): SelfUpdate
sees its files change, and starts Plus One again once nothing is in use,
saying so.

The tablet: its app says its version in its mDNS record ("v"). Older than
this PC, the app of this PC's version goes to it over the paired link, from
this release on GitHub (its SHA-256 checked against the release's
SHA256SUMS), while nothing is in use; the app installs it (Android checks
it is signed as the app it replaces). Both sides stay at one version. It
goes over the link (plus-one-link: by cable too) to an app that serves
"update" there, else over the link of before.
"""
import hashlib
import os
import re
import sys
import threading
import time
import urllib.request

from gi.repository import Gio, GLib

from . import __version__, link, log, once
from .link import channels

RELEASES = "https://github.com/GabrieleSantangelo/PlusOne/releases/download"
MAX_APK = 64 << 20         # bytes: an app is 15 MB
DOWNLOAD_TIMEOUT = 60      # seconds without data
TRY_AGAIN = 10 * 60        # seconds after an attempt, whatever came of it
SETTLE = 5                 # seconds after the files changed: dpkg is done
UPDATED_FROM = "PLUSONE_UPDATED_FROM"  # set for the process started again


def update_app(targets, expect, apk, version, paired=None):
    """Blocking: hands the paired tablet's app (whatever its protocol) its
    update, the APK at apk of version (its versionCode). The app checks
    it (its package, newer, signed as itself) and installs it; None then,
    else its reason not to: "busy" (the PC uses it now), "off" (not
    allowed there), "invalid" or "older"."""
    with open(apk, "rb") as f:
        data = f.read()
    tablet = (channels.connect(expect, "update") if expect else None) or \
        link.connect(targets, expect, paired, any_version=True)
    with tablet:
        tablet.settimeout(link.IO_TIMEOUT * 4)  # 15 MB over a slow Wi-Fi
        try:
            reply = tablet.request({"type": "update", "version": version,
                                    "sha256": hashlib.sha256(data).hexdigest()}, data, limit=link.UNPAIRED_HEADER)
        except link.Refused as e:
            return str(e) or "invalid"
    # Only "updating" is: anything else is not the app installing it.
    return None if reply.get("type") == "updating" else str(reply.get("type") or "invalid")


def version_code(version):
    """The app's versionCode for a version (build.gradle.kts): 0.8.0 -> 800."""
    major, minor, patch = (int(part) for part in version.split(".")[:3])
    return major * 10000 + minor * 100 + patch


def installed_version(package_dir):
    """__version__ as the files say now (dpkg may have replaced them)."""
    try:
        with open(os.path.join(package_dir, "__init__.py")) as f:
            found = re.search(r'^__version__ = "([^"]+)"', f.read(), re.M)
    except OSError:
        return None
    return found.group(1) if found else None


def _get(url, limit):
    with urllib.request.urlopen(url, timeout=DOWNLOAD_TIMEOUT) as response:
        data = response.read(limit + 1)
    if len(data) > limit:
        raise ValueError(f"{url}: more than {limit} bytes")
    return data


def app_apk(version, folder, get=_get):
    """Blocking: the path of this version's app, downloaded from its release
    once and checked against the release's SHA256SUMS."""
    name = f"plus-one-companion_v{version}.apk"
    path = os.path.join(folder, name)
    if os.path.exists(path):
        return path
    sums = get(f"{RELEASES}/v{version}/SHA256SUMS", 1 << 16).decode()
    wanted = next((line.split()[0] for line in sums.splitlines()
                   if line.split()[1:] == [name] or line.split()[1:] == ["*" + name]), None)
    if not wanted:
        raise ValueError(f"{name} is not in the release's SHA256SUMS")
    data = get(f"{RELEASES}/v{version}/{name}", MAX_APK)
    if hashlib.sha256(data).hexdigest() != wanted:
        raise ValueError(f"{name} is not the one the release lists")
    os.makedirs(folder, mode=0o700, exist_ok=True)
    for old in os.listdir(folder):  # an older version's: of no more use
        if old.endswith(".apk"):
            os.remove(os.path.join(folder, old))
    with open(path + ".part", "wb") as f:
        f.write(data)
    os.replace(path + ".part", path)
    return path


class TabletUpdates:
    """The tablet in use's app brought to this PC's version, when it is on the
    network, older, and nothing is in use. app: the application (its
    settings, tablets, nearby, busy() and notify())."""

    def __init__(self, app, folder=None, fetch=app_apk, send=update_app):
        self.app = app
        self.folder = folder or os.path.join(GLib.get_user_cache_dir(), "plus-one", "updates")
        self.fetch, self.send = fetch, send
        self.code = version_code(__version__)
        self.tried = {}      # app id -> (its version then, monotonic time)
        self.working = False

    def check(self):
        """On the main loop, whenever the tablet or what is in use changed."""
        if self.working or not self.app.settings.update_tablet or self.app.busy():
            return
        tablet = self.app.tablets.active()
        if tablet is None:
            return
        version = self.app.discovery.app_version(tablet.app_id)
        # 0: an app before 0.8, which cannot take one (installed by hand once).
        if not version or version >= self.code:
            return
        tried = self.tried.get(tablet.app_id)
        if tried and tried[0] == version and time.monotonic() - tried[1] < TRY_AGAIN:
            return
        targets = self.app.tablets.targets(tablet)
        if not targets:
            return
        self.tried[tablet.app_id] = (version, time.monotonic())
        self.working = True
        threading.Thread(target=self._update, args=(tablet, targets), daemon=True).start()

    def _update(self, tablet, targets):
        try:
            apk = self.fetch(__version__, self.folder)
            log(f"updates: Plus One {__version__} to {tablet.name}")
            reason = self.send(targets, tablet.app_id, apk, self.code)
        except Exception as e:  # noqa: BLE001 - not there, no release, gone: next time
            log("updates: the tablet's app not updated:", repr(e))
            reason = "error"
        GLib.idle_add(once(self._done, tablet, reason))

    def _done(self, tablet, reason):
        self.working = False
        if reason is None:
            self.app.notify("updates", "Updating Plus One on {tablet}", "To {version}, as on this PC",
                            tablet=tablet.name, version=__version__)
        elif reason != "error":
            log(f"updates: {tablet.name} said {reason}")


class SelfUpdate:
    """This PC's Plus One replaced on disk (apt): started again once nothing
    is in use, without a window open. Only an installed one: from the
    sources, files change all day; a Flatpak is updated by Flatpak."""

    def __init__(self, app, package_dir=None):
        self.app = app
        self.dir = package_dir or os.path.dirname(os.path.abspath(__file__))
        self.pending = None   # the version installed, newer than this one
        self._monitor = self._settle = None

    def start(self):
        if not self.dir.startswith("/usr/"):
            return
        self._monitor = Gio.File.new_for_path(self.dir).monitor_directory(Gio.FileMonitorFlags.NONE, None)
        self._monitor.connect("changed", self._changed)

    def _changed(self, *_args):
        if self._settle:
            GLib.source_remove(self._settle)
        self._settle = GLib.timeout_add_seconds(SETTLE, self._look)

    def _look(self):
        self._settle = None
        version = installed_version(self.dir)
        if version and version != __version__ and version != self.pending:
            self.pending = version
            log(f"updates: Plus One {version} installed, this is {__version__}")
            self.app.notify("updates", "Plus One {version} is installed",
                            "It starts again by itself once nothing is in use", version=version,
                            button=("Restart now", "app.restart"))
            self.when_idle()
        return False

    def when_idle(self):
        """On the main loop: called as what is in use changes."""
        if self.pending and not self.app.busy() and not self.app.get_windows():
            self.restart()

    def restart(self):
        log(f"updates: starting Plus One {self.pending} in place of {__version__}")
        self.app.devices.shutdown()
        os.environ[UPDATED_FROM] = __version__
        sys.stdout.flush()
        sys.stderr.flush()
        os.execv(sys.executable, [sys.executable] + sys.orig_argv[1:])
