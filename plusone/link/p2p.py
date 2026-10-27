"""Wi-Fi Direct with the tablet's app: a path of the link's own ("p2p"),
each packet once through the air, not twice through an access point or a
phone's hotspot, and no one else's queue in between.

Over the link the PC asks the app's "p2p" feature (P2pHandler.kt) with its
own Wi-Fi Direct address; the app finds the PC among its peers and says
its name there (Android keeps its own address from apps); the PC starts
NetworkManager's connection to it ("p2p-go"), then the app connects as the
group owner and says the group is up. plus-one-link then adds a path in
the group (192.168.49.1, the owner's, Android's always), better than the
LAN's: the session moves to it. The group lasts while the feature's channel
does (Group.close(), or Plus One gone: the connection is bound to it).

Only a radio of its own (a second adapter): a group on the radio that is
on a network already crashed Intel's AX211 firmware (89) as it joined, on
5 GHz and 2.4 alike (docs/RUST.md). PLUSONE_P2P_SAME_RADIO=1 tries anyway.
As learned with Second Screen: an activation is never cut short (the app
may accept as late as its end).
"""
import os
import time

from gi.repository import Gio, GLib

from .. import log
from . import channels

NM = "org.freedesktop.NetworkManager"
NM_PATH = "/org/freedesktop/NetworkManager"
DEVICE = "org.freedesktop.NetworkManager.Device"
P2P = "org.freedesktop.NetworkManager.Device.WifiP2P"
PEER = "org.freedesktop.NetworkManager.WifiP2PPeer"
ACTIVE = "org.freedesktop.NetworkManager.Connection.Active"
TYPE_WIFI, TYPE_WIFI_P2P = 2, 30
DEVICE_ACTIVATED = 100
ACTIVATED, DEACTIVATED = 2, 4
ACTIVATION = 55        # seconds: wpa_supplicant repeats its request this long
FIND = 20              # seconds to see the tablet among the peers
AFTER_APP = 1.5        # seconds between the app's connect and the PC's
CONNECT = 15           # seconds for the link's path in the group (its DHCP first)
GROUP_OWNER = "192.168.49.1"  # Android's group owner, always
LINK_PORT = 47101      # the app's link (CompanionService.LINK_PORT)


class NoGroup(Exception):
    """No Wi-Fi Direct group with the tablet (why)."""


def _bus():
    return Gio.bus_get_sync(Gio.BusType.SYSTEM, None)


def _call(bus, path, iface, method, args=None, reply=None):
    return bus.call_sync(NM, path, iface, method, args, reply, Gio.DBusCallFlags.NONE, 60_000, None).unpack()


def _get(bus, path, iface, name):
    try:
        return _call(bus, path, "org.freedesktop.DBus.Properties", "Get", GLib.Variant("(ss)", (iface, name)))[0]
    except GLib.Error:
        return None


def _iw_addresses():
    """{phy: its P2P device's address}, as iw tells it (NetworkManager may not)."""
    try:
        out = Gio.Subprocess.new(["iw", "dev"], Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_SILENCE)
        _ok, text, _err = out.communicate_utf8(None, None)
    except GLib.Error:
        return {}
    found, phy, address = {}, None, None
    for line in (text or "").splitlines():
        line = line.strip()
        if line.startswith("phy#"):
            phy, address = "phy" + line[4:], None
        elif line.startswith("addr "):
            address = line[5:].lower()
        elif line == "type P2P-device" and phy and address:
            found[phy] = address
        elif line.startswith(("Interface", "Unnamed")):
            address = None
    return found


def _phy(interface):
    try:
        with open(f"/sys/class/net/{interface}/phy80211/name") as f:
            return f.read().strip()
    except OSError:
        return None


def p2p_device(bus, same_radio=None):
    """NetworkManager's Wi-Fi Direct device to use: one whose radio is on no
    network (a second adapter); (its path, its address), or None."""
    same_radio = os.environ.get("PLUSONE_P2P_SAME_RADIO") == "1" if same_radio is None else same_radio
    devices = _call(bus, NM_PATH, NM, "GetDevices")[0]
    wifi = {_get(bus, d, DEVICE, "Interface"): d for d in devices if _get(bus, d, DEVICE, "DeviceType") == TYPE_WIFI}
    for device in devices:
        if _get(bus, device, DEVICE, "DeviceType") != TYPE_WIFI_P2P:
            continue
        # "p2p-dev-wlan0": the P2P device of wlan0's radio.
        parent = wifi.get((_get(bus, device, DEVICE, "Interface") or "").removeprefix("p2p-dev-"))
        busy = parent is not None and _get(bus, parent, DEVICE, "State") == DEVICE_ACTIVATED
        if busy and not same_radio:
            continue
        interface = (_get(bus, device, DEVICE, "Interface") or "").removeprefix("p2p-dev-")
        address = (_get(bus, device, P2P, "HwAddress") or "").lower() or _iw_addresses().get(_phy(interface), "")
        return device, address
    return None


