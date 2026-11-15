"""Tablets on the network, from their Quick Share and KDE Connect announcements."""
import base64

from gi.repository import GLib

from plusone.link.nearby import BROWSER, COMPANION, KDE_CONNECT, QUICK_SHARE, RESOLVER, RETRIES, Nearby, tablet_name


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


def test_the_app_stays_through_one_lost_answer():
    # A resolve that times out once (Wi-Fi power saving) asks again; the app
    # (and the tablet's speakers with it) goes only once every retry failed.
    nearby, changes = watching()
    app = (3, 0, "Plus One on Tab", COMPANION, "local")
    announce(nearby, "ItemNew", app)
    nearby._resolved(nearby.bus, reply(app, [b"name=Tab", b"id=00ff"]), app)
    assert app in nearby.companions and nearby.present()
    said = list(changes)
    for _ in RETRIES:
        nearby._resolved(nearby.bus, GLib.Error("Timeout reached"), app)
        assert app in nearby.companions and changes == said
    nearby._resolved(nearby.bus, GLib.Error("Timeout reached"), app)
    assert app not in nearby.companions and not nearby.present()


def test_without_avahi_there_is_no_telling():
    nearby, changes = watching()
    nearby._vanished(nearby.bus, "org.freedesktop.Avahi")
    assert nearby.present() and changes == [True]


def test_other_services_are_ignored():
    nearby, _changes = watching()
    announce(nearby, "ItemNew", (3, 0, "printer", "_ipp._tcp", "local"))
    assert nearby.bus.calls == [] and not nearby.services


def test_the_app_says_its_version():
    nearby, _changes = watching()
    app = (3, 0, "Plus One on Tab", COMPANION, "local")
    announce(nearby, "ItemNew", app)
    assert nearby.app_version("00ff") is None  # not resolved: not there
    nearby._resolved(nearby.bus, reply(app, [b"name=Tab", b"id=00ff"]), app)
    assert nearby.app_version("00ff") == 0  # an app before 0.8 says none
    said = []
    nearby.on_change = lambda: said.append(nearby.app_version("00ff"))
    nearby._resolved(nearby.bus, reply(app, [b"name=Tab", b"id=00ff", b"v=800"]), app)
    assert nearby.app_version("00ff") == 800 and said == [800]  # updated: told


def test_the_app_opening_pairing_is_seen_at_once():
    # The tablet's app changes its TXT in place (pair=1) under the same name:
    # no ItemNew says so, the resolver Avahi keeps running does (Found).
    nearby = Nearby(paired=lambda: {}, ours=lambda: "ff" * 32)
    nearby.bus, nearby.known = Bus(), True
    app = (3, 0, "Plus One 02e8", COMPANION, "local")
    announce(nearby, "ItemNew", app)
    assert [call[3] for call in nearby.bus.calls] == ["ResolveService", "ServiceResolverNew"]
    nearby._resolved(nearby.bus, reply(app, [b"t=0123456789abcdef"]), app)
    assert not nearby.apps()  # another PC's tablet, or this one before pairing opens
    nearby._made(nearby.bus, GLib.Variant("(o)", ("/Client1/ServiceResolver1",)), app)
    found = reply(app, [b"pair=1", b"name=Tab", b"id=6a2665bcebe1984c", b"t=0123456789abcdef"])
    nearby._resolver_signal(nearby.bus, ":1.5", "/Client1/ServiceResolver1", RESOLVER, "Found", found)
    assert nearby.apps() == [("6a2665bcebe1984c", "192.168.1.5", 1716, "Tab")]
    # Others' resolvers say nothing about it.
    nearby._resolver_signal(nearby.bus, ":1.5", "/Client1/ServiceResolver9", RESOLVER, "Found",
                            reply(app, [b"t=0123456789abcdef"]))
    assert nearby.apps()


def test_a_followed_app_that_leaves_frees_its_resolver():
    nearby, _changes = watching()
    app = (3, 0, "Plus One on Tab", COMPANION, "local")
    announce(nearby, "ItemNew", app)
    nearby._made(nearby.bus, GLib.Variant("(o)", ("/Client1/ServiceResolver1",)), app)
    announce(nearby, "ItemRemove", app)
    assert nearby.bus.calls[-1][1:4] == ("/Client1/ServiceResolver1", "org.freedesktop.Avahi.ServiceResolver", "Free")
    assert not nearby._followed and not nearby._following


def test_a_resolver_made_after_its_app_left_is_freed():
    nearby, _changes = watching()
    app = (3, 0, "Plus One on Tab", COMPANION, "local")
    announce(nearby, "ItemNew", app)
    announce(nearby, "ItemRemove", app)
    nearby._made(nearby.bus, GLib.Variant("(o)", ("/Client1/ServiceResolver1",)), app)
    assert nearby.bus.calls[-1][3] == "Free" and not nearby._following


def test_a_failed_resolver_is_followed_again_at_the_recheck():
    nearby, _changes = watching()
    app = (3, 0, "Plus One on Tab", COMPANION, "local")
    announce(nearby, "ItemNew", app)
    nearby._made(nearby.bus, GLib.Variant("(o)", ("/Client1/ServiceResolver1",)), app)
    nearby._resolver_signal(nearby.bus, ":1.5", "/Client1/ServiceResolver1", RESOLVER, "Failure",
                            GLib.Variant("(s)", ("Timeout reached",)))
    assert nearby.bus.calls[-1][3] == "Free" and app not in nearby._followed
    nearby._recheck()
    assert nearby.bus.calls[-1][3] == "ServiceResolverNew"


def test_a_found_before_the_resolvers_path_is_known_counts():
    nearby = Nearby(paired=lambda: {}, ours=lambda: "ff" * 32)
    nearby.bus, nearby.known = Bus(), True
    app = (3, 0, "Plus One 02e8", COMPANION, "local")
    announce(nearby, "ItemNew", app)
    found = reply(app, [b"pair=1", b"name=Tab", b"id=6a2665bcebe1984c"])
    nearby._resolver_signal(nearby.bus, ":1.5", "/Client1/ServiceResolver1", RESOLVER, "Found", found)
    assert not nearby.apps()
    nearby._made(nearby.bus, GLib.Variant("(o)", ("/Client1/ServiceResolver1",)), app)
    assert nearby.apps() == [("6a2665bcebe1984c", "192.168.1.5", 1716, "Tab")] and not nearby._early


def test_resolver_signals_are_not_kept_when_none_is_being_made():
    nearby, _changes = watching()
    nearby._resolver_signal(nearby.bus, ":1.5", "/Client1/ServiceResolver7", RESOLVER, "Found",
                            reply((3, 0, "x", COMPANION, "local"), []))
    assert not nearby._early
