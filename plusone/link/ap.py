"""The PC's own access point for the tablet: a path of the link's own
("ap"), where Wi-Fi Direct is not to be had (an adapter whose driver has no
P2P modes, like rtw88's for a RTL8822BU, but an access point). Once through
the air, and no one else's queue (a phone's hotspot's, a router's).

On an adapter that is not the PC's main connection's (that one is never
touched: a call may be on it), NetworkManager makes a network of its own, a
name of its own for the tablet and a new passphrase each time; over the link the app's "ap" feature
(ApHandler.kt) joins it as a local-only network beside the tablet's own
Wi-Fi (its internet stays there), and says its address in it; plus-one-link
adds a path there, better than the LAN's, and the session moves to it. The
network lasts while the feature's channel does (Network.close(), or Plus
One gone: the connection is bound to it).

A tablet that cannot be on two networks at once (no STA concurrency, as
the Tab S10 FE+) refuses, unless exclusive: then its Wi-Fi moves to the
PC's network for as long as it lasts (it has no internet meanwhile), and
its address cannot come over the link it left: the PC finds it in its own
network's DHCP leases, or among the interface's neighbours.

The network is NetworkManager's "shared" (its DHCP for the tablet), which
also forwards to the PC's own network; SEC-32 wants nothing forwarded (an
nftables rule on it, to come).
"""
import hashlib
import ipaddress
import os
import secrets
import subprocess
import time

from gi.repository import GLib

from .. import log
from . import channels
from .p2p import ACTIVATED, ACTIVATION, ACTIVE, DEACTIVATED, DEVICE, LINK_PORT, NM, NM_PATH, TYPE_WIFI, _bus, _call, _get

WIRELESS = "org.freedesktop.NetworkManager.Device.Wireless"
CAP_AP = 0x40  # NM_WIFI_DEVICE_CAP_AP
JOIN = 90      # seconds for the app to join (Android asks the user the first time)


class NoNetwork(Exception):
    """No network of the PC's own with the tablet (why)."""


def ap_device(bus):
    """A Wi-Fi device that can be an access point and is not the PC's main
    connection's (its path, its interface), or None."""
    primary = _get(bus, NM_PATH, NM, "PrimaryConnection")
    main = set(_get(bus, primary, ACTIVE, "Devices") or []) if primary and primary != "/" else set()
    for device in _call(bus, NM_PATH, NM, "GetDevices")[0]:
        if device in main or _get(bus, device, DEVICE, "DeviceType") != TYPE_WIFI:
            continue
        if (_get(bus, device, WIRELESS, "WirelessCapabilities") or 0) & CAP_AP:
            return device, _get(bus, device, DEVICE, "Interface")
    return None


def network_name(app_id):
    """The PC's network's name for this tablet: the same each time (Android
    asks the user once for a name, not at each passphrase, which is new each
    time), and no one else's (this PC's machine-id and the tablet's id,
    hashed: neither readable from it)."""
    try:
        with open("/etc/machine-id") as f:
            machine = f.read().strip()
    except OSError:
        machine = GLib.get_host_name()
    return "PlusOne-" + hashlib.sha256(f"{machine}/{app_id}".encode()).hexdigest()[:6]


