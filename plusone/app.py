"""Plus One as a GNOME application that keeps running in the background.

One process holds the engine and offers it three ways: a D-Bus interface for
other front ends (the Quick Settings extension), notifications, and the
settings window, created only when asked for. With the background mode on it
starts at login (`--gapplication-service`, no window) and idles at no cost, so
the Quick Settings toggle connects at once.
"""
import collections
import json
import os
import re
import shutil
import signal
import sys
import threading
import time
from dataclasses import asdict
from pathlib import Path

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
gi.require_version("Gst", "1.0")
from gi.repository import Adw, Gio, GLib, Gst  # noqa: E402

from . import __version__, add_log_handler, i18n, log, once, wifi  # noqa: E402
from .engine import Controller, Settings, State  # noqa: E402
from .i18n import _  # noqa: E402
from .signing import Signing  # noqa: E402
from .devices import SPEAKERS_NAME, BluetoothAudio, Devices, make_default_sink  # noqa: E402
from .link import Discovery, Nearby, Tablets  # noqa: E402
from .link.paths import Paths  # noqa: E402
from .seat import Seat  # noqa: E402

try:
    import plusone_link
except ImportError:  # a Plus One without the link (docs/LINK.md)
    plusone_link = None
from .updates import UPDATED_FROM, SelfUpdate, TabletUpdates  # noqa: E402

APP_ID = "io.github.gabrielesantangelo.PlusOne"
INTERFACE = APP_ID + "1"
INTERFACE_XML = f"""
<node>
  <interface name="{INTERFACE}">
    <method name="Connect"/>
    <method name="Disconnect"/>
    <method name="ShowWindow"/>
    <method name="SignDocument">
      <arg type="s" name="path" direction="in"/>
    </method>
    <method name="Busy">
      <arg type="b" name="busy" direction="out"/>
    </method>
    <property name="Version" type="s" access="read"/>
    <property name="State" type="s" access="read"/>
    <property name="Active" type="b" access="read"/>
    <property name="Title" type="s" access="read"/>
    <property name="Detail" type="s" access="read"/>
    <property name="Background" type="b" access="readwrite"/>
    <property name="SignAvailable" type="b" access="read"/>
  </interface>
</node>"""
CONFIG = os.path.join(GLib.get_user_config_dir(), "plus-one", "settings.json")
IN_FLATPAK = os.path.exists("/.flatpak-info")
TITLES = {
    State.IDLE: "Ready",
    State.CONNECTING: "Connecting…",
    State.STREAMING: "Connected",
    State.ERROR: "Problem",
}
EXTENSION = "plus-one@gabrielesantangelo.github.io"
SIGN_ENTRY = f"{APP_ID}.Sign.desktop"
HIDDEN_KEY = "X-PlusOne-Hidden"
NOTIFICATION = "status"


def load_settings(path=CONFIG):
    """The settings saved, each of the type it should be (others left at
    their defaults: a hand-edited or broken file must not stop the app)."""
    try:
        with open(path) as f:
            data = json.load(f)
    except (OSError, ValueError):
        return Settings()
    if not isinstance(data, dict):
        return Settings()
    defaults = Settings()
    kept = {}
    for name, value in data.items():
        if name not in Settings.__dataclass_fields__:
            continue
        default = getattr(defaults, name)
        if isinstance(default, bool) or isinstance(default, str):
            ok = type(value) is type(default)
        elif isinstance(default, int):
            ok = type(value) is int and value > 0
        else:  # None by default: a string, or nothing
            ok = value is None or isinstance(value, str)
        if ok:
            kept[name] = value
    return Settings(**kept)


