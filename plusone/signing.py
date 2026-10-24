"""Sign a PDF on the tablet in one step.

Connects the tablet if needed, opens the signing window full screen on the
tablet's monitor as soon as it appears, reports the result with a
notification, and disconnects again if the connection was opened for this.
If the tablet does not come, the notification offers signing on the PC.
"""
import os

from gi.repository import Gio, GLib

from . import log, once
from .engine import NOT_ANSWERING, State
from .i18n import _

MONITOR_WAIT = 5  # seconds for the tablet's monitor to appear once streaming
NOTIFICATION = "sign"


class Signing:
    def __init__(self, app):
        self.app = app
        self.queue = []            # paths waiting for the tablet
        self.opening = set()       # paths waiting for the tablet's monitor
        self.windows = []
        self.connected_for_us = False
        for name, callback in (("sign-here", self._sign_here), ("open-file", self._open_file),
                               ("show-file", self._show_file)):
            action = Gio.SimpleAction.new(name, GLib.VariantType("s"))
            action.connect("activate", callback)
            app.add_action(action)

    def request(self, path):
        path = os.path.abspath(path)
        log("signing", path)
        # Asked again while the tablet takes its time: one window per
        # document, or Done would leave the others open, and the tablet with them.
        for window in self.windows:
            if window.path == path:
                window.present()
                return
        if path in self.opening:
            return
        controller = self.app.controller
        if controller.state == State.STREAMING:
            self._open_on_tablet(path)
            return
        if path not in self.queue:
            self.queue.append(path)
        if not controller.running:
            self.connected_for_us = True
            controller.start()
        self._notify_waiting(path, _("Tap “Second screen” on the tablet to sign {name}")
                             .format(name=os.path.basename(path)))

    def _notify_waiting(self, path, body):
        note = Gio.Notification.new(_("Sign on the tablet"))
        note.set_body(body)
        note.add_button_with_target(_("Sign on this PC"), "app.sign-here", GLib.Variant("s", path))
        self.app.send_notification(NOTIFICATION, note)

    def on_state(self, state):
        """Returns True if it told the user about this state itself."""
        if not self.queue:
            return False
        if state == State.STREAMING:
            queue, self.queue = self.queue, []
            self.app.withdraw_notification(NOTIFICATION)
            for path in queue:
                self._open_on_tablet(path)
        elif state == State.CONNECTING and self.app.controller.detail[0] == NOT_ANSWERING:
            self._notify_waiting(self.queue[-1], _(NOT_ANSWERING))
            return True
        elif state in (State.IDLE, State.ERROR) and not self.app.controller.running:
            # Gave up, failed or was stopped: offer the PC instead.
            queue, self.queue = self.queue, []
            self.connected_for_us = False
            for path in queue:
                note = Gio.Notification.new(_("The tablet did not connect"))
                note.set_body(_("You can sign {name} on this PC instead")
                              .format(name=os.path.basename(path)))
                note.add_button_with_target(_("Sign on this PC"), "app.sign-here",
                                            GLib.Variant("s", path))
                self.app.send_notification(NOTIFICATION, note)
            return True
        return False

    # -- windows ---------------------------------------------------------------
    def _open_on_tablet(self, path, tries=0):
        from .sign import tablet_monitor
        self.opening.add(path)
        monitor = tablet_monitor()
        if monitor is None and tries < MONITOR_WAIT * 4:
            GLib.timeout_add(250, once(self._open_on_tablet, path, tries + 1))
            return
        self._open(path, monitor)

    def _open(self, path, monitor=None):
        from .sign import SignWindow
        self.opening.discard(path)
        try:
            window = SignWindow(self.app, path, lambda result: self._done(window, path, result))
        except GLib.Error as e:
            note = Gio.Notification.new(_("Cannot open {name}").format(name=os.path.basename(path)))
            note.set_body(e.message)
            self.app.send_notification(NOTIFICATION, note)
            self._maybe_disconnect()
            return
        self.windows.append(window)
        if monitor is not None:
            window.fullscreen_on_monitor(monitor)
        else:
            window.maximize()
        window.present()

    def _done(self, window, path, result):
        if window in self.windows:
            self.windows.remove(window)
        if result:
            log("signed:", result)
            note = Gio.Notification.new(_("Signed"))
            note.set_body(os.path.basename(result))
            note.set_default_action_and_target("app.open-file", GLib.Variant("s", result))
            note.add_button_with_target(_("Open"), "app.open-file", GLib.Variant("s", result))
            note.add_button_with_target(_("Show in folder"), "app.show-file", GLib.Variant("s", result))
            self.app.send_notification(NOTIFICATION, note)
        # Let the window go away first; the display it is on may follow.
        GLib.idle_add(once(self._maybe_disconnect))

    def _maybe_disconnect(self):
        if self.connected_for_us and not self.windows and not self.queue and not self.opening:
            self.connected_for_us = False
            self.app.controller.stop()

    # -- notification actions --------------------------------------------------
    def _sign_here(self, _action, param):
        path = param.get_string()
        if path in self.queue:
            self.queue.remove(path)
        self.app.withdraw_notification(NOTIFICATION)
        if self.connected_for_us and not self.queue and self.app.controller.state != State.STREAMING:
            # Stop searching for the tablet: the radio is not needed any more.
            self.connected_for_us = False
            self.app.controller.stop()
        self._open(path)

    def _open_file(self, _action, param):
        Gio.AppInfo.launch_default_for_uri(Gio.File.new_for_path(param.get_string()).get_uri(), None)

    def _show_file(self, _action, param):
        uri = Gio.File.new_for_path(param.get_string()).get_uri()
        Gio.bus_get_sync(Gio.BusType.SESSION, None).call(
            "org.freedesktop.FileManager1", "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1", "ShowItems", GLib.Variant("(ass)", ([uri], "")),
            None, Gio.DBusCallFlags.NONE, -1, None, None, None)
