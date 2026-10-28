"""Discovery over mDNS: the tablets on the local network, from what their
apps announce, Plus One's own (where to connect; which tablet, told by a
tag only its paired PCs recognise: tags.py) and others' (that a tablet is
there at all).

Android's Quick Share and KDE Connect both say what kind of device they run
on, and Avahi listens to the network anyway, so watching costs next to
nothing. A tablet that leaves without saying goodbye can stay listed for 75
minutes (the TTL mDNS recommends for that record), so each one is resolved
again every minute: that needs its SRV record, which lives 2 minutes, and
fails once the tablet stops answering (RFC 6762, section 10).
"""
import base64
import binascii

from gi.repository import Gio, GLib

from .. import log, once
from . import tags
from .trust import Paired, fingerprint, identity

AVAHI = "org.freedesktop.Avahi"
SERVER = AVAHI + ".Server"
BROWSER = AVAHI + ".ServiceBrowser"
QUICK_SHARE = "_FC9F5ED42C8A._tcp"
KDE_CONNECT = "_kdeconnect._udp"
COMPANION = "_plusone._tcp"  # Plus One's own app on the tablet (link.py)
SERVICES = (QUICK_SHARE, KDE_CONNECT, COMPANION)
UNSPEC = -1  # any interface, any protocol
INET = 0     # Avahi's IPv4: the app is reached at an IPv4 address
TABLET = 2  # Quick Share device type: 1 phone, 2 tablet, 3 laptop
RECHECK = 60  # seconds
# A resolve of the app's that failed (the tablet asleep answers late) is
# tried again after these, not only at the next RECHECK: once it took a
# minute to find a tablet that was there.
RETRIES = (2, 5, 15)  # seconds
RESOLVE_TIMEOUT = 10000  # ms; Avahi gives up after 5 s


def tablet_name(service, txt):
    """For a service's TXT record (list of byte strings): the device's name
    if it is a tablet ("" if it does not say), None if it is not one."""
    fields = dict(entry.partition(b"=")[::2] for entry in txt)
    if service == KDE_CONNECT:
        if fields.get(b"type") != b"tablet":
            return None
        return fields.get(b"name", b"").decode(errors="replace")
    # Quick Share: n is the endpoint info, base64url. Its first byte holds the
    # device type in bits 1-3; when visible to everyone, a length-prefixed
    # name follows 16 bytes of salt and key.
    n = fields.get(b"n", b"")
    try:
        info = base64.urlsafe_b64decode(n + b"=" * (-len(n) % 4))
    except (binascii.Error, ValueError):
        return None
    if not info or (info[0] >> 1) & 7 != TABLET:
        return None
    if len(info) > 18 and len(info) >= 18 + info[17]:
        return info[18:18 + info[17]].decode(errors="replace")
    return ""


_ours = []


def _our_fingerprint():
    if not _ours:
        _ours.append(fingerprint(identity()[2]))
    return _ours[0]