class Network:
    """The PC's access point with the paired tablet whose id starts with
    app_id on it, and the link's path there. Blocking: on a thread of its own."""

    def __init__(self, app_id, link_connect, exclusive=None):
        """link_connect(address): plus-one-link's path there ("ap");
        exclusive: the tablet may leave its own Wi-Fi for it
        (PLUSONE_AP_EXCLUSIVE=1)."""
        self.app_id, self.link_connect = app_id, link_connect
        self.exclusive = os.environ.get("PLUSONE_AP_EXCLUSIVE") == "1" if exclusive is None else exclusive
        self.interface = None
        self.bus = None
        self.tablet = None
        self.active = None

    def up(self):
        t0 = time.monotonic()
        self.bus = _bus()
        found = ap_device(self.bus)
        if found is None:
            raise NoNetwork("no adapter for an access point (besides the main connection's)")
        device, interface = found
        self.interface = interface
        self.tablet = channels.connect(self.app_id, "ap")
        if self.tablet is None:
            raise NoNetwork("the app does not join the PC's network over the link")
        ssid = network_name(self.app_id)
        passphrase = secrets.token_urlsafe(18)
        # 5 GHz, on a channel of another band than the main connection's (not
        # its air: a call may be on it), where the adapter may start a network.
        # 5 GHz, the channel the adapter chooses: one set (36, 157, apart from
        # the main connection's) the RTL8822BU (rtw88) said was up and nothing
        # saw it; chosen, it took 149 and the tablet joined in a second. (On
        # the main connection's channel they share the air: no transfer at
        # full speed there while a call goes.)
        for band, channel in (("a", 0), ("bg", 0)):
            if self._start(device, ssid, passphrase, band, channel):
                break
        else:
            raise NoNetwork(f"{interface} did not start a network")
        log(f"ap: {ssid} on {interface} ({band}, channel {channel or 'any'}) in {time.monotonic() - t0:.1f} s")
        self.tablet.send({"type": "ap", "ssid": ssid, "passphrase": passphrase, "exclusive": self.exclusive})
        address = self._tablet_address()
        log(f"ap: the tablet at {address}, in {time.monotonic() - t0:.1f} s")
        self.link_connect(f"{address}:{LINK_PORT}")

    def _tablet_address(self):
        """Where the tablet is on the PC's network: as it says (over the
        link), or (exclusive: that link was its Wi-Fi) as the network finds it."""
        own = self._own_network()
        until = time.monotonic() + JOIN + 5
        while time.monotonic() < until:
            if self.tablet.readable(0.5):
                said = self.tablet.receive()
                if said.get("type") == "ap-up" and said.get("address"):
                    return said["address"]
                if said.get("type") == "refused":
                    raise NoNetwork(f"the app said {said.get('reason')}")
            if self.exclusive and own is not None:
                for address in self._neighbours():
                    if ipaddress.ip_address(address) in own and address != str(own.network_address + 1):
                        return address
        raise NoNetwork("the tablet did not join the PC's network")

    def _own_network(self):
        out = subprocess.run(["ip", "-4", "-o", "addr", "show", "dev", self.interface],
                             capture_output=True, text=True, timeout=5).stdout.split()
        found = next((word for word in out if "/" in word and word[0].isdigit()), None)
        return ipaddress.ip_interface(found).network if found else None

    def _neighbours(self):
        """Addresses on the network's interface: its DHCP leases, its neighbours."""
        found = []
        try:
            with open(f"/var/lib/NetworkManager/dnsmasq-{self.interface}.leases") as f:
                found += [line.split()[2] for line in f if len(line.split()) > 2]
        except OSError:
            pass
        out = subprocess.run(["ip", "-4", "neigh", "show", "dev", self.interface],
                             capture_output=True, text=True, timeout=5).stdout
        found += [line.split()[0] for line in out.splitlines() if line and "FAILED" not in line]
        return found

    def _start(self, device, ssid, passphrase, band, channel=0):
        user = GLib.get_user_name()
        connection = {
            "connection": {"type": GLib.Variant("s", "802-11-wireless"), "id": GLib.Variant("s", "Plus One"),
                           "autoconnect": GLib.Variant("b", False),
                           "permissions": GLib.Variant("as", [f"user:{user}:"])},
            "802-11-wireless": {"ssid": GLib.Variant("ay", ssid.encode()), "mode": GLib.Variant("s", "ap"),
                                "band": GLib.Variant("s", band), "channel": GLib.Variant("u", channel)},
            "802-11-wireless-security": {"key-mgmt": GLib.Variant("s", "wpa-psk"), "psk": GLib.Variant("s", passphrase),
                                         "proto": GLib.Variant("as", ["rsn"]),
                                         "pairwise": GLib.Variant("as", ["ccmp"]),
                                         "group": GLib.Variant("as", ["ccmp"])},
            "ipv4": {"method": GLib.Variant("s", "shared")},
            "ipv6": {"method": GLib.Variant("s", "ignore")},
        }
        options = {"persist": GLib.Variant("s", "volatile"), "bind-activation": GLib.Variant("s", "dbus-client")}
        try:
            reply = _call(self.bus, NM_PATH, NM, "AddAndActivateConnection2",
                          GLib.Variant("(a{sa{sv}}ooa{sv})", (connection, device, "/", options)))
        except GLib.Error as e:
            log(f"ap: not on band {band}, channel {channel}:", e.message)
            return False
        self.active = reply[1]
        until = time.monotonic() + ACTIVATION
        while time.monotonic() < until:
            state = _get(self.bus, self.active, ACTIVE, "State")
            if state == ACTIVATED:
                return True
            if state in (DEACTIVATED, None):
                break
            time.sleep(0.1)
        log(f"ap: not up on band {band}, channel {channel}")
        self.active = None
        return False

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
