"""Bluetooth as a path of the link's own ("bt"): slow, but there with no
network at all, over the bond the PC and the tablet made in their own
Bluetooth settings (nothing of Plus One's to pair).

The app listens on RFCOMM under Plus One's UUID while it runs (BtListener.kt:
with no network, nothing could ask it to); BlueZ connects to it for a
profile Plus One registered (ProfileManager1: BlueZ finds the channel in the
tablet's records and hands over the socket), and Plus One hands the socket
to plus-one-link ("stream"), which makes the session (or a path for it) on
it. BlueZ calls Profile1 methods on the main loop's context: Bluetooth.up()
runs on a thread of its own while that loop runs.
"""
import socket
import threading

from gi.repository import Gio, GLib

from .. import log

UUID = "5f1a3c9e-8b2d-4e6f-9a7c-0d1b2e3f4a5b"  # BtListener.kt's
BLUEZ = "org.bluez"
PROFILE_PATH = "/io/github/gabrielesantangelo/plusone/bt"
DEVICE = "org.bluez.Device1"
WAIT = 30  # seconds for BlueZ to connect

PROFILE_XML = """<node><interface name="org.bluez.Profile1">
  <method name="Release"/>
  <method name="NewConnection"><arg type="o" direction="in"/><arg type="h" direction="in"/>
    <arg type="a{sv}" direction="in"/></method>
  <method name="RequestDisconnection"><arg type="o" direction="in"/></method>
</interface></node>"""


class NoBluetooth(Exception):
    """No Bluetooth path with the tablet (why)."""


def bonded(bus, name):
    """The bonded device of that name (the tablet's, as its own Bluetooth
    settings call it): its object path, or None."""
    objects = bus.call_sync(BLUEZ, "/", "org.freedesktop.DBus.ObjectManager", "GetManagedObjects", None, None,
                            Gio.DBusCallFlags.NONE, 10_000, None).unpack()[0]
    for path, interfaces in objects.items():
        device = interfaces.get(DEVICE)
        if device and device.get("Paired") and name in (device.get("Name"), device.get("Alias")):
            return path
    return None


class Bluetooth:
    """The path, made with the tablet bonded under that Bluetooth name;
    link: a plusone_link.Link."""

    def __init__(self, name, link):
        self.name, self.link = name, link
        self.bus = Gio.bus_get_sync(Gio.BusType.SYSTEM, None)
        self.device = None
        self._registration = None
        self._connected = threading.Event()
        self._error = None

    def register(self):
        """On the main loop's thread: Plus One's profile, for BlueZ to call."""
        info = Gio.DBusNodeInfo.new_for_xml(PROFILE_XML)
        self._registration = self.bus.register_object(PROFILE_PATH, info.interfaces[0], self._call)
        options = {"Name": GLib.Variant("s", "Plus One"), "Role": GLib.Variant("s", "client"),
                   "AutoConnect": GLib.Variant("b", False)}
        self.bus.call_sync(BLUEZ, "/org/bluez", "org.bluez.ProfileManager1", "RegisterProfile",
                           GLib.Variant("(osa{sv})", (PROFILE_PATH, UUID, options)), None,
                           Gio.DBusCallFlags.NONE, 10_000, None)

    def _call(self, _bus, _sender, _path, _interface, method, parameters, invocation):
        if method == "NewConnection":
            _device, index, _props = parameters.unpack()
            fds = invocation.get_message().get_unix_fd_list()
            fd = fds.get(index) if fds is not None else -1
            invocation.return_value(None)
            if fd >= 0:
                threading.Thread(target=self._hand_over, args=(fd,), daemon=True).start()
        else:
            invocation.return_value(None)

    def _hand_over(self, fd):
        sock = socket.socket(fileno=fd)
        try:
            self.link.stream("bt", sock)
        except Exception as e:  # noqa: BLE001 - said by up()
            self._error = e
        finally:
            sock.close()  # plus-one-link has its own
            self._connected.set()

    def up(self):
        """Blocking (not on the main loop): until the session has the path."""
        self.device = bonded(self.bus, self.name)
        if self.device is None:
            raise NoBluetooth(f"{self.name!r} is not bonded with this PC (its Bluetooth settings)")
        try:
            self.bus.call_sync(BLUEZ, self.device, DEVICE, "ConnectProfile", GLib.Variant("(s)", (UUID,)), None,
                               Gio.DBusCallFlags.NONE, WAIT * 1000, None)
        except GLib.Error as e:
            raise NoBluetooth(f"BlueZ did not connect: {e.message}") from None
        if not self._connected.wait(WAIT):
            raise NoBluetooth("no socket from BlueZ")
        if self._error:
            raise NoBluetooth(f"plus-one-link: {self._error}")
        log(f"bt: a path by Bluetooth with {self.name}")

    def close(self):
        if self.device:
            try:
                self.bus.call_sync(BLUEZ, self.device, DEVICE, "DisconnectProfile", GLib.Variant("(s)", (UUID,)),
                                   None, Gio.DBusCallFlags.NONE, 5_000, None)
            except GLib.Error:
                pass
        try:
            self.bus.call_sync(BLUEZ, "/org/bluez", "org.bluez.ProfileManager1", "UnregisterProfile",
                               GLib.Variant("(o)", (PROFILE_PATH,)), None, Gio.DBusCallFlags.NONE, 5_000, None)
        except GLib.Error:
            pass
        if self._registration:
            self.bus.unregister_object(self._registration)
            self._registration = None
