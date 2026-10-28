"""Links: authenticated connections to the paired tablets' app.

The app listens on the network at APP_PORT (or where it is announced,
nearby.py). A link is a transport's stream (transport.py) secured with TLS
1.3, each side presenting its self-signed certificate (trust.py), and a
hello each way: the app's version, whether each knows the other. A tablet
not paired yet is paired only when the user asked (pair()); otherwise only
a paired tablet's app gets a link, and a certificate other than the paired
one where that tablet is announced is an Impostor.

Over a link, framed messages (wire.py): request() and its answer; hold()
keeps a session's link alive and notices when the app hangs up or is gone.
"""
import secrets
import socket
import ssl
import time

from .. import log
from .transport import open_stream
from .trust import MAX_NAME, Paired, commitment, fingerprint, identity, pairing_code
from .wire import MAX_HEADER, UNPAIRED_HEADER, Wire

PROTOCOL = 3
APP_PORT = 47236             # the app's (LinkServer.kt PORT), unless it was taken
CONNECT_TIMEOUT = 5          # seconds
PAIRING_TIMEOUT = 120        # seconds for the user to compare the codes
IO_TIMEOUT = 30              # seconds for a message to go through
LINK_TIMEOUT = 20            # seconds a link waits for the tablet to acknowledge what it sent
HANDSHAKE_FOR = 20           # seconds for TLS, hello and pairing messages (not the user's answer)
KEEPALIVE = 5                # seconds between a session's pings (the app lets go of a silent PC: LinkServer.kt SILENT_PC)


class Incompatible(Exception):
    """The app speaks another version of the protocol: one of the two needs updating."""


class PairingRefused(Exception):
    pass


class PairingClosed(PairingRefused):
    """The tablet takes a new PC only while its user holds its pairing
    window open (Plus One → Pair a new PC); it was shut."""


class Impostor(PairingRefused):
    """At an address announced for a paired tablet, an app with another
    certificate: someone answering mDNS for it. Never paired with."""


class Refused(Exception):
    """The app did not do what was asked: the reason, "off" (not let in the
    app), "permission" (not by Android), "background" (it could not come to
    the front to start), "busy" or "unavailable"."""


class Link:
    """A link to a paired tablet's app (connect()). name: the tablet's;
    fingerprint: its certificate's; hello: what its app said of itself."""

    def __init__(self, sock, wire, name, fingerprint, hello):
        self.sock, self.wire = sock, wire
        self.name, self.fingerprint, self.hello = name, fingerprint, hello

    @property
    def peer(self):
        """The app's address."""
        return self.sock.getpeername()[0]

    def send(self, header, payload=None):
        self.wire.send(header, payload)

    def receive(self, limit=MAX_HEADER):
        return self.wire.receive(limit)

    def readable(self, timeout):
        return self.wire.readable(timeout)

    def request(self, header, payload=None, limit=MAX_HEADER):
        """header (and its payload) to the app, and its answer; Refused if
        the app said no."""
        self.send(header, payload)
        reply = self.receive(limit)
        if reply.get("type") == "refused":
            raise Refused(str(reply.get("reason", "")))
        return reply

    def hold(self, until, check=None, tick=0.5, every=None):
        """While a session lasts, until until() is true: pings every so
        often (the app lets go of a silent PC), reads what the app says
        (nothing is expected; its hanging up raises EOFError, the tablet gone
        an OSError within LINK_TIMEOUT), and check() each tick, which may
        raise to end the session."""
        every = every or KEEPALIVE
        pinged = time.monotonic()
        while not until():
            if check:
                check()
            if self.readable(tick):
                self.receive()
            if time.monotonic() - pinged > every:
                self.send({"type": "ping"})
                pinged = time.monotonic()

    def settimeout(self, seconds):
        self.sock.settimeout(seconds)

    def shutdown(self):
        """From another thread: whatever waits on the link wakes, and ends."""
        try:
            self.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass

    def close(self):
        self.sock.close()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


