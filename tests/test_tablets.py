"""The paired tablets, the one in use, and where to reach its app."""
import pytest

from plusone import link
from plusone.engine import Settings
from plusone.link import Tablets

KITCHEN, OFFICE = "aaaa" + "0" * 60, "bbbb" + "0" * 60


@pytest.fixture
def paired(tmp_path):
    store = link.Paired(str(tmp_path))
    store.add(OFFICE, "Office")
    store.add(KITCHEN, "Kitchen")
    return store


class Nearby:
    def __init__(self, found=()):
        self.found = list(found)  # (address, port, app id)

    def addresses(self, app_id):
        return [(a, p) for a, p, i in self.found if i == app_id]

    def refresh(self):
        pass


def test_the_tablet_in_use(paired):
    saved = []
    settings = Settings()
    tablets = Tablets(settings, lambda: saved.append(settings.tablet), paired=paired)
    assert [t.name for t in tablets.all()] == ["Kitchen", "Office"]
    assert tablets.active().fingerprint == KITCHEN and saved == [KITCHEN]  # none chosen yet: the first
    tablets.use(OFFICE)
    assert tablets.active().name == "Office" and tablets.active().app_id == OFFICE[:16]
    tablets.forget(OFFICE)
    assert tablets.active().name == "Kitchen" and OFFICE not in paired.all()
    tablets.forget(KITCHEN)
    assert tablets.active() is None and settings.tablet is None


def test_pairings_of_0_5_are_kept(tmp_path):
    (tmp_path / "paired.json").write_text('{"%s": "Tab", "%s": 3}' % (KITCHEN, OFFICE))
    store = link.Paired(str(tmp_path))
    assert store.all() == {KITCHEN: "Tab"}
    store.learn(KITCHEN, app_at="10.0.0.7:47236")
    store.add(KITCHEN, "Tab S10")  # paired again: what was learned stays
    assert store.entries() == {KITCHEN: {"name": "Tab S10", "app_at": "10.0.0.7:47236"}}


def test_where_the_app_was_last_is_tried_when_it_is_not_announced(paired, monkeypatch):
    monkeypatch.setattr(link.tablets, "on_link", lambda host: True)
    nearby = Nearby([("10.0.0.7", 47236, KITCHEN[:16])])
    tablets = Tablets(Settings(), paired=paired, discovery=nearby)
    kitchen = tablets.active()
    assert tablets.targets(kitchen) == [("10.0.0.7", 47236)]
    assert paired.entries()[KITCHEN]["app_at"] == "10.0.0.7:47236"  # remembered
    nearby.found = []  # asleep: mDNS does not answer
    refreshed = []
    nearby.refresh = lambda: refreshed.append(1)
    assert tablets.addresses(kitchen) == []  # not said to be there
    assert tablets.targets(kitchen) == [("10.0.0.7", 47236)] and refreshed == [1]  # but tried there
    office = next(t for t in tablets.all() if t.fingerprint == OFFICE)
    assert tablets.targets(office) == []  # never seen: nowhere to try
    paired.learn(OFFICE, app_at="nonsense:99999")
    assert tablets.targets(office) == []



def test_where_the_app_was_on_another_network_is_forgotten(paired, monkeypatch):
    # 15/11: 10.151.253.132, from the network of before, tried for 5 s at home.
    monkeypatch.setattr(link.tablets, "on_link", lambda host: host.startswith("192.168.1."))
    tablets = Tablets(Settings(), paired=paired, discovery=Nearby())
    kitchen = tablets.active()
    paired.learn(KITCHEN, app_at="10.151.253.132:47236")
    assert tablets.targets(kitchen) == []
    assert not paired.entries()[KITCHEN].get("app_at")  # forgotten, not tried again
    paired.learn(KITCHEN, app_at="192.168.1.112:47236")
    assert tablets.targets(kitchen) == [("192.168.1.112", 47236)]


def test_on_link_reads_this_pcs_networks(monkeypatch):
    import json
    import subprocess
    up = [{"ifname": "lo", "addr_info": [{"local": "127.0.0.1", "prefixlen": 8}]},
          {"ifname": "wlan0", "addr_info": [{"local": "192.168.1.155", "prefixlen": 24},
                                            {"local": "2a01:e11::5", "prefixlen": 64}]}]
    monkeypatch.setattr(subprocess, "run", lambda *a, **k: subprocess.CompletedProcess(a, 0, json.dumps(up), ""))
    on_link = link.tablets.on_link
    assert on_link("192.168.1.112") and on_link("2a01:e11::9")
    assert not on_link("10.151.253.132") and not on_link("127.0.0.1")
    assert on_link("not an address")  # no telling: tried


def test_on_link_without_ip_tries(monkeypatch):
    import subprocess

    def missing(*a, **k):
        raise FileNotFoundError("ip")
    monkeypatch.setattr(subprocess, "run", missing)
    assert link.tablets.on_link("10.0.0.7")


def test_only_a_network_address_is_remembered(paired):
    """Another transport's target (USB, say) is not where to try next time."""
    found = Nearby()
    found.addresses = lambda app_id: [("usb", "R5CX", 47236), ("10.0.0.8", 47236)]
    tablets = Tablets(Settings(), paired=paired, discovery=found)
    kitchen = tablets.active()
    assert tablets.targets(kitchen) == [("usb", "R5CX", 47236), ("10.0.0.8", 47236)]
    assert paired.entries()[KITCHEN]["app_at"] == "10.0.0.8:47236"