def save_settings(settings):
    os.makedirs(os.path.dirname(CONFIG), mode=0o700, exist_ok=True)
    # Replaced whole: the Files extension may read it meanwhile. Only ours to read.
    fd = os.open(CONFIG + ".part", os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump(asdict(settings), f, indent=2)
    os.replace(CONFIG + ".part", CONFIG)


def launcher():
    """Command that starts this installation of Plus One."""
    if IN_FLATPAK or shutil.which("plus-one"):
        return "plus-one"
    return str(Path(__file__).resolve().parents[1] / "bin" / "plus-one")


def exec_arg(command):
    """command as one argument of a desktop entry's Exec line: quoted, as
    the Desktop Entry spec wants a path with spaces (or a %) to be; then
    escaped again as a key file's string value (a backslash doubled)."""
    escaped = "".join("\\" + c if c in '"`$\\' else c for c in command).replace("%", "%%")
    return '"' + escaped.replace("\\", "\\\\") + '"'


def start_link():
    """plus-one-link's socket in this session, if it is installed and not
    there yet (a session opened before the package was: its socket is
    enabled from the next login on)."""
    if IN_FLATPAK or plusone_link is None or os.path.exists(plusone_link.socket_path()):
        return
    try:
        bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        bus.call_sync("org.freedesktop.systemd1", "/org/freedesktop/systemd1", "org.freedesktop.systemd1.Manager",
                      "StartUnit", GLib.Variant("(ss)", ("plus-one-link.socket", "replace")), None,
                      Gio.DBusCallFlags.NONE, 5000, None)
    except GLib.Error as e:
        log("link: plus-one-link's socket not started:", e.message)


def set_autostart(enabled, config_dir=None):
    """Start in the background at login: an XDG autostart entry, or the
    Background portal inside a Flatpak (which cannot write ~/.config/autostart)."""
    if IN_FLATPAK:
        bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        options = {"reason": GLib.Variant("s", _("Keeps the Quick Settings toggle ready to connect the tablet")),
                   "autostart": GLib.Variant("b", enabled),
                   "commandline": GLib.Variant("as", ["plus-one", "--gapplication-service"]),
                   "dbus-activatable": GLib.Variant("b", False)}
        bus.call("org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop",
                 "org.freedesktop.portal.Background", "RequestBackground",
                 GLib.Variant("(sa{sv})", ("", options)), None, Gio.DBusCallFlags.NONE, -1,
                 None, None, None)
        return
    path = Path(config_dir or GLib.get_user_config_dir()) / "autostart" / f"{APP_ID}.desktop"
    if not enabled:
        path.unlink(missing_ok=True)
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    # TryExec: once Plus One is removed, the session stops trying to start it.
    path.write_text("[Desktop Entry]\nType=Application\nName=Plus One\n"
                    f"TryExec={launcher()}\nExec={exec_arg(launcher())} --gapplication-service\nIcon={APP_ID}\n"
                    "NoDisplay=true\nX-GNOME-Autostart-enabled=true\n")


def set_sign_entry(shown, data_dir=None):
    """Show or hide the "Sign on Tablet" app (Open With, the app grid). An
    entry with Hidden=true in the user's applications folder hides the one of
    the same name installed system-wide; a per-user install (install.sh) has
    its only entry there, which is hidden and shown again in place."""
    if IN_FLATPAK:
        return  # the sandbox cannot write there
    path = Path(data_dir or GLib.get_user_data_dir()) / "applications" / SIGN_ENTRY
    entry = GLib.KeyFile()
    try:
        entry.load_from_file(str(path), GLib.KeyFileFlags.KEEP_COMMENTS
                             | GLib.KeyFileFlags.KEEP_TRANSLATIONS)
        ours = entry.get_string("Desktop Entry", HIDDEN_KEY)
    except GLib.Error:
        ours = None if path.exists() else "absent"
    if not shown and ours == "absent":
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("[Desktop Entry]\nType=Application\nName=Sign on Tablet\n"
                        f"TryExec={launcher()}\nExec={exec_arg(launcher())} --sign %F\nHidden=true\n"
                        f"{HIDDEN_KEY}=created\n")
    elif not shown and ours is None:
        entry.set_boolean("Desktop Entry", "Hidden", True)
        entry.set_string("Desktop Entry", HIDDEN_KEY, "edited")
        entry.save_to_file(str(path))
    elif shown and ours == "created":
        path.unlink()
    elif shown and ours == "edited":
        entry.remove_key("Desktop Entry", "Hidden")
        entry.remove_key("Desktop Entry", HIDDEN_KEY)
        entry.save_to_file(str(path))


def _shell_extensions_call(method, args, reply_type, callback):
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)

    def done(conn, res):
        try:
            result = conn.call_finish(res).unpack()
        except GLib.Error:
            result = None  # not GNOME Shell, or no access from a sandbox
        if callback:
            callback(result)

    bus.call("org.gnome.Shell.Extensions", "/org/gnome/Shell/Extensions", "org.gnome.Shell.Extensions",
             method, args, reply_type, Gio.DBusCallFlags.NONE, -1, None, done)


