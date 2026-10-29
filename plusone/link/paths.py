"""What each tablet is connected by now, from plus-one-link's events
(docs/LINK.md): the cable, the Wi-Fi..., or nothing for a moment while
its session waits for another path. Told to the window and to GNOME's
Quick Settings (the app's Title and Detail).
"""
import threading
import time

from gi.repository import GLib

from .. import log

try:
    import plusone_link
except ImportError:  # a Plus One without the link
    plusone_link = None

# The link's path kinds, as the user calls them.
NAMES = {"usb": "cable", "lan": "Wi-Fi", "p2p": "Wi-Fi Direct", "wifi-direct": "Wi-Fi Direct", "ap": "Wi-Fi Direct",
         "bt": "Bluetooth", "bluetooth": "Bluetooth", "relay": "the internet"}
RECONNECTING = "reconnecting…"
AGAIN = 15.0  # seconds before looking for plus-one-link again, while it is not there
REWATCH = 1.0  # seconds from one watch to the next at least, while it answers: one hung up at once is no busy loop


class Paths:
    """on_change(): on the main loop, whenever a tablet's path changed.
    path(device): the user's name for it, RECONNECTING while the session
    waits for one, or None (no session over the link: the link of before,
    or none at all)."""

    def __init__(self, on_change, on_link=None):
        self.on_change = on_change
        self.on_link = on_link or (lambda link: None)  # plus-one-link there (again): told our settings
        self._paths = {}  # device id -> kind, or None (waiting)
        self._heard = set()  # the devices an event told of since the watch was made
        self._lock = threading.Lock()  # the events' thread and the watch's: one change at a time
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
        """Under _lock."""
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
        if not device or kind not in ("up", "moved", "detached", "gone"):
            return
        with self._lock:
            self._heard.add(device)
            if kind in ("up", "moved"):
                self._set(device, event.get("path"))
            elif kind == "detached":
                self._set(device, None)
            else:
                self._set(device, None, gone=True)

    def _found(self, devices):
        """plus-one-link's devices(), asked once the watch was made: the
        sessions there are now, but for a device an event told of since
        (the events tell it as it is from then on, whichever came first)."""
        connected = {device["id"]: device.get("path") for device in devices if device.get("connected")}
        with self._lock:
            for device in set(self._paths) | set(connected):
                if device not in self._heard:
                    self._set(device, connected.get(device), gone=device not in connected)

    def _answers(self):
        """Whether plus-one-link is there, and answers (devices())."""
        try:
            with plusone_link.Link() as link:
                link.devices()
            return True
        except (OSError, ValueError, plusone_link.LinkError):
            return False

    def _run(self):
        # Each watch until it ends, no waking meanwhile. plus-one-link still
        # answering (it shuts a watch it cannot tell an event): watched again
        # at once, REWATCH after the last at the soonest, the paths shown as
        # they were meanwhile. Gone, or any failure: no path, and looked for
        # again each AGAIN (it may start later, socket-activated or by hand).
        while not self._stopped.is_set():
            since = time.monotonic()
            link = None
            answers = False
            try:
                link = plusone_link.Link()
                self.on_link(link)
                with self._lock:
                    self._heard = set()
                self._watcher = link.watch(self._event)
                self._found(link.devices())
                link.close()
                # Until plus-one-link hangs up or goes (its events' connection ends).
                self._watcher.thread.join()
                answers = not self._stopped.is_set() and self._answers()
            except (OSError, ValueError, plusone_link.LinkError):
                pass
            except Exception as e:  # noqa: BLE001 - looked for again all the same
                log("link paths:", repr(e))
            finally:
                if link is not None:
                    link.close()
                if self._watcher is not None:
                    self._watcher.close()
                    self._watcher = None
                if not answers:
                    with self._lock:
                        for device in list(self._paths):
                            self._set(device, None, gone=True)
            self._stopped.wait(max(0.0, since + REWATCH - time.monotonic()) if answers else AGAIN)

    def stop(self):
        self._stopped.set()
        if self._watcher is not None:
            self._watcher.close()
