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
import json
import select
import socket
import threading

from .. import log
from .link import Refused
from .wire import MAX_HEADER

try:
    import plusone_link
except ImportError:  # a Plus One without the link: the link of before only
    plusone_link = None

ANSWER_WAIT = 10.0  # seconds for the app's answer to a request
CHUNK = 64 << 10   # bytes of a payload or a long message per channel message


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


def connected(app_id):
    """Whether plus-one-link has a session with the paired tablet whose id
    starts with app_id now (by cable, say, with no address on the network)."""
    if plusone_link is None:
        return False
    try:
        with plusone_link.Link() as connection:
            return any(d["id"].startswith(app_id) and d.get("connected") for d in connection.devices())
    except plusone_link.LinkError:
        return False


class ChannelLink:
    """See the module. peer: None (no address: the packets go on channels);
    name: the tablet's."""
    peer = None

    def __init__(self, connection, device, feature, control, name=""):
        self.connection, self.device, self.feature, self.control = connection, device, feature, control
        self.name = name
        self._channels = []
        self._sending = threading.Lock()  # a payload's pieces together, whatever else is sent meanwhile

    def send(self, header, payload=None):
        """header, then payload's bytes if any (its "size" says how many)."""
        if payload is not None:
            header = dict(header, size=len(payload))
        data = json.dumps(header).encode()
        with self._sending:
            if len(data) > CHUNK:
                self.control.send(json.dumps({"type": "long", "size": len(data)}).encode())
                self._pieces(data)
            else:
                self.control.send(data)
            if payload:
                self._pieces(payload)

    def _pieces(self, data):
        view = memoryview(data)
        for start in range(0, len(view), CHUNK):
            self.control.send(view[start:start + CHUNK])

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

    def settimeout(self, _timeout):
        """As the link of before's: here sending waits for the link (its
        backlog), and the link's own liveness says when the tablet is gone."""

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

    def send(self, body):
        try:
            self.channel.send(body)
        except OSError as e:
            if e.errno not in self.errors:
                self.errors.add(e.errno)
                log(f"{self.channel.feature}: cannot send:", e.strerror)

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
        self.channel.close()
