"""plusone_link: Plus One's link from Python (docs/LINK.md).

plus-one-link keeps the sessions with the paired tablets; this asks it for
channels through its socket. A channel is a message socket: one send, one
message; close it and the channel closes; an empty read is its end (closed
by the tablet, or the session over).

    from plusone_link import Link, REALTIME
    link = Link()
    tablet = link.connect("192.168.1.20:47000")
    video = link.open(tablet, "display", "video", REALTIME)
    video.send(packet)
    data = video.recv(timeout=0.1)       # None: nothing in time
    server = link.serve("speakers", lambda channel: ...)
    watcher = link.watch(lambda event: ...)

No dependencies: the standard library's socket (file descriptors come
with the answers, SCM_RIGHTS).
"""
import array
import json
import os
import select
import socket
import threading
import traceback

CONTROL, INPUT, REALTIME = "control", "input", "realtime"
REQUEST = 64 << 10
# Seconds plus-one-link has to answer a request, besides what the request
# waits for itself (a connect's timeout, a browse's wait); and what open()
# and stream() may wait for: a session made (the tablet reached anew where
# it was and wherever mDNS finds it, one at a time) and the tablet's answer.
ANSWER_WAIT = 5.0
SESSION_WAIT = 15.0
# A channel's largest message: control's (the system caps a message
# socket's), and input's or realtime's (a datagram on any network, QUIC's
# too: plus-one-link's session.rs MAX_DATAGRAM).
MAX_CONTROL = 200 << 10
MAX_DATAGRAM = 1380


class LinkError(Exception):
    """plus-one-link said no (its reason), or is not there."""


def socket_path():
    """plus-one-link's socket: PLUSONE_LINK_SOCKET, or its usual place."""
    if os.environ.get("PLUSONE_LINK_SOCKET"):
        return os.environ["PLUSONE_LINK_SOCKET"]
    runtime = os.environ.get("XDG_RUNTIME_DIR") or f"/run/user/{os.getuid()}"
    return os.path.join(runtime, "plus-one", "link")


def _connect(path):
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    try:
        sock.connect(path)
    except OSError as e:
        sock.close()
        raise LinkError(f"plus-one-link is not there ({path}: {e.strerror})") from None
    return sock


def _receive(sock):
    """(message, descriptor or None); LinkError if plus-one-link went, or
    said what is not a message (ValueError: not JSON)."""
    data, fds, _flags, _address = socket.recv_fds(sock, REQUEST, 1)
    try:
        if not data:
            raise LinkError("plus-one-link went away")
        message = json.loads(data)
        if not isinstance(message, dict):
            raise LinkError(f"plus-one-link said {data[:80]!r}")
    except BaseException:
        for fd in fds:
            os.close(fd)
        raise
    return message, (fds[0] if fds else None)


class Channel:
    """One channel's end. device: the tablet's id; feature, name, kind
    (CONTROL, INPUT or REALTIME), id: what it is."""

    def __init__(self, fd, device, feature, name, kind, id):
        self.sock = socket.socket(fileno=fd)
        self.device, self.feature, self.name, self.kind, self.id = device, feature, name, kind, id
        self._largest = MAX_CONTROL if kind == CONTROL else MAX_DATAGRAM

    def send(self, data):
        """One message (waits for room)."""
        self.sock.send(data)

    def recv(self, timeout=None):
        """One message; b"" at the channel's end; None if nothing came
        within timeout (seconds; None: as long as it takes)."""
        if timeout is not None and not select.select([self.sock], [], [], timeout)[0]:
            return None
        return self.sock.recv(self._largest)

    def fileno(self):
        return self.sock.fileno()

    def close(self):
        self.sock.close()

    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        self.close()

    def __repr__(self):
        return f"<Channel {self.feature}/{self.name} {self.kind} #{self.id}>"


class _Events:
    """A connection of its own that carries events (serve, watch), read on
    a thread of its own."""

    def __init__(self, path, request, on_event, name):
        self.sock = _connect(path)
        try:
            self.sock.settimeout(ANSWER_WAIT)
            self.sock.send(json.dumps(request).encode())
            reply, fd = _receive(self.sock)
            if fd is not None:
                os.close(fd)
            if not reply.get("ok"):
                raise LinkError(reply.get("error", "refused"))
            self.sock.settimeout(None)  # the events: whenever they come
            self._on_event = on_event
            self.thread = threading.Thread(target=self._run, name=name, daemon=True)
            self.thread.start()
        except BaseException:
            self.sock.close()
            raise

    def _run(self):
        while True:
            try:
                event, fd = _receive(self.sock)
            except (OSError, ValueError, LinkError):
                return
            try:
                self._on_event(event, fd)
            except Exception:  # noqa: BLE001 - said, and the next events still come
                traceback.print_exc()

    def close(self):
        """No more events (what was served, no longer)."""
        try:
            self.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.sock.close()


