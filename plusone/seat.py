"""Whether the desktop may be touched now (SEC-22): logind's session of
ours, active and unlocked. Locked, inactive (another user's session in
front) or not known: the tablet's fingers and pen reach nothing, whatever
touches is let go at once, and the tablet's camera and microphone pause;
inactive, the screen's capture pauses too. A stolen tablet must not tap at
the lock screen, nor type into another user's session.

Plus One runs as a D-Bus service of its user's, in no session of its own:
the session is its user's graphical one (logind's User.Display)."""
import os

from gi.repository import Gio, GLib

from . import log

LOGIN1 = "org.freedesktop.login1"
MANAGER = "/org/freedesktop/login1"
SESSION = "org.freedesktop.login1.Session"
PROPERTIES = "org.freedesktop.DBus.Properties"


class Seat:
    """on_change(open, active): on the main loop, at first and whenever it
    changes; open: active and unlocked. bus: the system bus (a test's)."""

    def __init__(self, on_change, bus=None):
        self.on_change = on_change
        self.open = self.active = self._told = False
        self.path = None
        try:
            self.bus = bus or Gio.bus_get_sync(Gio.BusType.SYSTEM, None)
            self.path = self._session()
        except GLib.Error as e:
            log(f"seat: no session of ours ({e.message}): the tablet's input stays off")
            self.on_change(False, False)
            return
        self.bus.signal_subscribe(LOGIN1, PROPERTIES, "PropertiesChanged", self.path, SESSION,
                                  Gio.DBusSignalFlags.NONE, lambda *args: self._read())
        self._read()

    def _call(self, path, interface, method, args, reply):
        return self.bus.call_sync(LOGIN1, path, interface, method, args, GLib.VariantType(reply),
                                  Gio.DBusCallFlags.NONE, 5000, None).unpack()

    def _session(self):
        """Our session's object path: this process's, else our user's
        graphical one."""
        try:
            return self._call(MANAGER, "org.freedesktop.login1.Manager", "GetSessionByPID",
                              GLib.Variant("(u)", (os.getpid(),)), "(o)")[0]
        except GLib.Error:
            user = self._call(MANAGER, "org.freedesktop.login1.Manager", "GetUser",
                              GLib.Variant("(u)", (os.getuid(),)), "(o)")[0]
            _id, path = self._call(user, PROPERTIES, "Get", GLib.Variant("(ss)", ("org.freedesktop.login1.User",
                                                                                  "Display")), "(v)")[0]
            if path == "/":
                raise GLib.Error("the user has no graphical session")
            return path

    def _read(self):
        try:
            values = self._call(self.path, PROPERTIES, "GetAll", GLib.Variant("(s)", (SESSION,)), "(a{sv})")[0]
            active, locked = bool(values["Active"]), bool(values["LockedHint"])
        except (GLib.Error, KeyError) as e:
            log(f"seat: the session's state unknown ({e}): the tablet's input off")
            active, locked = False, True
        open = active and not locked
        if not self._told or (open, active) != (self.open, self.active):
            self._told = True
            self.open, self.active = open, active
            log(f"seat: {'open' if open else 'locked' if active else 'inactive'}")
            self.on_change(open, active)
