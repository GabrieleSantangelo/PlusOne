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
import json
import os
import select
import socket
import threading

CONTROL, INPUT, REALTIME = "control", "input", "realtime"
REQUEST = 64 << 10
# A channel's largest message: control's (the system caps a message
# socket's), and input's or realtime's (a datagram on any network).
MAX_CONTROL = 200 << 10
MAX_DATAGRAM = 1400


class LinkError(Exception):
    """plus-one-link said no (its reason), or is not there."""


def socket_path():
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
    """(message, descriptor or None); LinkError if plus-one-link went."""
    data, fds, _flags, _address = socket.recv_fds(sock, REQUEST, 1)
    if not data:
        for fd in fds:
            os.close(fd)
        raise LinkError("plus-one-link went away")
    return json.loads(data), (fds[0] if fds else None)


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
        self.sock.send(json.dumps(request).encode())
        reply, _fd = _receive(self.sock)
        if not reply.get("ok"):
            self.sock.close()
            raise LinkError(reply.get("error", "refused"))
        self._on_event = on_event
        self.thread = threading.Thread(target=self._run, name=name, daemon=True)
        self.thread.start()

    def _run(self):
        while True:
            try:
                event, fd = _receive(self.sock)
            except (OSError, ValueError, LinkError):
                return
            self._on_event(event, fd)

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

    def _request(self, message):
        with self._lock:
            self._sock.send(json.dumps(message).encode())
            reply, fd = _receive(self._sock)
        if not reply.get("ok"):
            if fd is not None:
                os.close(fd)
            raise LinkError(reply.get("error", "refused"))
        return reply, fd

    def devices(self):
        """The paired tablets: [{"id", "name", "connected", "features",
        "rtt_ms", "dropped"}]."""
        return self._request({"op": "devices"})[0]["devices"]

    def connect(self, address=None, device=None, timeout=5.0):
        """A session with the tablet at address ("IP:PORT"), or with device
        (its id) wherever it is: its id. open() makes one when needed."""
        request = {"op": "connect", "timeout": timeout}
        request.update({"address": address} if address else {"device": device})
        return self._request(request)[0]["device"]

    def disconnect(self, device):
        """The session with the tablet over (its channels with it)."""
        self._request({"op": "close", "device": device})

    def browse(self, wait=1.5):
        """The tablets announcing the link nearby (mDNS): [{"id" (its start),
        "name", "version", "address"}]."""
        return self._request({"op": "browse", "wait": wait})[0]["tablets"]

    def queued(self, device):
        """Bytes on their way to the tablet not yet sent: how far behind the
        network is."""
        return self._request({"op": "queued", "device": device})[0]["queued"]

    def open(self, device, feature, name, kind=CONTROL):
        """A channel with the tablet (its id, or a device from devices()); the
        session made first if there is none."""
        device = device["id"] if isinstance(device, dict) else device
        reply, fd = self._request({"op": "open", "device": device, "feature": feature, "channel": name,
                                   "class": kind})
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
        """Tablets coming and going, from now on: on_event({"event": "up" or
        "gone", "device", ...}). close() the result to stop."""
        return _Events(self.path, {"op": "watch"}, lambda event, _fd: on_event(event), "link-watch")

    def close(self):
        self._sock.close()

    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        self.close()
