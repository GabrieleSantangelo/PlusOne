"""Who is who: this PC's identity (a self-signed certificate) and the
tablets the user paired with (their certificates' fingerprints).

Pairing (link.pair) happens only when the user asks for it, choosing the
tablet in Plus One's window: both screens show a six-digit code made from
the two certificates and the user confirms it on both (like Bluetooth's
numeric comparison); then each side remembers the other's certificate. The
code also mixes a random number from each side, the app's committed to
before it sees the PC's: someone in the middle, showing each side a
certificate of their own, cannot make both screens show the same code but
by a one-in-a-million chance.
"""
import hashlib
import json
import os
import socket
import ssl
import subprocess
import threading

from gi.repository import GLib

from .. import log, write_private

MAX_NAME = 64
# (Named after the app's first name: the identity and pairings made then stay.)
CONFIG = os.path.join(GLib.get_user_config_dir(), "plus-one", "companion")
_making_identity = threading.Lock()


def pairing_code(a, b, ours, theirs):
    """Six digits from two certificates (DER, in either order) and the PC's
    and the app's random numbers; android/.../Identity.kt pairingCode
    computes the same."""
    first, second = sorted([a, b])
    digest = hashlib.sha256(first + second + ours + theirs).digest()
    return f"{int.from_bytes(digest[:4], 'big') % 1_000_000:06d}"


def commitment(nonce, app_cert, pc_cert):
    """What the app sends before it learns the PC's number: binds its own."""
    return hashlib.sha256(nonce + app_cert + pc_cert).hexdigest()


def fingerprint(der):
    return hashlib.sha256(der).hexdigest()


def identity(directory=None):
    """(certificate path, key path, certificate DER) of this PC, made on
    first use with openssl (on every Ubuntu, and in the GNOME runtime): an
    EC P-256 key and a self-signed certificate."""
    directory = directory or CONFIG
    cert, key = os.path.join(directory, "cert.pem"), os.path.join(directory, "key.pem")
    with _making_identity:  # two PDFs at once: one key, one certificate
        if not (os.path.exists(cert) and os.path.exists(key)):
            os.makedirs(directory, mode=0o700, exist_ok=True)
            os.chmod(directory, 0o700)  # the key in it is ours only
            subprocess.run(["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
                            "-nodes", "-keyout", key + ".part", "-out", cert + ".part", "-days", "10950",
                            "-subj", f"/CN=Plus One on {socket.gethostname()}"],
                           check=True, capture_output=True, timeout=30)
            os.chmod(key + ".part", 0o600)
            os.replace(key + ".part", key)
            os.replace(cert + ".part", cert)
    with open(cert) as f:
        der = ssl.PEM_cert_to_DER_cert(f.read())
    return cert, key, der


class Paired:
    """Tablets the user paired with, by certificate fingerprint: their name,
    and where their app was last on the network."""

    def __init__(self, directory=None):
        self.path = os.path.join(directory or CONFIG, "paired.json")

    def entries(self):
        """{fingerprint: {"name": ..., maybe "app_at": "ip:port"}}."""
        try:
            with open(self.path) as f:
                data = json.load(f)
        except (OSError, ValueError):
            return {}
        entries = {}
        for fp, entry in (data.items() if isinstance(data, dict) else ()):
            if isinstance(entry, str):
                entry = {"name": entry}  # 0.5 kept the name only
            if isinstance(entry, dict) and isinstance(entry.get("name"), str):
                entries[fp] = {key: value for key, value in entry.items()
                               if key == "name" or (key == "app_at" and isinstance(value, str))}
        return entries

    def _unreadable(self):
        """The file is there but cannot be read as pairings."""
        try:
            with open(self.path) as f:
                json.load(f)
        except FileNotFoundError:
            return False
        except (OSError, ValueError):
            return True
        return False

    def all(self):
        """{fingerprint: name}."""
        return {fp: entry["name"] for fp, entry in self.entries().items()}

    def add(self, fp, name):
        if self._unreadable():
            # Written over, the other tablets would be forgotten: kept aside.
            os.replace(self.path, self.path + ".unreadable")
            log("tablets: paired.json could not be read, kept as paired.json.unreadable")
        data = self.entries()
        data[fp] = dict(data.get(fp, {}), name=name)
        write_private(self.path, data)

    def learn(self, fp, **fields):
        """What was found out about a paired tablet (app_at=where its app was
        last on the network)."""
        data = self.entries()
        if fp in data and any(data[fp].get(key) != value for key, value in fields.items()):
            data[fp].update(fields)
            write_private(self.path, data)

    def forget(self, fp):
        data = self.entries()
        if data.pop(fp, None) is not None:
            write_private(self.path, data)