def shell_extension_state(callback):
    """callback(None) if the Quick Settings toggle is not installed (or the
    shell cannot tell), else callback(True/False) for enabled or not."""
    def got(result):
        info = result[0] if result else {}
        callback(info.get("state") == 1 if info else None)
    _shell_extensions_call("GetExtensionInfo", GLib.Variant("(s)", (EXTENSION,)),
                           GLib.VariantType("(a{sv})"), got)


def set_shell_extension(enabled):
    """Only ever on the user's explicit request: Plus One never turns shell
    extensions on by itself."""
    log("Quick Settings toggle", "on" if enabled else "off")
    _shell_extensions_call("EnableExtension" if enabled else "DisableExtension",
                           GLib.Variant("(s)", (EXTENSION,)), GLib.VariantType("(b)"), None)


class PlusOneApp(Adw.Application):
    def __init__(self):
        # Command line: `--sign [FILE...]`; D-Bus Open: files to sign.
        super().__init__(application_id=APP_ID,
                         flags=Gio.ApplicationFlags.HANDLES_COMMAND_LINE
                         | Gio.ApplicationFlags.HANDLES_OPEN)
        # Started by D-Bus activation (Quick Settings): stay long enough to
        # take the call that woke us up.
        self.set_inactivity_timeout(10000)
        self.settings = load_settings()
        # The system's language, as the Quick Settings toggle, Files and the
        # tablet have it: a choice of Plus One's own mixed languages.
        i18n.set_language("auto")
        # Every way of finding the tablets' apps: mDNS for now.
        self.discovery = Discovery([Nearby(on_change=self._on_nearby)])
        self.tablets = Tablets(self.settings, self.save_settings, self.discovery)
        # What the tablet is connected by (the link's events): in the status.
        self.paths = Paths(self._path_changed, on_link=self._link_up)
        self.controller = Controller(self.settings, self._on_state, self.tablets)
        self.listeners = []      # windows following the state
        self.log_listeners = []  # windows showing the log
        self.log_lines = collections.deque(maxlen=400)
        add_log_handler(lambda line: GLib.idle_add(once(self._on_log, line)))
        self._held = False
        self._notified = None  # the state the last notification was for
        self._registration = None
        for name, callback in (("connect", lambda *a: self.controller.start()),
                               ("disconnect", lambda *a: self.controller.stop()),
                               ("show", lambda *a: self.activate()),
                               ("pair", lambda *a: self._show_and_pair()),
                               ("quit", lambda *a: self._quit()),
                               ("wifi-latency", lambda *a: wifi.set_low_latency(True, lambda ok: None)),
                               ("restart", lambda *a: self.self_update.pending and self.self_update.restart()),
                               ("whats-new", lambda *a: Gio.AppInfo.launch_default_for_uri(
                                   f"https://github.com/GabrieleSantangelo/PlusOne/releases/tag/v{__version__}", None))):
            action = Gio.SimpleAction.new(name, None)
            action.connect("activate", callback)
            self.add_action(action)
        self.signing = Signing(self)
        self.tablet_updates = TabletUpdates(self)
        self.self_update = SelfUpdate(self)
        self._device_states = {}  # the camera's, the microphone's and the speakers', as last notified
        self.devices = Devices(self)

    # -- GApplication ------------------------------------------------------
    def do_startup(self):
        Adw.Application.do_startup(self)
        Gst.init(None)
        # GNOME's usual keys: preferences, close the window, quit (in the background too).
        for action, keys in (("win.preferences", ["<Control>comma"]), ("window.close", ["<Control>w"]),
                             ("app.quit", ["<Control>q"])):
            self.set_accels_for_action(action, keys)
        if self.settings.background:
            set_autostart(True)
        set_sign_entry(self.settings.sign_on_tablet)
        start_link()
        # Which tablets have the app, and where: for pairing, the desktop,
        # signing and the devices. Avahi listens anyway.
        self.discovery.start()
        # Turned off by a Plus One that did not live to turn it back. Here,
        # not as Devices are made: a second Plus One of a moment (Sign with
        # Plus One) makes them too, and turned it on under the speakers.
        BluetoothAudio.restore()
        self.devices.apply()
        # The desktop locked or another user's in front: the tablet's input
        # reaches nothing, its camera and microphone pause (SEC-22).
        self.seat = Seat(self._seat)
        self._offer_low_latency()
        self._update_hold()
        self.self_update.start()
        updated_from = os.environ.pop(UPDATED_FROM, None)
        if updated_from:  # started again by SelfUpdate: say what came
            self.notify("updates", "Plus One updated to {version}", "From {before}",
                        version=__version__, before=updated_from,
                        button=("What's new", "app.whats-new"))

    def _offer_low_latency(self):
        """Once, at the first start: the Wi-Fi's power saving delays the
        tablet; turning it off takes the user's password (Preferences turn it
        on again)."""
        if self.settings.wifi_latency_asked or wifi.low_latency():
            return
        self.settings.wifi_latency_asked = True
        self.save_settings()

        def check():
            if wifi.saving():
                GLib.idle_add(once(lambda: self.notify(
                    "wifi", "The Wi-Fi's power saving delays the tablet",
                    "15-30 ms more on each packet. Turning it off takes your password; Preferences turn it on "
                    "again.", button=("Turn it off", "app.wifi-latency"))))
        threading.Thread(target=check, name="plusone-wifi-check", daemon=True).start()

    def _seat(self, open, active):
        """seat.py: the desktop's session locked, inactive, or open again."""
        self.controller.gate(not open, active)
        self.devices.gate(not open)

    def _quit(self):
        """Ctrl+Q, the extension's Quit: each signing window closed as by its
        ✕ (a signature drawn is asked about, one being saved finishes), then
        Plus One, unless one stayed."""
        from .sign import SignWindow
        for window in [w for w in self.get_windows() if isinstance(w, SignWindow)]:
            window.close()
        GLib.idle_add(once(lambda: any(isinstance(w, SignWindow) for w in self.get_windows()) or self.quit()))

    def do_shutdown(self):
        self.devices.shutdown()  # the microphone's source goes with Plus One
        Adw.Application.do_shutdown(self)

    def do_activate(self):
        self._window().present()

    def _window(self):
        from .gui import Window
        return next((w for w in self.get_windows() if isinstance(w, Window)), None) or Window(self)

    def _show_and_pair(self):
        window = self._window()
        window.present()
        window.pair()

    def do_command_line(self, cmdline):
        args = cmdline.get_arguments()[1:]
        if args[:1] == ["--sign"]:
            paths = [cmdline.create_file_for_arg(arg).get_path() for arg in args[1:]]
            for path in filter(None, paths):
                self.signing.request(path)
            if not args[1:]:
                self.choose_and_sign()
        else:
            self.activate()
        return 0

    def do_open(self, files, n_files, hint):
        for file in files:
            if file.get_path():
                self.signing.request(file.get_path())

    def choose_and_sign(self, parent=None):
        from gi.repository import Gtk
        pdfs = Gtk.FileFilter(name="PDF")
        pdfs.add_mime_type("application/pdf")
        filters = Gio.ListStore.new(Gtk.FileFilter)
        filters.append(pdfs)
        dialog = Gtk.FileDialog(title=_("Choose the PDFs to sign"), filters=filters)
        self.hold()  # the portal dialog is not one of our windows

        def chosen(dialog, result):
            self.release()
            try:
                files = dialog.open_multiple_finish(result)
            except GLib.Error:
                return  # cancelled
            for file in files or []:
                if file.get_path():
                    self.signing.request(file.get_path())
        dialog.open_multiple(parent, None, chosen)

    def do_dbus_register(self, connection, object_path):
        Adw.Application.do_dbus_register(self, connection, object_path)
        info = Gio.DBusNodeInfo.new_for_xml(INTERFACE_XML).interfaces[0]
        reg = connection.register_object(object_path, info, self._dbus_method,
                                         self._dbus_get, self._dbus_set)
        self._registration = (connection, object_path, reg)
        return True

    def do_dbus_unregister(self, connection, object_path):
        if self._registration:
            connection.unregister_object(self._registration[2])
            self._registration = None
        Adw.Application.do_dbus_unregister(self, connection, object_path)

    def _on_log(self, line):
        self.log_lines.append(line)
        for listener in list(self.log_listeners):
            listener(line)

    def window_closed(self):
        # Without the background mode, closing the window ends Plus One.
        if not self.settings.background:
            self.controller.stop()
        self._idle()  # an update may have waited for the window

    def save_settings(self):
        save_settings(self.settings)

    # -- state -------------------------------------------------------------
    def status_text(self):
        """(title, detail) for the current state, in the current language;
        while connected, what by (the cable, the Wi-Fi...)."""
        state = self.controller.state
        template, args = self.controller.detail
        detail = _(template).format(**args) if template else ""
        title = _(TITLES[state])
        by = self.connected_by()
        if by and state in (State.CONNECTING, State.STREAMING):
            title = _("{state} · {path}").format(state=title, path=_(by))
            detail = _("{detail} · {path}").format(detail=detail, path=_(by)) if detail else _(by)
        return title, detail

    def connected_by(self):
        """What the tablet in use is connected by now: "cable", "Wi-Fi"...,
        "reconnecting…"; "Wi-Fi" over the link of before; None if not connected."""
        tablet = self.tablets.active()
        if tablet is None:
            return None
        by = self.paths.path(tablet.fingerprint)
        if by is None and self.controller.state == State.STREAMING:
            return "Wi-Fi"  # the link of before goes over the network only
        return by

    def _link_up(self, link):
        """plus-one-link there (again: it may have restarted): our settings to it."""
        try:
            link.use_usb(self.settings.cable)
        except (OSError, ValueError, plusone_link.LinkError):
            pass

    def set_cable(self, enabled):
        if enabled == self.settings.cable:
            return
        self.settings.cable = enabled
        save_settings(self.settings)
        if plusone_link is not None:
            def tell():
                try:
                    with plusone_link.Link() as link:
                        link.use_usb(enabled)
                except (OSError, ValueError, plusone_link.LinkError):
                    pass  # not there: told when it comes (_link_up)
            threading.Thread(target=tell, daemon=True).start()

    def _path_changed(self):
        self._emit_properties()
        for listener in list(self.listeners):
            listener()

    def _on_state(self, state, template, args):
        self._update_hold()
        self._notify(state, template)
        self._emit_properties()
        for listener in list(self.listeners):
            listener()
        if state in (State.IDLE, State.ERROR):
            self._idle()

    def busy(self):
        """Whether something is in use: a connection, a PDF being signed, the
        tablet's camera, microphone or speakers used by a program here."""
        signing = self.signing
        return bool(self.controller.running or signing.windows or signing.in_app
                    or any(getattr(self.devices, kind).state == "in use"
                           for kind in ("camera", "microphone", "speakers")))

    def _idle(self):
        """What waited for nothing to be in use: updates, both sides."""
        GLib.idle_add(once(self.tablet_updates.check))
        GLib.idle_add(once(self.self_update.when_idle))

    def notify(self, key, title, body, button=None, **args):
        """A notification of English templates (i18n) formatted with args;
        button: (label, action)."""
        note = Gio.Notification.new(_(title).format(**args))
        note.set_body(_(body).format(**args))
        note.set_default_action("app.show")
        if button:
            note.add_button(_(button[0]), button[1])
        self.send_notification(key, note)

    def _update_hold(self):
        # Stay without a window while connected, or always in background mode
        # (idle costs nothing: the radio is only used while connecting), or
        # while the tablet's camera, microphone or speakers are the PC's.
        want = (self.settings.background or self.controller.running
                or self.settings.camera or self.settings.microphone or self.settings.speakers)
        if want and not self._held:
            self.hold()
        elif not want and self._held:
            self.release()
        self._held = want

    def _notify(self, state, template):
        # Paused and shown again (the tablet's screen off and on): still the
        # same connection, not a banner each time.
        again, self._notified = state == State.STREAMING == self._notified, state
        if again:
            return
        title, detail = self.status_text()
        if state == State.STREAMING:
            note = Gio.Notification.new(_("Tablet connected"))
            note.set_body(detail)
            note.add_button(_("Disconnect"), "app.disconnect")
        elif state == State.ERROR or (state == State.IDLE and template):
            # A title that says what happened (not "Ready"), and the one button that helps.
            note = Gio.Notification.new(title if state == State.ERROR else _("The second screen ended"))
            note.set_body(detail)
            if template == "Pair a tablet first":
                note.add_button(_("Pair a tablet"), "app.pair")
            else:
                note.add_button(_("Try again"), "app.connect")
        else:
            self.withdraw_notification(NOTIFICATION)
            return
        note.set_default_action("app.show")
        self.send_notification(NOTIFICATION, note)

    def set_background(self, enabled):
        if enabled == self.settings.background:
            return
        self.settings.background = enabled
        save_settings(self.settings)
        set_autostart(enabled)
        self._update_hold()
        self._emit_properties()
        for listener in list(self.listeners):  # the window's switch, if set from Quick Settings
            listener()

    def use_tablet(self, fingerprint):
        """The paired tablet that gets the desktop and the PDFs from now on."""
        self.tablets.use(fingerprint)
        self.devices.apply()
        self._on_nearby()

    def set_device(self, name, on):
        """The tablet's camera, microphone or speakers (name: the setting) as the PC's, or not."""
        if getattr(self.settings, name) == on:
            return
        setattr(self.settings, name, on)
        save_settings(self.settings)
        self.devices.apply()
        if name == "speakers" and on and self.devices.speakers.module:
            # Turned on by hand: the PC's sound there now, one click. (Not
            # at login: which output plays stays the user's to choose.)
            make_default_sink(SPEAKERS_NAME)
        self._update_hold()

    def set_camera_options(self, facing, quality):
        changed = quality != self.settings.camera_quality
        self.settings.camera_facing, self.settings.camera_quality = facing, quality
        save_settings(self.settings)
        if changed:
            self.devices.restart_camera()  # the device's picture size with it

    def set_microphone_noise(self, noise):
        self.settings.microphone_noise = noise
        save_settings(self.settings)
        self.devices.microphone.renew()

    def devices_changed(self):
        for listener in list(self.listeners):
            listener()
        # The tablet did not let a program have its camera or microphone: the
        # app there asks its user now; this says to look at the tablet.
        for kind in ("camera", "microphone", "speakers"):
            state = getattr(self.devices, kind).state
            if state == self._device_states.get(kind):
                continue
            self._device_states[kind] = state
            if state in ("tablet-off", "tablet-permission"):
                note = Gio.Notification.new({"camera": _("Allow the camera on the tablet"),
                                             "microphone": _("Allow the microphone on the tablet"),
                                             "speakers": _("Allow the speakers on the tablet")}[kind])
                note.set_body(_("A program wants it: tap Plus One's notification on the tablet"))
                note.set_default_action("app.show")
                self.send_notification(f"device-{kind}", note)
            else:
                self.withdraw_notification(f"device-{kind}")
        if hasattr(self, "devices") and not self.busy():  # (not while Devices is being made)
            self._idle()

    def forget_tablet(self, fingerprint):
        """This PC forgets a paired tablet: using it again takes pairing again."""
        active = self.tablets.active()
        if active and active.fingerprint == fingerprint:
            self.controller.stop()  # its session, if it has one, ends with it
        self.tablets.forget(fingerprint)
        self.devices.apply()
        self._on_nearby()

    def set_share_signatures(self, enabled):
        """The saved signatures go to the tablet's app and come from it (signing.py)."""
        if enabled != self.settings.share_signatures:
            self.settings.share_signatures = enabled
            save_settings(self.settings)

    def set_sign_on_tablet(self, enabled):
        if enabled == self.settings.sign_on_tablet:
            return
        self.settings.sign_on_tablet = enabled
        save_settings(self.settings)
        set_sign_entry(enabled)
        self._on_nearby()

    def sign_available(self):
        """Whether Files should offer "Sign on Tablet" now: switched on, a
        tablet paired, and one connected or on the network (or no telling)."""
        return self.settings.sign_on_tablet and self.tablets.active() is not None and (
            self.controller.state in (State.CONNECTING, State.STREAMING) or self.discovery.present())

    def _on_nearby(self):
        self.devices.apply()  # the speakers come and go with the tablet
        self._emit_properties()
        for listener in list(self.listeners):
            listener()
        self.tablet_updates.check()  # the tablet's app, older than this PC's?

    # -- D-Bus -------------------------------------------------------------
    def _properties(self):
        title, detail = self.status_text()
        return {"Version": GLib.Variant("s", __version__),
                "State": GLib.Variant("s", self.controller.state),
                "Active": GLib.Variant("b", self.controller.running),
                "Title": GLib.Variant("s", title),
                "Detail": GLib.Variant("s", detail),
                "Background": GLib.Variant("b", self.settings.background),
                "SignAvailable": GLib.Variant("b", self.sign_available())}

    def _emit_properties(self):
        if self._registration:
            connection, path, _reg = self._registration
            connection.emit_signal(None, path, "org.freedesktop.DBus.Properties", "PropertiesChanged",
                                   GLib.Variant("(sa{sv}as)", (INTERFACE, self._properties(), [])))

    def _dbus_method(self, connection, sender, path, interface, method, params, invocation):
        if method == "Busy":  # a newer Plus One starting: whether to leave this one running
            invocation.return_value(GLib.Variant("(b)", (self.busy(),)))
            return
        try:
            if method == "Connect":
                self.controller.start()
            elif method == "Disconnect":
                self.controller.stop()
            elif method == "ShowWindow":
                self.activate()
            elif method == "SignDocument":
                self.signing.request(params.unpack()[0])
        finally:
            invocation.return_value(None)  # the caller waits for it, whatever happened

    def _dbus_get(self, connection, sender, path, interface, prop):
        return self._properties()[prop]

    def _dbus_set(self, connection, sender, path, interface, prop, value):
        if prop == "Background":
            self.set_background(value.unpack())
            return True
        return False


