"""The user's saved signatures, on the PC and in the tablet's app.

signatures.json holds {"signatures": [...], "removed": [...]}. A signature
is ink.py's normalized form (top-left at 0,0, width 1, and the width in
points it was drawn at) with an "id". The order is this side's own, the
most recently used first: "My signature" places the first.

The tablet's app keeps its own list. Each job takes this one to the app and
brings the app's back, and the two merge: every signature either side has,
except those either side removed. Removed ids are remembered, so a signature
removed on one side does not come back from the other.
"""
import json
import math
import os
import uuid

from gi.repository import GLib

from . import write_private

PATH = os.path.join(GLib.get_user_data_dir(), "plus-one", "signatures.json")
OLD = os.path.join(GLib.get_user_data_dir(), "plus-one", "signature.json")  # 0.4 kept one
MAX_SIGNATURES = 50
KEEP_REMOVED = 500  # removed ids remembered


MAX_POINTS = 20000  # samples in a signature: a long one has a few thousand
# Its coordinates, of a signature 1 wide: initials far taller than wide are
# a few; past this a number scaled onto a page was no longer one (inf).
MAX_COORDINATE = 1000.0
# Its pressures as the tablet keeps them, Android's own: past full (1.0)
# where the stylus's calibration says so, taken as full (_parse); past
# this, no pressure at all.
MAX_PRESSURE = 16.0


def _number(v):
    return isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v)


def _point(pt):
    """(x, y, pressure): coordinates within MAX_COORDINATE, pressure 0 to
    MAX_PRESSURE."""
    return (isinstance(pt, list) and len(pt) == 3 and all(_number(v) for v in pt)
            and abs(pt[0]) <= MAX_COORDINATE and abs(pt[1]) <= MAX_COORDINATE and 0 <= pt[2] <= MAX_PRESSURE)


def valid(signature):
    """Whether signature is a normalized signature with an id: points of
    three sane numbers (_point), no empty stroke, of a sane size."""
    try:
        strokes = signature["strokes"]
        width = signature.get("width", 1.0)
        return (isinstance(signature["id"], str) and 0 < len(signature["id"]) <= 64
                and _number(width) and width > 0
                and isinstance(strokes, list) and 0 < len(strokes)
                and sum(len(s) for s in strokes) <= MAX_POINTS
                and all(isinstance(s, list) and s and all(_point(pt) for pt in s) for s in strokes))
    except (KeyError, TypeError, AttributeError):
        return False


def _parse(data):
    """(signatures, removed ids) of a list ({"signatures", "removed"}): the
    valid ones, their pressures at most full. Every signature kept comes
    through here, whoever made it: what draws them never sees more."""
    if not isinstance(data, dict):
        return [], []
    found = data.get("signatures")
    removed = data.get("removed")
    kept, seen = [], set()
    for s in found if isinstance(found, list) else []:
        if valid(s) and s["id"] not in seen:  # one of each
            seen.add(s["id"])
            kept.append(dict(s, strokes=[[[x, y, min(1.0, p)] for x, y, p in stroke] for stroke in s["strokes"]]))
    return (kept[:MAX_SIGNATURES],
            list(dict.fromkeys(r for r in removed if isinstance(r, str) and 0 < len(r) <= 64))[-KEEP_REMOVED:]
            if isinstance(removed, list) else [])


def load(path=None, old=None):
    """(signatures, removed ids)."""
    path, old = path or PATH, old or OLD
    try:
        with open(path) as f:
            return _parse(json.load(f))
    except FileNotFoundError:
        pass
    except (OSError, ValueError):
        return [], []
    try:  # Plus One 0.4 kept a single signature
        with open(old) as f:
            single = json.load(f)
        if isinstance(single, dict) and single.get("strokes"):
            found, _removed = _parse({"signatures": [dict(single, id=uuid.uuid4().hex)]})
            if found:
                save(found, [], path)  # its id must stay the same
                return found, []
    except (OSError, ValueError):
        pass
    return [], []


def save(signatures, removed, path=None):
    write_private(path or PATH, {"signatures": signatures[:MAX_SIGNATURES], "removed": removed[-KEEP_REMOVED:]})


def thin(signature):
    """signature with at most MAX_POINTS samples, as valid() takes them back
    (else it is "saved", and dropped at the next load): every n-th, ends kept."""
    strokes = signature["strokes"]
    step = -(-sum(len(s) for s in strokes) // MAX_POINTS)
    if step <= 1:
        return signature
    return dict(signature, strokes=[[p for i, p in enumerate(s) if i % step == 0 or i == len(s) - 1]
                                    for s in strokes])


def add(signature, path=None):
    """Keeps a normalized signature, first in the list; returns it with its
    id. ValueError if it is no signature (nothing in it, numbers that are not)."""
    signatures, removed = load(path)
    kept, _removed = _parse({"signatures": [dict(thin(signature), id=uuid.uuid4().hex)]})
    if not kept:
        raise ValueError("not a signature")
    save(kept + signatures, removed, path)
    return kept[0]


def remove(signature_id, path=None):
    signatures, removed = load(path)
    save([s for s in signatures if s["id"] != signature_id], removed + [signature_id], path)


def use(signature_id, path=None):
    """Moves the signature to the front: "My signature" places it next time."""
    signatures, removed = load(path)
    chosen = [s for s in signatures if s["id"] == signature_id]
    if chosen and signatures[0] is not chosen[0]:
        save(chosen + [s for s in signatures if s["id"] != signature_id], removed, path)


def merged(ours, our_removed, theirs, their_removed):
    """(signatures, removed): ours in our order then theirs we lack, minus
    those either side removed."""
    removed = list(dict.fromkeys(our_removed + their_removed))
    gone = set(removed)
    known = {s["id"] for s in ours}
    return ([s for s in ours if s["id"] not in gone]
            + [s for s in theirs if s["id"] not in gone and s["id"] not in known]), removed


def merge(theirs, path=None):
    """Merges the tablet's list ({"signatures", "removed"}) into ours."""
    signatures, removed = load(path)
    their_signatures, their_removed = _parse(theirs)
    result = merged(signatures, removed, their_signatures, their_removed)
    if result != (signatures, removed):
        save(*result, path)


def message(path=None):
    """Our list as a job carries it to the tablet."""
    signatures, removed = load(path)
    return {"signatures": signatures, "removed": removed}
