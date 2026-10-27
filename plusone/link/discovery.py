"""Discovery: every way of finding the tablets' apps, as one. Each source
watches one way (mDNS, nearby.py, today; a USB cable or Bluetooth would be
others) and Discovery answers for all of them: a tablet is there if any
source sees it, its app is reached wherever any of them says.

A source has start(), stop(), present(), names(), apps() [(app id,
address, port, name)], app_version(app id), addresses(app id) (targets,
transport.py), refresh(), and calls the on_change it was made with when
what it sees may have changed.
"""


class Discovery:
    def __init__(self, sources):
        self.sources = list(sources)

    def start(self):
        for source in self.sources:
            source.start()

    def stop(self):
        for source in self.sources:
            source.stop()

    def present(self):
        """A tablet is around, or there is no telling."""
        return any(source.present() for source in self.sources)

    def names(self):
        return sorted({name for source in self.sources for name in source.names()})

    def apps(self):
        """The app on each tablet around, [(app id, address, port, name)] by
        name: one per app, as the first source to see it says."""
        seen = {}
        for source in self.sources:
            for app in source.apps():
                seen.setdefault(app[0] or f"{app[1]}:{app[2]}", app)
        return sorted(seen.values(), key=lambda app: (app[3].lower(), app[0]))

    def app_version(self, app_id):
        """The versionCode the app with this id says (0: not said; None:
        no source sees it)."""
        found = [v for v in (source.app_version(app_id) for source in self.sources) if v is not None]
        return max(found) if found else None

    def addresses(self, app_id):
        """Every target the app with this id is announced at, by any source:
        only the real one proves it (TLS)."""
        return list(dict.fromkeys(target for source in self.sources for target in source.addresses(app_id)))

    def refresh(self):
        """Asked where the apps are now (the user wants a tablet)."""
        for source in self.sources:
            source.refresh()
