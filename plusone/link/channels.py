"""A feature's session over the link (plus-one-link, docs/LINK.md), with
the link of before's surface (link.Link: request, hold, shutdown, close),
so the feature's code is the same: its control channel carries the same
JSON messages. More channels for its packets come with it (packets()):
what Datagrams carries over the link of before, over the link's own.
"""
import json
import select
import socket

from .. import log
from .link import Refused

try:
    import plusone_link
except ImportError:  # a Plus One without the link: the link of before only
    plusone_link = None

ANSWER_WAIT = 10.0  # seconds for the app's answer to a request


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
        device = next((d["id"] for d in connection.devices() if d["id"].startswith(app_id)), None)
        if device is None:
            raise plusone_link.LinkError("not a paired tablet")
        control = connection.open(device, feature, "control", plusone_link.CONTROL)
    except plusone_link.LinkError as e:
        log(f"{feature}: not over the link ({e})")
        connection.close()
        return None
    return ChannelLink(connection, device, feature, control)


class ChannelLink:
    """See the module. peer: None (no address: the packets go on channels)."""
    peer = None

    def __init__(self, connection, device, feature, control):
        self.connection, self.device, self.feature, self.control = connection, device, feature, control
        self._channels = []

    def send(self, header, payload=None):
        if payload is not None:
            raise ValueError("no payload on a control channel")
        self.control.send(json.dumps(header).encode())

    def receive(self, timeout=ANSWER_WAIT):
        data = self.control.recv(timeout=timeout)
        if data is None:
            raise TimeoutError(f"{self.feature}: no answer")
        if data == b"":
            raise EOFError(f"{self.feature}: the app hung up")
        return json.loads(data)

    def request(self, header, payload=None):
        """header to the app, and its answer; Refused if the app said no."""
        self.send(header, payload)
        reply = self.receive()
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