class Nearby:
    """Watches Avahi for tablets; on_change() fires when present() or
    names() may have changed."""

    def __init__(self, on_change=None, paired=None, ours=None):
        """paired() -> {fingerprint: name} (this PC's tablets: paired.json);
        ours(): this PC's own fingerprint. The app's announcements name only
        those."""
        self.on_change = on_change or (lambda: None)
        self.paired = paired or (lambda: Paired().all())
        self.ours = ours or _our_fingerprint
        self.bus = None
        self.known = False   # Avahi is there to ask
        self.services = set()  # (interface, protocol, name, type, domain) announced
        self.tablets = {}    # those on a tablet -> its name ("" if unknown)
        # The app's services -> (address, port, name, app id); replaced whole
        # on each change, as the signing threads read it.
        self.companions = {}
        self._browsers = []
        self._failed = {}  # an app's service -> resolves failed in a row
        self.versions = {}  # an app's service -> its versionCode ("v"; 0 before 0.8)
        self._subscription = self._watch = self._timer = None

    def present(self):
        """A tablet is on the network, or there is no telling."""
        return bool(self.tablets) or not self.known

    def names(self):
        return sorted({name for name in self.tablets.values() if name})

    def apps(self):
        """The app on each tablet on the network, [(app id, address, port,
        name)] by name: one per tablet, which may show up on two interfaces.
        The id is the start of the app's certificate fingerprint."""
        seen = {}
        for address, port, name, app_id in sorted(self.companions.values()):
            seen.setdefault(app_id or f"{address}:{port}", (app_id, address, port, name))
        return sorted(seen.values(), key=lambda app: (app[3].lower(), app[0]))

    def app_version(self, app_id):
        """The versionCode the app with this id announces (0: not said, an
        app before 0.8; None: not on the network)."""
        found = [self.versions.get(key, 0) for key, v in self.companions.items() if v[3] == app_id]
        return max(found) if found else None

    def addresses(self, app_id):
        """Every (address, port) the app with this id is announced at: someone
        else may announce it too, and only the real one proves it (TLS)."""
        return sorted({(address, port) for address, port, _name, found in self.companions.values()
                       if found == app_id})

    def start(self):
        if self.bus is not None:
            return
        try:
            self.bus = Gio.bus_get_sync(Gio.BusType.SYSTEM, None)
        except GLib.Error as e:
            log("nearby: no system bus:", e.message)
            return
        # All browsers' signals, filtered by service type: ours may send some
        # before we learn their paths.
        self._subscription = self.bus.signal_subscribe(
            None, BROWSER, None, None, None, Gio.DBusSignalFlags.NONE, self._signal)
        self._watch = Gio.bus_watch_name_on_connection(
            self.bus, AVAHI, Gio.BusNameWatcherFlags.NONE, self._appeared, self._vanished)
        self._timer = GLib.timeout_add_seconds(RECHECK, self._recheck)

    def stop(self):
        if self.bus is None:
            return
        for path in self._browsers:
            self.bus.call(AVAHI, path, BROWSER, "Free", None, None,
                          Gio.DBusCallFlags.NONE, -1, None, None)
        self.bus.signal_unsubscribe(self._subscription)
        Gio.bus_unwatch_name(self._watch)
        GLib.source_remove(self._timer)
        self.bus = self._subscription = self._watch = self._timer = None
        self._forget()
        self.known = False

    def _forget(self):
        self._browsers = []
        self.services.clear()
        self.tablets.clear()
        # Replaced, not cleared: other threads iterate it (tablets.targets).
        self.companions = {}

    # -- Avahi -----------------------------------------------------------------
    def _appeared(self, bus, name, owner):
        self.known = True
        for service in SERVICES:
            bus.call(AVAHI, "/", SERVER, "ServiceBrowserNew",
                     GLib.Variant("(iissu)", (UNSPEC, UNSPEC, service, "", 0)),
                     GLib.VariantType("(o)"), Gio.DBusCallFlags.NONE, -1, None, self._browsing)
        self.on_change()

    def _browsing(self, bus, result):
        try:
            self._browsers.append(bus.call_finish(result).unpack()[0])
        except GLib.Error as e:
            log("nearby: cannot browse:", e.message)

    def _vanished(self, bus, name):
        self.known = False
        self._forget()
        self.on_change()

    def _signal(self, bus, sender, path, interface, signal, params):
        if signal not in ("ItemNew", "ItemRemove"):
            return
        interface, protocol, name, service, domain, _flags = params.unpack()
        if service not in SERVICES:
            return
        key = (interface, protocol, name, service, domain)
        if signal == "ItemNew":
            if key not in self.services:
                self.services.add(key)
                self._resolve(key)
        else:
            self.services.discard(key)
            if key in self.companions:
                self.companions = {k: v for k, v in self.companions.items() if k != key}
                log("nearby: the Plus One app left the network")
                self.on_change()
            self._update(key, None)

    def _resolve(self, key):
        aprotocol = INET if key[3] == COMPANION else UNSPEC
        self.bus.call(AVAHI, "/", SERVER, "ResolveService",
                      GLib.Variant("(iisssiu)", key + (aprotocol, 0)),
                      GLib.VariantType("(iissssisqaayu)"), Gio.DBusCallFlags.NONE,
                      RESOLVE_TIMEOUT, None, self._resolved, key)

    def _resolved(self, bus, result, key):
        try:
            reply = bus.call_finish(result).unpack()
            txt = [bytes(entry) for entry in reply[9]]
        except GLib.Error:
            reply = txt = None  # gone, or not answering any more
        if key not in self.services:
            return
        if key[3] == COMPANION:
            failed = 0 if reply is not None else self._failed.get(key, 0) + 1
            self._failed[key] = failed
            if 0 < failed <= len(RETRIES):
                GLib.timeout_add_seconds(RETRIES[failed - 1], once(self._retry, key))
                if key in self.companions:
                    return  # one answer lost is not the app gone (the tablet's speakers with it)
            self._companion(key, reply, txt)
            # The app runs on a tablet: that is a tablet on the network too.
            self._update(key, self.companions[key][2] if key in self.companions else None)
            return
        self._update(key, tablet_name(key[3], txt) if txt is not None else None)

    def _companion(self, key, reply, txt):
        before = self.companions.get(key), self.versions.get(key)
        companions = {k: v for k, v in self.companions.items() if k != key}
        found = self._which(txt) if reply is not None else None
        if found is not None:
            app_id, name, version = found
            companions[key] = (reply[7], reply[8], name or key[2], app_id)
            self.versions[key] = version
        else:
            self.versions.pop(key, None)
        self.companions = companions
        if (self.companions.get(key), self.versions.get(key)) != before:
            log("nearby:", f"the Plus One app on {self.companions[key][2]} at {reply[7]}:{reply[8]}"
                if key in self.companions else "the Plus One app is gone")
            self.on_change()

    def _which(self, txt):
        """Which app an announcement is: (app id, name, version), or None if
        it is no tablet of this PC's (nor one whose pairing is open)."""
        fields = dict(entry.partition(b"=")[::2] for entry in txt)
        app_id = fields.get(b"id", b"").decode(errors="replace").lower()[:64]
        name = fields.get(b"name", b"").decode(errors="replace")[:64]
        version = fields.get(b"v", b"")
        version = int(version) if version.isdigit() and len(version) <= 9 else 0
        if b"t" not in fields:
            return app_id, name, version  # an app from before tags: says who it is
        if fields.get(b"pair") == b"1" and app_id:
            return app_id, name, 0  # its user opened pairing on it: it says who, for a while
        announced = set(fields[b"t"].decode(errors="replace").split(","))
        ours = self.ours()
        for fingerprint, paired_name in self.paired().items():
            if tags.recognised(announced, fingerprint, ours):
                return fingerprint[:16], paired_name, 0  # its version: over the link
        return None

    def _update(self, key, name):
        # Each tablet shows up once per app and IP version: report changes only.
        before = bool(self.tablets), self.names()
        if name is None:
            self.tablets.pop(key, None)
        else:
            self.tablets[key] = name
        after = bool(self.tablets), self.names()
        if after != before:
            log("nearby:", f"tablet on the network ({', '.join(after[1]) or 'unnamed'})"
                if after[0] else "no tablet on the network")
            self.on_change()

    def _retry(self, key):
        if self.bus is not None and key in self.services:
            self._resolve(key)

    def refresh(self):
        """Asked where the apps are now (the user wants a tablet)."""
        if self.bus is not None:
            for key in list(self.services):
                if key[3] == COMPANION:
                    self._resolve(key)

    def _recheck(self):
        for key in list(self.services):
            self._resolve(key)
        return True
