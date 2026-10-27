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


def test_where_the_app_was_last_is_tried_when_it_is_not_announced(paired):
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


def test_only_a_network_address_is_remembered(paired):
    """Another transport's target (USB, say) is not where to try next time."""
    found = Nearby()
    found.addresses = lambda app_id: [("usb", "R5CX", 47236), ("10.0.0.8", 47236)]
    tablets = Tablets(Settings(), paired=paired, discovery=found)
    kitchen = tablets.active()
    assert tablets.targets(kitchen) == [("usb", "R5CX", 47236), ("10.0.0.8", 47236)]
    assert paired.entries()[KITCHEN]["app_at"] == "10.0.0.8:47236"
