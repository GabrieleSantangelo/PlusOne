"""Signing in the tablet's app: a PDF goes over a link (link.py) as a job,
the user signs it there, and its marks come back. The app keeps a job until
the PC says it got the answer: should the link drop meanwhile, the PC
connects again and asks for it (resume).

Messages (over the link): "sign" (job id, title, pen, the PDF's SHA-256,
the saved signatures if they are shared; the PDF as payload) -> "opened"
-> "signed" (marks, the SHA-256 of the PDF signed, the signatures there if
shared) or "cancelled" -> "got"; "resume" (job id) after a drop; "ping"
while the user takes their time. Marks for another PDF than the one sent
are not taken (Mismatch).

Over the link (plus-one-link: by cable too) when it reaches the tablet,
its "sign" feature; else over the link of before.
"""
import hashlib
import math
import os
import threading
import time
import unicodedata
import uuid

from . import log
from .link import IO_TIMEOUT, Incompatible, Paired, PairingRefused, channels, connect

PING_EVERY = 15              # seconds of silence while the user signs before asking "still there?"
SILENT_FOR = 45              # seconds of silence after which the link is taken for dead


class Mismatch(Exception):
    """The tablet answered with marks for another PDF than the one sent (its
    SHA-256 not the one sent): none of them is taken."""


class Lost(Exception):
    """The link went while the user was signing on the tablet, and it did
    not come back in time: what was signed there did not reach the PC."""


RESUME_FOR = 600     # seconds to look for the tablet again after the link drops mid-job
RESUME_EVERY = 5     # seconds between attempts
MAX_JOBS = 24        # PDFs waiting on the tablet at once (the app takes 32 connections)


class _Turns:
    """PDFs sent together (five selected in Files) go to the tablet one at a
    time, in the order they were asked for: the tablet's queue keeps that
    order."""

    def __init__(self):
        self._changed = threading.Condition()
        self._issued = 0
        self._now = 0

    def ticket(self):
        with self._changed:
            self._issued += 1
            return self._issued - 1

    def wait(self, ticket):
        with self._changed:
            self._changed.wait_for(lambda: self._now == ticket)

    def done(self, ticket):
        with self._changed:
            self._now = max(self._now, ticket + 1)
            self._changed.notify_all()


_turns = _Turns()
_places = threading.BoundedSemaphore(MAX_JOBS)


def next_turn():
    """A place in line for sign(), taken when the PDF is asked for (its order)."""
    return _turns.ticket()


def _answer(link, job, digest, on_opened, name, on_signatures):
    """Waits for the job's answer: the result, or None if cancelled;
    Mismatch for marks on another PDF than the one of digest. Asks
    whether the app is still there when it is quiet for a while: a link
    that died without a word (Wi-Fi gone) raises TimeoutError."""
    heard = time.monotonic()
    while True:
        if not link.readable(PING_EVERY):
            if time.monotonic() - heard > SILENT_FOR:
                raise TimeoutError("the tablet stopped answering")
            link.send({"type": "ping"})
            continue
        message = link.receive()
        heard = time.monotonic()
        kind = message.get("type")
        if kind == "opened" and on_opened:
            on_opened(name)
        elif kind == "resumed":
            log("signing: the tablet still has the job")
        elif kind == "unknown-job":
            raise Lost("the tablet no longer has the job")
        elif kind in ("signed", "cancelled") and message.get("job") == job:
            # The app keeps the answer until it knows it arrived.
            link.send({"type": "got", "job": job})
            link.send({"type": "bye"})
            if kind == "signed" and message.get("sha256") != digest:
                raise Mismatch(f"marks for a PDF of SHA-256 {message.get('sha256')!r}")
            if on_signatures and isinstance(message.get("signatures"), dict):
                on_signatures(message["signatures"])
            marks = message.get("marks") if kind == "signed" else None
            return (marks if isinstance(marks, dict) else {"pages": []}) if kind == "signed" else None


def sign(target, path, paired=None, on_opened=None, pen=None, signatures=None,
         on_signatures=None, locate=None, resume_for=RESUME_FOR, turn=None, expect=None, title=None):
    """Blocking (run it in a thread): send the PDF at path to the app at
    target ((host, port), or a list of them: the addresses the app was
    announced at, tried in turn) and wait for the user to sign it there.
    expect: the id it was announced with (link.connect). Returns the
    result ({"pages": [...], "pen": ..., maybe "signature": ...}) or None if
    the user cancelled. Only to a paired tablet: pairing is link.pair()'s, when
    the user chooses a tablet in the window. pen ({"color", "width"}) and
    the saved signatures (signatures.message()) go along, so the tablet
    signs like the PC; on_signatures(the tablet's list) runs with the answer,
    if it carries one (only when they are shared: signatures, both sides).

    Several at once: each goes in its turn (next_turn(), or one taken now),
    and at most MAX_JOBS wait on the tablet; more wait here for a place.

    Should the link drop once the PDF is on the tablet, the app keeps the
    job: Plus One connects again (to locate()'s addresses, the app's port may
    have changed) and asks for it, for resume_for seconds.

    Raises OSError, EOFError, ValueError, PairingRefused or Incompatible
    before the PDF reached the tablet, Lost or Mismatch after."""
    paired = paired or Paired()
    turn = next_turn() if turn is None else turn
    try:
        _turns.wait(turn)
        _places.acquire()
        try:
            link, job, digest = _send(target, path, paired, pen, signatures, expect, title)
        except BaseException:
            _places.release()
            raise
    finally:
        _turns.done(turn)
    try:
        return _wait(link, job, digest, target, paired, on_opened, on_signatures, locate, resume_for, expect)
    finally:
        _places.release()