def _handshake(target, paired, ask_pairing, expect=None, timeout=CONNECT_TIMEOUT, any_version=False):
    """(socket, wire, tablet name, its certificate's fingerprint, its hello)
    of a TLS link to the app at target, the tablet paired: pairing if needed
    when ask_pairing is given (the user chose this tablet to pair with; it
    asks them to compare the code), else raising PairingRefused. expect:
    the id the app was announced with; if it is a paired tablet's, that
    tablet's certificate is required (Impostor otherwise). any_version: an
    app of another protocol is not Incompatible (to update it), if both know
    each other."""
    cert, key, ours = identity()
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    context.minimum_version = ssl.TLSVersion.TLSv1_3
    # No CA vouches for either side: the app's certificate is checked
    # against the paired list (or the user compares the code).
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    context.load_cert_chain(cert, key)
    raw = open_stream(target, timeout)
    try:
        sock = context.wrap_socket(raw)
    except Exception:
        raw.close()
        raise
    deadline = time.monotonic() + HANDSHAKE_FOR

    def small():
        """A message of the handshake: small, and all of them in time."""
        sock.settimeout(max(0.1, deadline - time.monotonic()))
        return wire.receive(UNPAIRED_HEADER)

    try:
        theirs = sock.getpeercert(binary_form=True)
        fp = fingerprint(theirs)
        wire = Wire(sock)
        known = paired.all()
        pinned = [p for p in known if expect and p.startswith(expect)]
        if pinned and fp not in pinned:
            raise Impostor(f"an app at {target[0]} announced as a paired tablet, with another certificate")
        known = fp in known
        sock.settimeout(IO_TIMEOUT)
        wire.send({"type": "hello", "name": socket.gethostname(), "version": PROTOCOL, "paired": known,
                   "pair": ask_pairing is not None})
        hello = small()
        if hello.get("pairing") == "closed" and ask_pairing is not None:
            raise PairingClosed("the tablet's pairing window is shut")
        name = str(hello.get("name") or "tablet")[:MAX_NAME]
        if hello.get("version") != PROTOCOL:
            if not any_version:
                raise Incompatible(f"{name} speaks version {hello.get('version')}, this PC {PROTOCOL}")
            if not known or not hello.get("paired"):
                raise PairingRefused(name)  # never paired across versions
        if not known or not hello.get("paired"):
            if ask_pairing is None:
                raise PairingRefused(name)
            # The app commits to its number, learns ours, then shows its own.
            commit = small()
            nonce = secrets.token_bytes(16)
            wire.send({"type": "pair-nonce", "nonce": nonce.hex()})
            reveal = small()
            their_nonce = bytes.fromhex(str(reveal.get("nonce", "")))
            if (commit.get("type") != "pair-commit" or len(their_nonce) != 16
                    or commit.get("commit") != commitment(their_nonce, theirs, ours)):
                raise PairingRefused(name)
            code = pairing_code(ours, theirs, nonce, their_nonce)
            log(f"link: pairing with {name}, code {code}")
            sock.settimeout(PAIRING_TIMEOUT + 10)
            accepted = ask_pairing(name, code)
            wire.send({"type": "pair-ok" if accepted else "pair-no"})
            answer = wire.receive(UNPAIRED_HEADER)
            if not accepted or answer.get("type") != "pair-ok":
                raise PairingRefused(name)
            paired.add(fp, name)
        # The handshake's budget (0.1 s at worst by now) is no timeout for what follows.
        sock.settimeout(IO_TIMEOUT)
        return sock, wire, name, fp, hello
    except Exception:
        sock.close()
        raise


def pair(targets, ask, paired=None):
    """Blocking: pairs with the app at targets (the addresses of the tablet
    the user picked), ask(name, code) -> bool asking them to compare the
    code, which the tablet shows too. Returns (fingerprint, name); raises
    PairingRefused if either side said no, OSError & co. if it did not answer."""
    paired = paired or Paired()
    error = None
    for target in targets:
        try:
            sock, wire, name, fp, _hello = _handshake(target, paired, ask)
        except (OSError, EOFError, ValueError) as e:
            error = e
            continue
        with sock:
            try:
                wire.send({"type": "bye"})
            except OSError:
                pass
        return fp, name
    raise error or OSError("no address for the tablet")


def connect(targets, expect, paired=None, any_version=False):
    """Blocking: a Link to the paired tablet's app at the first of targets
    (its addresses: a (host, port), or a list of them) that answers as it
    should. expect: the id the app is announced with (_handshake)."""
    paired = paired or Paired()
    error = None
    for target in [targets] if isinstance(targets, tuple) else targets:
        try:
            sock, wire, name, fp, hello = _handshake(target, paired, None, expect, any_version=any_version)
        except Impostor as e:
            log("link:", e)
            error = e
            continue
        except (OSError, EOFError, ValueError) as e:
            error = e
            continue
        # A tablet gone from the network (off, out of range) answers nothing,
        # not even a reset: what was sent goes unacknowledged, and the kernel
        # gives the link up after LINK_TIMEOUT instead of TCP's quarter of an hour.
        try:
            sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_USER_TIMEOUT, LINK_TIMEOUT * 1000)
        except OSError:
            pass  # not TCP: its transport has its own
        return Link(sock, wire, name, fp, hello)
    raise error or OSError("no address for the tablet")
