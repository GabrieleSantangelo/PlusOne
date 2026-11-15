"""BleNearby: the tablets heard holding pairing open, as a discovery source."""
import threading

from plusone.link import BleNearby, Discovery
from plusone.link import blenearby

KITCHEN = "aaaa" + "0" * 12


class Clock:
    def __init__(self):
        self.now = 100.0

    def __call__(self):
        return self.now


class FakeLink:
    """plusone_link.Link's pairing_scan, as a context manager made by calling."""

    def __init__(self, tablets=(), fail=None):
        self.tablets = list(tablets)
        self.fail = fail
        self.scans = []
        self.gate = None  # an Event a scan waits for

    def __call__(self):
        return self

    def __enter__(self):
        return self

    def __exit__(self, *_):
        return False

    def pairing_scan(self, wait):
        self.scans.append(wait)
        if self.gate:
            self.gate.wait(5)
        if self.fail:
            raise self.fail
        return self.tablets


def scanned(source):
    """refresh_pairing(), and its thread over."""
    source.refresh_pairing()
    for thread in threading.enumerate():
        if thread.name == "ble-nearby":
            thread.join(5)


def test_a_tablet_heard_is_listed_with_where_it_is():
    changes = []
    link = FakeLink([{"id": KITCHEN, "name": "Kitchen tablet", "address": "192.168.1.9", "port": 47101}])
    source = BleNearby(on_change=lambda: changes.append(1), link=link, clock=Clock())
    assert source.apps() == [] and source.addresses(KITCHEN) == []
    scanned(source)
    assert source.apps() == [(KITCHEN, "192.168.1.9", 47101, "Kitchen tablet")]
    assert source.addresses(KITCHEN) == [("192.168.1.9", 47101)]
    assert link.scans == [blenearby.SCAN] and changes == [1]
    # Heard again as it was: nothing to tell.
    scanned(source)
    assert changes == [1]


def test_a_tablet_not_heard_for_a_while_is_no_longer_there():
    clock = Clock()
    link = FakeLink([{"id": KITCHEN, "name": "K", "address": "10.0.0.2", "port": 1}])
    changes = []
    source = BleNearby(on_change=lambda: changes.append(1), link=link, clock=clock)
    scanned(source)
    clock.now += blenearby.FRESH - 1
    assert source.apps() != []
    clock.now += 2
    assert source.apps() == [] and source.addresses(KITCHEN) == []
    link.tablets = []
    scanned(source)  # the one that went is dropped, and said
    assert changes == [1, 1]


def test_a_tablet_with_no_name_yet_gets_one_to_show():
    source = BleNearby(link=FakeLink([{"id": KITCHEN, "name": "", "address": "10.0.0.2", "port": 1}]), clock=Clock())
    scanned(source)
    assert source.apps() == [(KITCHEN, "10.0.0.2", 1, "Tablet aaaa")]


def test_nonsense_from_the_link_is_left_out():
    source = BleNearby(link=FakeLink([{"id": "short", "name": "x", "address": "1.2.3.4", "port": 1},
                                      {"id": KITCHEN, "address": "1.2.3.4", "port": "x"},
                                      {"name": "no id"}, "not a dict"]), clock=Clock())
    scanned(source)
    assert source.apps() == []


def test_one_scan_at_a_time():
    link = FakeLink([])
    link.gate = threading.Event()
    source = BleNearby(link=link, clock=Clock())
    source.refresh_pairing()
    source.refresh_pairing()  # the pairing page asks every second and a half
    source.refresh_pairing()
    link.gate.set()
    scanned(source)
    assert len(link.scans) == 1
    scanned(source)  # and another once it is over
    assert len(link.scans) == 2


def test_no_link_or_a_link_that_fails_is_no_tablet_and_no_trouble(monkeypatch):
    for fail in (OSError("no such file"), ValueError("not json"), RuntimeError("LinkError")):
        source = BleNearby(link=FakeLink(fail=fail), clock=Clock())
        scanned(source)
        scanned(source)
        assert source.apps() == []
    monkeypatch.setattr(blenearby, "plusone_link", None)
    BleNearby().refresh_pairing()  # no client library: nothing to ask, no thread


def test_it_is_no_way_to_tell_a_tablet_is_not_there():
    source = BleNearby(link=FakeLink(), clock=Clock())
    assert not source.present() and source.names() == [] and source.app_version(KITCHEN) is None
    source.start()
    source.stop()
    source.refresh()  # the paired tablets are not asked for like this: no scan


def test_discovery_asks_for_pairing_only_those_that_can_hear_it():
    class Plain:
        calls = []

        def start(self): pass
        def refresh(self): self.calls.append("refresh")
        def apps(self): return []
        def addresses(self, _app_id): return []

    link = FakeLink([{"id": KITCHEN, "name": "K", "address": "10.0.0.2", "port": 1}])
    ble = BleNearby(link=link, clock=Clock())
    plain = Plain()
    found = Discovery([plain, ble])
    found.refresh()
    assert link.scans == [], "an ordinary refresh does not scan"
    found.refresh_pairing()
    for thread in threading.enumerate():
        if thread.name == "ble-nearby":
            thread.join(5)
    assert link.scans == [blenearby.SCAN]
    assert [app[0] for app in found.apps()] == [KITCHEN]
    assert found.addresses(KITCHEN) == [("10.0.0.2", 1)]
