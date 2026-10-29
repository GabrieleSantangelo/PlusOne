"""A feature's session over the link (plus-one-link, docs/LINK.md), with
the link of before's surface (link.Link: request, hold, shutdown, close),
so the feature's code is the same: its control channel carries the same
JSON messages. More channels for its packets come with it (packets()):
what Datagrams carries over the link of before, over the link's own.

A payload (a PDF to sign, the app) follows its header on the control
channel in CHUNK pieces, as on the link of before it follows on the
socket; so does a message longer than a channel message may be (a signed
document's marks): "long" says how long, the pieces follow.
"""
import collections
import json
import select
import socket
import threading
import time

from .. import log
from .link import IO_TIMEOUT, Refused
from .wire import MAX_HEADER

try:
    import plusone_link
except ImportError:  # a Plus One without the link: the link of before only
    plusone_link = None

ANSWER_WAIT = 10.0  # seconds for the app's answer to a request
CHUNK = 64 << 10   # bytes of a payload or a long message per channel message
SENDING_OVER = 1.0  # seconds what the channels send a tablet is measured over
WIRE = 92  # bytes a datagram takes on the network besides its body: the seal's 30, QUIC's 34, UDP and IPv4's 28

# What the channels of this process sent each tablet lately: device ->
# (when, bytes) each body; read by sending_kbps().
_sent = {}
_sent_lock = threading.Lock()


def sending_kbps(device):
    """What this process's channels send device now (kbit/s, as the network
    carries it): the screen's share of the link's budget leaves it (the
    speakers' PCM, 1.7 Mbit/s, on a hotspot of 3)."""
    with _sent_lock:
        sent = _sent.get(device)
        if not sent:
            return 0
        _forget(sent, time.monotonic())
        return sum(size for _at, size in sent) * 8 / 1000 / SENDING_OVER


def _forget(sent, now):
    while sent and now - sent[0][0] > SENDING_OVER:
        sent.popleft()


class Budget:
    """What the path to device's congestion controller allows (kbit/s):
    plus-one-link's word on it, asked once (kbps()), then as it tells it
    moved (watch(): past 10 %, a cut at once; architecture §3.11). None on
    a path with no controller of its own (TCP, a cable)."""

    def __init__(self, connection, device):
        self.connection, self.device = connection, device
        self._told = None  # the last told: kbit/s, 0 for none
        self._watch = None

    def kbps(self):
        if self._told is not None:
            return self._told or None
        try:
            return next((d for d in self.connection.devices() if d["id"] == self.device), {}).get("rate_kbps") or None
        except (OSError, ValueError, plusone_link.LinkError):
            return None

    def watch(self, on_budget):
        """on_budget(kbit/s or None) as it moves, on the events' thread, until close()."""
        def event(event):
            if event.get("event") == "budget" and event.get("device") == self.device:
                self._told = int(event.get("kbps") or 0)
                on_budget(self._told or None)
        try:
            self._watch = self.connection.watch(event)
        except (OSError, ValueError, plusone_link.LinkError) as e:
            log("the link's budget not watched:", e)

    def close(self):
        if self._watch is not None:
            self._watch.close()


def connect(app_id, feature):
    """The feature's session with the paired tablet whose id starts with
    app_id, over the link: a ChannelLink, or None (plus-one-link not running,
    the tablet not reached there, or its app not serving the feature): the
    link of before then."""
    if plusone_link is None:
        return None
    try:
        connection = plusone_link.Link()
    except plusone_link.LinkError:
        return None
    try:
        found = next((d for d in connection.devices() if d["id"].startswith(app_id)), None)
        if found is None:
            raise plusone_link.LinkError("not a paired tablet")
        control = connection.open(found["id"], feature, "control", plusone_link.CONTROL)
    except plusone_link.LinkError as e:
        log(f"{feature}: not over the link ({e})")
        connection.close()
        return None
    return ChannelLink(connection, found["id"], feature, control, found.get("name") or "")


def _session(app_id):
    """plus-one-link's entry for the paired tablet whose id starts with
    app_id, if it has a session with it now; else None."""
    if plusone_link is None:
        return None
    try:
        with plusone_link.Link() as connection:
            return next((d for d in connection.devices() if d["id"].startswith(app_id) and d.get("connected")), None)
    except plusone_link.LinkError:
        return None


def connected(app_id):
    """Whether plus-one-link has a session with that tablet now (by cable,
    say, with no address on the network)."""
    return _session(app_id) is not None


def app_version(app_id):
    """The versionCode that tablet's app serves "update" at over the link
    (its own), while there is a session with it; else None."""
    session = _session(app_id)
    version = (session or {}).get("features", {}).get("update")
    return version if type(version) is int and version > 1 else None