class Link:
    """plus-one-link's socket: path, or its usual place."""

    def __init__(self, path=None):
        self.path = path or socket_path()
        self._sock = _connect(self.path)
        self._lock = threading.Lock()

    def stream(self, kind, sock):
        """A path on a stream socket made here (a Bluetooth one BlueZ gave:
        kind "bt"); the device whose session goes on it."""
        reply, fd = self._request({"op": "stream", "kind": kind}, given=sock.fileno(), wait=SESSION_WAIT)
        if fd is not None:
            os.close(fd)
        return reply["device"]

    def _request(self, message, given=None, wait=0.0):
        """plus-one-link's answer to message (with given, a descriptor of
        ours, if any) within ANSWER_WAIT and wait: (reply, descriptor or
        None); LinkError if it said no, or did not answer in time."""
        data = json.dumps(message).encode()
        with self._lock:
            if self._sock.fileno() < 0:
                raise LinkError("no longer connected to plus-one-link")
            self._sock.settimeout(ANSWER_WAIT + wait)
            try:
                if given is None:
                    self._sock.send(data)
                else:
                    self._sock.sendmsg([data], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", [given]))])
                reply, fd = _receive(self._sock)
            except TimeoutError:
                # Its answer may come yet, and be taken for the next one's.
                self._sock.close()
                raise LinkError(f"plus-one-link did not answer in time ({message['op']})") from None
        if not reply.get("ok"):
            if fd is not None:
                os.close(fd)
            raise LinkError(reply.get("error", "refused"))
        return reply, fd

    def devices(self):
        """The paired tablets: [{"id", "name", "connected", "features",
        "rtt_ms", "dropped", "paths"}]; each path {"id", "kind", "transport",
        "state" ("active", "suspect", "failed"), "rtt_ms", "silent_ms",
        "budget_bps", "budget_from"}."""
        return self._request({"op": "devices"})[0]["devices"]

    def connect(self, address=None, device=None, timeout=5.0):
        """A session with the tablet at address ("IP:PORT"), or with device
        (its id) wherever it is: its id. open() makes one when needed."""
        request = {"op": "connect", "timeout": timeout}
        request.update({"address": address} if address else {"device": device})
        return self._request(request, wait=timeout)[0]["device"]

    def disconnect(self, device):
        """The session with the tablet over (its channels with it)."""
        self._request({"op": "close", "device": device})

    def browse(self, wait=1.5):
        """The tablets announcing the link nearby (mDNS): [{"id" (its start),
        "name", "version", "address"}]."""
        return self._request({"op": "browse", "wait": wait}, wait=wait)[0]["tablets"]

    def use_usb(self, on=True):
        """Whether an Android tablet plugged in is asked to be Plus One's
        accessory (its file transfer off meanwhile); one reached by cable
        before always is."""
        self._request({"op": "usb", "use": bool(on)})

    def queued(self, device):
        """Bytes on their way to the tablet not yet sent: how far behind the
        network is."""
        return self._request({"op": "queued", "device": device})[0]["queued"]

    def open(self, device, feature, name, kind=CONTROL):
        """A channel with the tablet (its id, or a device from devices()); the
        session made first if there is none."""
        device = device["id"] if isinstance(device, dict) else device
        reply, fd = self._request({"op": "open", "device": device, "feature": feature, "channel": name,
                                   "class": kind}, wait=SESSION_WAIT)
        if fd is None:
            raise LinkError("no channel came with the answer")
        return Channel(fd, device, feature, name, kind, reply.get("id"))

    def serve(self, feature, on_channel, version=1):
        """The tablets' channels for feature, from now on: on_channel(Channel)
        for each, on a thread of its own. close() the result to stop."""
        def incoming(event, fd):
            if event.get("event") != "incoming" or fd is None:
                return
            channel = Channel(fd, event.get("device"), event.get("feature"), event.get("channel"),
                              event.get("class"), event.get("id"))
            threading.Thread(target=on_channel, args=(channel,), name=f"link-{feature}", daemon=True).start()
        return _Events(self.path, {"op": "serve", "feature": feature, "version": version}, incoming,
                       f"link-serve-{feature}")

    def watch(self, on_event):
        """Tablets coming and going, from now on: on_event({"event": "up",
        "moved", "detached" or "gone", "device", ...}); {"event":
        "budget", "device", "kbps"}: what the path's congestion controller
        allows as it moves (None: a path with none of its own); and
        {"event": "path", "device", ...}: a path's state changed (as
        devices()' "paths"). close() the result to stop."""
        return _Events(self.path, {"op": "watch"}, lambda event, _fd: on_event(event), "link-watch")

    def close(self):
        self._sock.close()

    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        self.close()