OBJECT_PATH = "/" + APP_ID.replace(".", "/")


def _owner_pid(bus):
    try:
        return bus.call_sync("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
                             "GetConnectionUnixProcessID", GLib.Variant("(s)", (APP_ID,)), None,
                             Gio.DBusCallFlags.NONE, 2000, None).unpack()[0]
    except GLib.Error:
        return None


BUSY = object()  # the running instance did not answer in time


def _running_call(bus, interface, method, args=None):
    try:
        return bus.call_sync(APP_ID, OBJECT_PATH, interface, method, args, None,
                             Gio.DBusCallFlags.NO_AUTO_START, 2000, None).unpack()[0]
    except GLib.Error as e:
        if e.matches(Gio.io_error_quark(), Gio.IOErrorEnum.TIMED_OUT) or \
                e.matches(Gio.dbus_error_quark(), Gio.DBusError.NO_REPLY):
            return BUSY
        return None  # 0.3 had no Version; older releases no interface at all


def _running_property(bus, name):
    return _running_call(bus, "org.freedesktop.DBus.Properties", "Get", GLib.Variant("(ss)", (INTERFACE, name)))


def _older(version):
    """Whether a running Plus One of version (None: one too old to say) is
    older than this one: 0.9.2 is, 0.10.0 and 0.11.0 are not."""
    def numbers(version):
        return tuple(int(re.match(r"\d*", part).group() or 0) for part in version.split("."))
    return version is None or numbers(version) < numbers(__version__)