class ChannelLink:
    """See the module. peer: None (no address: the packets go on channels);
    name: the tablet's."""
    peer = None

    def __init__(self, connection, device, feature, control, name=""):
        self.connection, self.device, self.feature, self.control = connection, device, feature, control
        self.name = name
        self._channels = []
        self._sending = threading.Lock()  # a payload's pieces together, whatever else is sent meanwhile
        self._timeout = IO_TIMEOUT

    def send(self, header, payload=None):
        """header, then payload's bytes if any (its "size" says how many),
        within settimeout()'s seconds all told."""
        if payload is not None:
            header = dict(header, size=len(payload))
        data = json.dumps(header).encode()
        with self._sending:
            deadline = None if self._timeout is None else time.monotonic() + self._timeout
            if len(data) > CHUNK:
                self._put(json.dumps({"type": "long", "size": len(data)}).encode(), deadline)
                self._pieces(data, deadline)
            else:
                self._put(data, deadline)
            if payload:
                self._pieces(payload, deadline)

    def _pieces(self, data, deadline):
        view = memoryview(data)
        for start in range(0, len(view), CHUNK):
            self._put(view[start:start + CHUNK], deadline)

    def _put(self, data, deadline):
        """One message, once the link has room for it, before deadline."""
        if deadline is None:
            self.control.send(data)
            return
        left = deadline - time.monotonic()
        if left <= 0:
            raise TimeoutError(f"{self.feature}: the app took nothing more in time")
        self.control.sock.settimeout(left)
        try:
            self.control.send(data)
        finally:
            self.control.sock.settimeout(None)

    def _next(self, timeout):
        data = self.control.recv(timeout=timeout)
        if data is None:
            raise TimeoutError(f"{self.feature}: no answer")
        if data == b"":
            raise EOFError(f"{self.feature}: the app hung up")
        return data

    def receive(self, timeout=ANSWER_WAIT, limit=MAX_HEADER):
        """The next message; a long one put together (at most limit bytes)."""
        message = json.loads(self._next(timeout))
        if not (isinstance(message, dict) and message.get("type") == "long"):
            return message
        size = message.get("size")
        if type(size) is not int or not 0 < size <= limit:
            raise ValueError(f"{self.feature}: a long message of {size!r} bytes")
        data = bytearray()
        while len(data) < size:
            data += self._next(timeout)
        if len(data) != size:
            raise ValueError(f"{self.feature}: a long message longer than it said")
        return json.loads(data)

    def readable(self, timeout):
        """Whether a message came within timeout (seconds)."""
        return bool(select.select([self.control.sock], [], [], timeout)[0])

    def settimeout(self, timeout):
        """As the link of before's: how long a send() may take, all told
        (seconds; None: as long as the link's backlog lasts). A tablet that
        stops reading fails it then (TimeoutError); one that is gone, the
        link's own liveness ends sooner."""
        self._timeout = timeout

    def request(self, header, payload=None, limit=MAX_HEADER, timeout=ANSWER_WAIT):
        """header (and its payload) to the app, and its answer (at most
        limit bytes, within timeout once sent); Refused if the app said no."""
        self.send(header, payload)
        reply = self.receive(timeout, limit)
        if reply.get("type") == "refused":
            raise Refused(str(reply.get("reason", "")))
        return reply

    def hold(self, until, check=None, tick=0.5, every=None):
        """While the session lasts, until until(): check() each tick (it may
        raise to end it); the app hanging up raises EOFError. No pings: the
        link's own say when the tablet is gone (its channels end)."""
        while not until():
            if check:
                check()
            data = self.control.recv(timeout=tick)
            if data == b"":
                raise EOFError(f"{self.feature}: the app hung up")

    def packets(self, name, kind=None):
        """A channel of the feature's for its packets (realtime unless kind
        says otherwise), opened before the request that needs it."""
        channel = self.connection.open(self.device, self.feature, name, kind or plusone_link.REALTIME)
        self._channels.append(channel)
        return ChannelPackets(self.connection, self.device, channel)

    def shutdown(self):
        """From another thread: whatever waits on it wakes, and ends."""
        try:
            self.control.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass

    def close(self):
        for channel in [self.control, *self._channels]:
            channel.close()
        self.connection.close()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


class ChannelPackets:
    """One channel as Datagrams is used: send(body), receive(timeout) (None:
    nothing in time, or its end), queued() (plus-one-link's backlog to the
    tablet), close(); payload: the most a body may be."""

    def __init__(self, connection, device, channel):
        self.connection, self.device, self.channel = connection, device, channel
        self.payload = plusone_link.MAX_DATAGRAM
        self.errors = set()
        self._ended = False
        self.budget = Budget(connection, device)

    def send(self, body):
        try:
            self.channel.send(body)
        except OSError as e:
            if e.errno not in self.errors:
                self.errors.add(e.errno)
                log(f"{self.channel.feature}: cannot send:", e.strerror)
            return
        now = time.monotonic()
        with _sent_lock:
            sent = _sent.setdefault(self.device, collections.deque())
            sent.append((now, len(body) + WIRE))
            _forget(sent, now)

    def receive(self, timeout):
        if self._ended:
            select.select([], [], [], timeout)
            return None
        try:
            data = self.channel.recv(timeout=timeout)
        except (OSError, ValueError):
            data = b""
        if data == b"":
            self._ended = True
            return None
        return data

    def queued(self):
        try:
            return self.connection.queued(self.device)
        except (OSError, ValueError, plusone_link.LinkError):
            return 0

    def close(self):
        self.budget.close()
        self.channel.close()
