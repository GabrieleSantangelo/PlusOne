"""The paired tablets, and the one in use: the only one that gets the
desktop and the PDFs to sign, and whose camera, microphone and speakers
are the PC's. Where its app is: announced (discovery.py), or
where it was last.
"""
from dataclasses import dataclass

from .trust import MAX_NAME, Paired


@dataclass(frozen=True)
class Tablet:
    fingerprint: str  # of its app's certificate
    name: str

    @property
    def app_id(self):
        """What its app announces itself as (nearby.py)."""
        return self.fingerprint[:16]


class Tablets:
    def __init__(self, settings, save=lambda: None, discovery=None, paired=None):
        """settings.tablet: the fingerprint of the one in use; save() stores
        the settings; discovery: where the apps are, if known."""
        self.settings = settings
        self.save = save
        self.discovery = discovery
        self.paired = paired or Paired()

    def all(self):
        """The paired tablets, by name."""
        return sorted((Tablet(fp, entry["name"][:MAX_NAME])
                       for fp, entry in self.paired.entries().items()),
                      key=lambda t: (t.name.lower(), t.fingerprint))

    def active(self):
        """The tablet in use, or None if none is paired."""
        tablets = self.all()
        chosen = next((t for t in tablets if t.fingerprint == self.settings.tablet), None)
        if chosen is None and tablets:
            # The one in use was forgotten, or never chosen (0.5): the first one.
            chosen = tablets[0]
            self.use(chosen.fingerprint)
        return chosen

    def use(self, fingerprint):
        if self.settings.tablet != fingerprint:
            self.settings.tablet = fingerprint
            self.save()

    def forget(self, fingerprint):
        self.paired.forget(fingerprint)
        if self.settings.tablet == fingerprint:
            self.settings.tablet = None
            self.save()

    def screen_seen(self, screen):
        """The tablet's app said its screen is (w, h): remembered for the
        sizes the Second screen page offers."""
        text = f"{max(screen)}x{min(screen)}"
        if self.settings.app_screen != text:
            self.settings.app_screen = text
            self.save()

    def addresses(self, tablet):
        """Where its app is announced: its targets (transport.py), (address,
        port) on the network; the first of those is remembered."""
        found = self.discovery.addresses(tablet.app_id) if self.discovery else []
        tcp = next((t for t in found if len(t) == 2 and isinstance(t[1], int)), None)
        if tcp:
            at = "%s:%d" % tcp
            if self.paired.entries().get(tablet.fingerprint, {}).get("app_at") != at:
                self.paired.learn(tablet.fingerprint, app_at=at)
        return found

    def targets(self, tablet):
        """Where to reach its app: where it is announced, else where it was
        last (mDNS can miss a tablet asleep for a while; its address seldom
        changes, and the TLS link proves it is the tablet either way)."""
        found = self.addresses(tablet)
        if found:
            return found
        if self.discovery:
            self.discovery.refresh()
        at = self.paired.entries().get(tablet.fingerprint, {}).get("app_at", "")
        host, _colon, port = at.rpartition(":")
        return [(host, int(port))] if host and port.isdigit() and 0 < int(port) < 65536 else []
