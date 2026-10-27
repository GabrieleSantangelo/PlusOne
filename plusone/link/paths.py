"""What each tablet is connected by now, from plus-one-link's events
(docs/LINK.md): the cable, the Wi-Fi..., or nothing for a moment while
its session waits for another path. Told to the window and to GNOME's
Quick Settings (the app's Title and Detail).
"""
import threading

from gi.repository import GLib

try:
    import plusone_link
except ImportError:  # a Plus One without the link
    plusone_link = None

# The link's path kinds, as the user calls them.
NAMES = {"usb": "cable", "lan": "Wi-Fi", "wifi-direct": "Wi-Fi Direct", "ap": "Wi-Fi Direct",
         "bluetooth": "Bluetooth", "relay": "the internet"}
RECONNECTING = "reconnecting…"
AGAIN = 15.0  # seconds before looking for plus-one-link again, while it is not there


class Paths:
    """on_change(): on the main loop, whenever a tablet's path changed.
    path(device): the user's name for it, RECONNECTING while the session
    waits for one, or None (no session over the link: the link of before,
    or none at all)."""

    def __init__(self, on_change, on_link=None):
        self.on_change = on_change
        self.on_link = on_link or (lambda link: None)  # plus-one-link there (again): told our settings
        self._paths = {}  # device id -> kind, or None (waiting)
        self._watcher = None
        self._stopped = threading.Event()
        if plusone_link is not None:
            threading.Thread(target=self._run, name="plusone-paths", daemon=True).start()

    def path(self, device):
        """device: the tablet's id, or the start of it (an app id)."""
        if not device:
            return None
        for id_, kind in list(self._paths.items()):  # changed by the events' thread
            if id_.startswith(device) or device.startswith(id_):
                return NAMES.get(kind, kind) if kind else RECONNECTING
        return None

    def _set(self, device, kind, gone=False):
        before = dict(self._paths)
        paths = dict(before)
        if gone:
            paths.pop(device, None)
        else:
            paths[device] = kind
        self._paths = paths  # a new one: readers never see it change under them
        if paths != before:
            GLib.idle_add(self._changed)

    def _changed(self):
        self.on_change()
        return False

    def _event(self, event):
        kind = event.get("event")
        device = event.get("device")
        if not device:
            return
        if kind in ("up", "moved"):
            self._set(device, event.get("path"))
        elif kind == "detached":
            self._set(device, None)
        elif kind == "gone":
            self._set(device, None, gone=True)

    def _run(self):
        # While plus-one-link is not there: looked for again now and then
        # (it may start later, socket-activated or by hand).
        while not self._stopped.is_set():
            try:
                link = plusone_link.Link()
                self.on_link(link)
                self._watcher = link.watch(self._event)
                for device in link.devices():
                    if device.get("connected"):
                        self._set(device["id"], device.get("path"))
                link.close()
                # Until plus-one-link goes (its events' connection ends): no waking meanwhile.
                self._watcher.thread.join()
                self._watcher = None
                for device in list(self._paths):
                    self._set(device, None, gone=True)
            except (OSError, ValueError, plusone_link.LinkError):
                pass
            self._stopped.wait(AGAIN)

    def stop(self):
        self._stopped.set()
        if self._watcher is not None:
            self._watcher.close()
