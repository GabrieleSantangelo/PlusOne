"""Discovery: every source of where the tablets' apps are, as one."""
from plusone.link import Discovery

KITCHEN, OFFICE = "aaaa" + "0" * 12, "bbbb" + "0" * 12


class Source:
    """A way of finding the apps, as Discovery sees it (nearby.py's API)."""

    def __init__(self, apps=(), present=False, versions=None):
        self.found = list(apps)  # (app id, target, name)
        self.here = present
        self.versions = versions or {}
        self.calls = []

    def start(self):
        self.calls.append("start")

    def stop(self):
        self.calls.append("stop")

    def refresh(self):
        self.calls.append("refresh")

    def present(self):
        return self.here

    def names(self):
        return sorted(name for _id, _target, name in self.found)

    def apps(self):
        return [(app_id, *target[-2:], name) for app_id, target, name in self.found]

    def app_version(self, app_id):
        return self.versions.get(app_id) if any(a == app_id for a, _t, _n in self.found) else None

    def addresses(self, app_id):
        return [target for a, target, _n in self.found if a == app_id]


def test_sources_answer_as_one():
    wifi = Source([(KITCHEN, ("10.0.0.2", 47236), "Kitchen")], present=True, versions={KITCHEN: 801})
    usb = Source([(KITCHEN, ("usb", "R5CX", 47236), "Kitchen"), (OFFICE, ("usb", "R5CY", 47236), "Office")],
                 versions={KITCHEN: 900})
    found = Discovery([wifi, usb])
    found.start()
    found.refresh()
    found.stop()
    assert wifi.calls == usb.calls == ["start", "refresh", "stop"]
    assert found.present()
    assert found.names() == ["Kitchen", "Office"]
    # Every target any source has, in the sources' order, each once.
    assert found.addresses(KITCHEN) == [("10.0.0.2", 47236), ("usb", "R5CX", 47236)]
    assert found.addresses("cccc" + "0" * 12) == []
    # One entry per app, as the first source to see it says.
    assert [(app[0], app[1]) for app in found.apps()] == [(KITCHEN, "10.0.0.2"), (OFFICE, "R5CY")]
    assert found.app_version(KITCHEN) == 900 and found.app_version("cccc" + "0" * 12) is None


def test_no_tablet_when_no_source_sees_one():
    assert not Discovery([Source(), Source()]).present()
    assert Discovery([Source(), Source(present=True)]).present()  # mDNS: no telling, say
