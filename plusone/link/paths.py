"""What each tablet is connected by now, from plus-one-link's events
(docs/LINK.md): the cable, the Wi-Fi..., or nothing for a moment while
its session waits for another path. Told to the window and to GNOME's
Quick Settings (the app's Title and Detail). Each path's state (active,
suspect, failed) is kept too, and logged when it is in trouble: nothing
shows it yet.
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
    or none at all). states(device): its paths as plus-one-link last told
    them."""

    def __init__(self, on_change, on_link=None):
        self.on_change = on_change
        self.on_link = on_link or (lambda link: None)  # plus-one-link there (again): told our settings
        self._paths = {}  # device id -> kind, or None (waiting)
        self._states = {}  # device id -> {path id: plus-one-link's word on it}
        self._heard = set()  # the devices an event told of since the watch was made, and (device, path id)s
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

    def states(self, device):
        """The paths to device (its id) as plus-one-link last told them:
        [{"id", "kind", "transport", "state", "rtt_ms", "silent_ms",
        "budget_bps", "budget_from"}], what else it says too."""
        return list(self._states.get(device, {}).values())

    def _set(self, device, kind, gone=False):
        """Under _lock."""
        before = dict(self._paths)
        paths = dict(before)
        if gone:
            paths.pop(device, None)
            self._states = {id_: known for id_, known in self._states.items() if id_ != device}
        else:
            paths[device] = kind
        self._paths = paths  # a new one: readers never see it change under them
        if paths != before:
            GLib.idle_add(self._changed)

    def _state(self, device, path):
        """A path's state, as plus-one-link tells it ("path" events,
        devices' "paths"): kept (a failed one forgotten), logged as it gets
        in trouble or out of it. Under _lock."""
        if not isinstance(path, dict) or "id" not in path:
            return
        known = dict(self._states.get(device, {}))
        before = known.get(path["id"], {}).get("state")
        state = path.get("state")
        if state == "failed":
            known.pop(path["id"], None)
        else:
            known[path["id"]] = path
        self._states = {**self._states, device: known}
        name = NAMES.get(path.get("kind"), path.get("kind"))
        if state in ("suspect", "failed") and state != before:  # not again at each watch
            silent = path.get("silent_ms")
            log(f"link: the {name} path ({path['id']}) is {state}"
                + (f", nothing heard for {silent / 1000:.1f} s" if isinstance(silent, (int, float)) else ""))
        elif before == "suspect" and state == "active":
            log(f"link: the {name} path ({path['id']}) is active again")

    def _changed(self):
        self.on_change()
        return False

    def _event(self, event):
        kind = event.get("event")
        device = event.get("device")
        if not device or kind not in ("up", "moved", "path", "detached", "gone"):
            return
        with self._lock:
            self._heard.add((device, event.get("id")) if kind == "path" else device)
            if kind in ("up", "moved"):
                self._set(device, event.get("path"))
            elif kind == "path":
                self._state(device, event)
            elif kind == "detached":
                self._set(device, None)
            else:
                self._set(device, None, gone=True)

    def _found(self, devices):
        """plus-one-link's devices(), asked once the watch was made: the
        sessions there are now and their paths, but for a device or a path
        an event told of since (the events tell it as it is from then on,
        whichever came first). A path known before and neither listed nor
        told of since is no more (its end told while nothing watched)."""
        connected = {device["id"]: device for device in devices if device.get("connected")}
        with self._lock:
            for device in set(self._paths) | set(connected):
                if device not in self._heard:
                    self._set(device, connected.get(device, {}).get("path"), gone=device not in connected)
            for device in set(self._paths) & set(connected):
                listed = {path["id"]: path for path in connected[device].get("paths") or []
                          if isinstance(path, dict) and "id" in path}
                known = self._states.get(device, {})
                self._states = {**self._states, device: {id_: path for id_, path in known.items()
                                                         if id_ in listed or (device, id_) in self._heard}}
                for id_, path in listed.items():
                    if (device, id_) not in self._heard:
                        self._state(device, path)

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
                        self._states = {}  # and what any path was
            self._stopped.wait(max(0.0, since + REWATCH - time.monotonic()) if answers else AGAIN)

    def stop(self):
        self._stopped.set()
        if self._watcher is not None:
            self._watcher.close()
