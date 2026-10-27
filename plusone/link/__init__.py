"""Plus One's connectivity: everything between this PC and its paired
tablets, under the services that use it (the desktop, the speakers, the
camera and the microphone, signing, updates). A service never opens a
socket of its own: it asks for what it needs here.

The primitives:

- Discovery (discovery.py): which apps are around, where, of which
  version, from every source at once: mDNS (nearby.py) today; another way
  of finding a tablet is another source beside it.
- Trust (trust.py): this PC's identity, the paired tablets; pair() makes
  one, when the user asks (both screens show the same code).
- The tablets (tablets.py): the paired ones, the one in use, and where to
  reach it (where it is announced, else where it was last).
- Transports (transport.py): how a connection is opened (TCP today).
- Links (link.py): connect() gives a Link to a paired tablet's app,
  authenticated (TLS, the paired certificates): request() and its answer,
  send()/receive() for a protocol of its own (signing's), hold() to keep a
  session's link alive until it ends or the tablet is gone.
- Datagrams (datagrams.py): a session's UDP, sealed both ways, for what
  must not wait (frames, input, sound); its key goes over the link.
"""
from .datagrams import Datagrams
from .discovery import Discovery
from .link import (APP_PORT, IO_TIMEOUT, KEEPALIVE, LINK_TIMEOUT, PAIRING_TIMEOUT, PROTOCOL, Impostor,
                   Incompatible, Link, PairingRefused, Refused, connect, pair)
from .nearby import Nearby
from .tablets import Tablet, Tablets
from .trust import MAX_NAME, Paired, fingerprint, identity
from .wire import MAX_HEADER, UNPAIRED_HEADER, Wire

__all__ = ["APP_PORT", "Datagrams", "Discovery", "IO_TIMEOUT", "Impostor", "Incompatible", "KEEPALIVE", "LINK_TIMEOUT",
           "Link", "MAX_HEADER", "MAX_NAME", "Nearby", "PAIRING_TIMEOUT", "PROTOCOL", "Paired", "PairingRefused",
           "Refused", "Tablet", "Tablets", "UNPAIRED_HEADER", "Wire", "connect", "fingerprint", "identity", "pair"]