def replace_older_instance():
    """After an upgrade the previous version may still run in the background,
    and GApplication would hand it requests it does not know (like --sign).
    Stop it, unless something is in use there (a connection, a PDF out for
    signing, the tablet's camera, microphone or speakers). One left running,
    or a newer one, takes this start's request as any Plus One running does
    (SelfUpdate starts an older one again once nothing is in use)."""
    if IN_FLATPAK:
        return  # its PID would be the host's: not one to signal from here
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    pid = _owner_pid(bus)
    if pid is None:
        return
    version = _running_property(bus, "Version")
    if version is not BUSY and not _older(version):
        return  # this version, or a newer one
    busy = version if version is BUSY else _running_call(bus, INTERFACE, "Busy")
    if busy is None:  # one before Busy: its connection is all it tells
        state = _running_property(bus, "State")
        busy = BUSY if state is BUSY else state in ("connecting", "streaming")
    if busy is BUSY:
        log("Plus One is running but busy; leaving it running")
        return
    if busy:
        log(f"Plus One {version or '(older)'} is in use; leaving it running")
        return
    log(f"replacing Plus One {version or '(older)'} (pid {pid}) with {__version__}")
    try:
        os.kill(pid, signal.SIGTERM)
    except OSError:
        return
    for _attempt in range(30):
        if _owner_pid(bus) is None:
            return
        time.sleep(0.1)


def main(argv=None):
    log("Plus One", sys.argv[1:] or "")
    replace_older_instance()
    app = PlusOneApp()
    # On logout (or replace_older_instance's SIGTERM): do_shutdown runs, and
    # takes the microphone's source with it, not left in the sound settings.
    try:  # GLib >= 2.80 moved it; the old name warns in newer PyGObject (as __main__)
        gi.require_version("GLibUnix", "2.0")
        from gi.repository import GLibUnix
        signal_add = GLibUnix.signal_add
    except (ValueError, ImportError, AttributeError):
        signal_add = GLib.unix_signal_add
    for number in (signal.SIGTERM, signal.SIGINT):
        signal_add(GLib.PRIORITY_DEFAULT, number, lambda: app.quit() or False)
    return app.run(argv if argv is not None else sys.argv[:1])
