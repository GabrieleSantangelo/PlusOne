"""What the app announces on the network instead of its id (SEC-15): a tag
per paired PC, which only that PC can tell is its tablet's, and only for a
quarter of an hour: nobody else on the network can follow the tablet, nor
learn which PCs it is paired with. The same in the app (Tags.kt) and the
link's daemon (mdns.rs):
    tag = SHA-256(app's fingerprint ‖ PC's fingerprint ‖ epoch, 8 bytes BE)[:8]
    epoch = Unix time // EPOCH
with the epoch before and after taken too (the two clocks need not agree)."""
import hashlib
import time

EPOCH = 900  # seconds


def tag(app_fp, pc_fp, epoch):
    """Both fingerprints in hex (SHA-256 of the DER certificate): 16 hex digits."""
    data = bytes.fromhex(app_fp) + bytes.fromhex(pc_fp) + epoch.to_bytes(8, "big")
    return hashlib.sha256(data).hexdigest()[:16]


def current(now=None):
    return int((time.time() if now is None else now) // EPOCH)


def recognised(tags, app_fp, pc_fp, now=None):
    """Whether one of the tags announced is the one for this app and PC, now."""
    epoch = current(now)
    return any(tag(app_fp, pc_fp, e) in tags for e in (epoch - 1, epoch, epoch + 1))