class Group:
    """A Wi-Fi Direct group with the paired tablet whose id starts with
    app_id, and the link's path in it. Blocking: on a thread of its own."""

    def __init__(self, app_id, link_connect):
        """link_connect(address): plus-one-link's path there ("p2p")."""
        self.app_id, self.link_connect = app_id, link_connect
        self.bus = None
        self.tablet = None   # the feature's channel: the group lasts while it does
        self.active = None   # NetworkManager's active connection

    def up(self):
        """Until the session is on the group's path; NoGroup if not."""
        t0 = time.monotonic()
        self.bus = _bus()
        found = p2p_device(self.bus)
        if found is None:
            raise NoGroup("no Wi-Fi Direct radio of its own (a second adapter)")
        device, address = found
        self.tablet = channels.connect(self.app_id, "p2p")
        if self.tablet is None:
            raise NoGroup("the app does not serve Wi-Fi Direct over the link")
        _call(self.bus, device, P2P, "StartFind", GLib.Variant("(a{sv})", ({"timeout": GLib.Variant("i", 120)},)))
        self.tablet.send({"type": "p2p", "address": address})
        answer = self.tablet.receive(timeout=FIND + 10)
        if answer.get("type") != "answer":
            raise NoGroup(f"the app said {answer.get('reason') or answer.get('type')} ({answer.get('seen', '')})")
        peer = self._peer(device, answer.get("name", ""))
        user = GLib.get_user_name()
        connection = {
            "connection": {"type": GLib.Variant("s", "wifi-p2p"), "id": GLib.Variant("s", "Plus One"),
                           "autoconnect": GLib.Variant("b", False),
                           "permissions": GLib.Variant("as", [f"user:{user}:"])},
            "wifi-p2p": {"peer": GLib.Variant("s", _get(self.bus, peer, PEER, "HwAddress") or "")},
            "ipv4": {"method": GLib.Variant("s", "auto"), "never-default": GLib.Variant("b", True)},
            "ipv6": {"method": GLib.Variant("s", "ignore")},
        }
        options = {"persist": GLib.Variant("s", "volatile"), "bind-activation": GLib.Variant("s", "dbus-client")}
        # The app first (Android then has the PC allowed), the PC's negotiation
        # after: both at once, the PC's request met a "not yet" (status 1)
        # and the app's provision discovery found the PC off its channel.
        self.tablet.send({"type": "p2p-go"})
        time.sleep(AFTER_APP)
        reply = _call(self.bus, NM_PATH, NM, "AddAndActivateConnection2",
                      GLib.Variant("(a{sa{sv}}ooa{sv})", (connection, device, peer, options)))
        self.active = reply[1]
        self._wait()
        group = self.tablet.receive(timeout=ACTIVATION)
        owner = group.get("address") if group.get("owner") else None
        log(f"p2p: the group is up in {time.monotonic() - t0:.1f} s; the app's address there {owner or '?'}")
        # Up, and the PC's address in it a moment later (the app's DHCP).
        until = time.monotonic() + CONNECT
        while True:
            try:
                self.link_connect(f"{owner or GROUP_OWNER}:{LINK_PORT}")
                return
            except Exception as e:  # noqa: BLE001 - plusone_link.LinkError, OSError: again until CONNECT
                if time.monotonic() > until:
                    raise NoGroup(f"the link not in the group: {e}") from None
                time.sleep(1)

    def _peer(self, device, name):
        """The tablet among NetworkManager's peers, by its name there."""
        until = time.monotonic() + FIND
        seen = []
        while time.monotonic() < until:
            seen = []
            for peer in _get(self.bus, device, P2P, "Peers") or []:
                peer_name = _get(self.bus, peer, PEER, "Name") or ""
                if name and peer_name == name:
                    return peer
                seen.append(peer_name)
            time.sleep(0.25)
        raise NoGroup(f"{name!r} not seen on Wi-Fi Direct (seen: {', '.join(seen) or 'none'})")

    def _wait(self):
        """Until the group is up, with an address: never cut short."""
        until = time.monotonic() + ACTIVATION
        while True:
            state = _get(self.bus, self.active, ACTIVE, "State")
            if state == ACTIVATED:
                return
            if state in (DEACTIVATED, None):
                raise NoGroup("the group did not come up")
            if time.monotonic() > until:
                raise NoGroup("the group took too long")
            time.sleep(0.1)

    def close(self):
        if self.tablet is not None:
            try:
                self.tablet.send({"type": "stop"})
            except OSError:
                pass
            self.tablet.close()
            self.tablet = None
        if self.active is not None and self.bus is not None:
            try:
                _call(self.bus, NM_PATH, NM, "DeactivateConnection", GLib.Variant("(o)", (self.active,)))
            except GLib.Error:
                pass
            self.active = None
