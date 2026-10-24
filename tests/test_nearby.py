"""Tablets on the network, from their Quick Share and KDE Connect announcements."""
import base64

from gi.repository import GLib

from plusone.nearby import BROWSER, KDE_CONNECT, QUICK_SHARE, Nearby, tablet_name


def quick_share(first_byte, name=None):
    info = bytes([first_byte]) + bytes(range(16))  # salt and encrypted key
    if name is not None:
        info += bytes([len(name)]) + name.encode()
    return [b"IPv4=192.168.1.5", b"n=" + base64.urlsafe_b64encode(info).rstrip(b"="), b"f=2462"]


def test_quick_share_says_the_device_type():
    # version 1 | hidden | type << 1, as Android sends it in contacts-only mode.
    assert tablet_name(QUICK_SHARE, quick_share(0x34)) == ""
    assert tablet_name(QUICK_SHARE, quick_share(0x32)) is None  # a phone
    assert tablet_name(QUICK_SHARE, quick_share(0x24, "Tab S10")) == "Tab S10"  # visible to everyone
    assert tablet_name(QUICK_SHARE, [b"n=!!"]) is None
    assert tablet_name(QUICK_SHARE, []) is None


def test_kde_connect_says_the_device_type():
    assert tablet_name(KDE_CONNECT, [b"type=tablet", b"name=Galaxy Tab S10 FE+"]) == "Galaxy Tab S10 FE+"
    assert tablet_name(KDE_CONNECT, [b"type=phone", b"name=Phone"]) is None


class Bus:
    def __init__(self):
        self.calls = []

    def call(self, *args):
        self.calls.append(args)

    def call_finish(self, result):
        if isinstance(result, GLib.Error):
            raise result
        return result


def reply(key, txt):
    return GLib.Variant("(iissssisqaayu)", key + ("android.local", 0, "192.168.1.5", 1716, txt, 0))


def watching():
    changes = []
    nearby = Nearby(on_change=lambda: changes.append(nearby.present()))
    nearby.bus, nearby.known = Bus(), True
    return nearby, changes


def announce(nearby, signal, key):
    nearby._signal(nearby.bus, ":1.5", "/Client1/ServiceBrowser1", BROWSER, signal,
                   GLib.Variant("(iisssu)", key + (0,)))


def test_a_tablet_comes_and_goes():
    nearby, changes = watching()
    assert not nearby.present()
    tablet = (3, 0, "89bd", KDE_CONNECT, "local")
    phone = (3, 0, "I1NY", QUICK_SHARE, "local")
    for key in (tablet, phone):
        announce(nearby, "ItemNew", key)
    assert [call[3] for call in nearby.bus.calls] == ["ResolveService"] * 2
    nearby._resolved(nearby.bus, reply(phone, quick_share(0x32)[1:2]), phone)
    assert not nearby.present() and changes == []
    nearby._resolved(nearby.bus, reply(tablet, [b"type=tablet", b"name=Tab"]), tablet)
    assert nearby.present() and nearby.names() == ["Tab"] and changes == [True]
    announce(nearby, "ItemRemove", tablet)
    assert not nearby.present() and changes == [True, False]


def test_a_tablet_that_stops_answering_is_gone():
    nearby, changes = watching()
    tablet = (3, 1, "I0FL", QUICK_SHARE, "local")
    announce(nearby, "ItemNew", tablet)
    nearby._resolved(nearby.bus, reply(tablet, quick_share(0x34)[1:2]), tablet)
    assert nearby.present()
    nearby._recheck()
    nearby._resolved(nearby.bus, GLib.Error("Timeout reached"), tablet)
    assert not nearby.present() and changes == [True, False]


def test_without_avahi_there_is_no_telling():
    nearby, changes = watching()
    nearby._vanished(nearby.bus, "org.freedesktop.Avahi")
    assert nearby.present() and changes == [True]


def test_other_services_are_ignored():
    nearby, _changes = watching()
    announce(nearby, "ItemNew", (3, 0, "printer", "_ipp._tcp", "local"))
    assert nearby.bus.calls == [] and not nearby.services