def _connect(target, expect, paired):
    """The tablet's "sign" over the link if it is there (expect: its id),
    else over the link of before (target: its addresses)."""
    tablet = channels.connect(expect, "sign") if expect else None
    return tablet or connect(target, expect, paired)


def _send(target, path, paired, pen, signatures, expect, title=None):
    """(link, job id, the PDF's SHA-256) once the PDF is on the tablet."""
    link = _connect(target, expect, paired)
    job = uuid.uuid4().hex
    try:
        with open(path, "rb") as f:
            pdf = f.read()
        link.settimeout(IO_TIMEOUT * 4)  # a big PDF over slow Wi-Fi
        digest = hashlib.sha256(pdf).hexdigest()
        header = {"type": "sign", "job": job, "title": title or os.path.basename(path), "sha256": digest}
        if pen:
            header["pen"] = pen
        if signatures:
            header["signatures"] = signatures
        link.send(header, pdf)
        link.settimeout(IO_TIMEOUT)
    except BaseException:
        link.close()
        raise
    return link, job, digest


def _wait(link, job, digest, target, paired, on_opened, on_signatures, locate, resume_for, expect):
    """The job's answer, connecting again if the link drops (sign())."""
    name = link.name
    with link:
        try:
            return _answer(link, job, digest, on_opened, name, on_signatures)
        except Mismatch:
            raise
        except Exception as e:  # noqa: BLE001 - the PDF is on the tablet: whatever broke, ask again
            log("signing: lost the tablet while signing:", repr(e))
    deadline = time.monotonic() + resume_for
    while time.monotonic() < deadline:
        time.sleep(RESUME_EVERY)
        where = locate() if locate else target
        if not where:
            continue
        try:
            link = _connect(where, expect, paired)
        except (OSError, EOFError, ValueError, PairingRefused, Incompatible):
            continue
        with link:
            try:
                link.send({"type": "resume", "job": job})
                log("signing: back in touch with the tablet")
                return _answer(link, job, digest, None, link.name, on_signatures)
            except Mismatch:
                raise
            except Exception as e:  # noqa: BLE001 - as above
                log("signing: lost the tablet again:", repr(e))
    raise Lost(name)


def _finite(value):
    number = float(value)  # TypeError, ValueError: not a number
    if not math.isfinite(number):
        raise ValueError("not a finite number")
    return number


# Characters that reverse how the text around them reads (an "exe.pdf" trick).
BIDI_OVERRIDES = set("\u202a\u202b\u202c\u202d\u202e\u2066\u2067\u2068\u2069")


def to_marks(result):
    """The app's marks as plusone/pdf.py takes them: {page index: [Stroke |
    Text]}. What does not make sense (not a page index, not finite, a colour
    not of three parts, control characters) is left out, mark by mark."""
    from .pdf import INK, PEN_WIDTH, Stroke, Text

    marks = {}
    pages = result.get("pages") if isinstance(result, dict) else None
    for page in pages if isinstance(pages, list) else []:
        index = page.get("page") if isinstance(page, dict) else None
        if type(index) is not int or index < 0:
            continue
        found = []
        for mark in page.get("marks") if isinstance(page.get("marks"), list) else []:
            try:
                color = tuple(min(1.0, max(0.0, _finite(c))) for c in mark.get("color", INK))
                if len(color) != 3:
                    raise ValueError("a colour of three parts")
                if mark.get("type") == "stroke":
                    p = [_finite(v) for v in mark.get("points", [])]
                    points = [(p[i], p[i + 1], min(1.0, max(0.0, p[i + 2]))) for i in range(0, len(p) - 2, 3)]
                    width = _finite(mark.get("width", PEN_WIDTH))
                    if points and 0 < width <= 100:
                        found.append(Stroke(points, width=width, color=color))
                elif mark.get("type") == "text" and isinstance(mark.get("text"), str):
                    # Controls and direction overrides out; joiners (Persian,
                    # emoji) kept, which isprintable() took too.
                    text = "".join(ch for ch in mark["text"] if unicodedata.category(ch) != "Cc"
                                   and ch not in BIDI_OVERRIDES)[:500]
                    size = _finite(mark.get("size", 11.0))
                    if text and 0 < size <= 500:
                        found.append(Text(_finite(mark["x"]), _finite(mark["y"]), text, size=size, color=color,
                                          angle=_finite(mark.get("angle", 0.0))))
            except (AttributeError, KeyError, TypeError, ValueError):
                log("signing: a mark from the tablet skipped")
        if found:
            marks[index] = found
    return marks
