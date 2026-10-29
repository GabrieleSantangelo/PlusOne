"""Saved signatures: the list, 0.4's single one, merging with the tablet's."""
import json

from plusone import signatures
from plusone.ink import normalize
from plusone.pdf import Stroke


def drawn(x=0):
    return normalize([Stroke([(x, 0, 0.5), (x + 60, 20, 0.5)])])


def test_added_first_removed_and_used(tmp_path):
    a = signatures.add(drawn())
    b = signatures.add(drawn(10))
    found, removed = signatures.load()
    assert [s["id"] for s in found] == [b["id"], a["id"]]  # the newest first
    signatures.use(a["id"])
    assert [s["id"] for s in signatures.load()[0]] == [a["id"], b["id"]]
    signatures.remove(b["id"])
    assert signatures.load() == ([a], [b["id"]])


def test_the_single_signature_of_0_4_is_kept(tmp_path):
    (tmp_path / "signature.json").write_text(json.dumps(drawn()))
    first = signatures.load()[0]
    assert len(first) == 1 and first[0]["strokes"] == drawn()["strokes"]
    assert signatures.load()[0] == first  # its id does not change


def test_broken_files(tmp_path):
    assert signatures.load() == ([], [])
    (tmp_path / "signatures.json").write_text("{nope")
    assert signatures.load() == ([], [])
    (tmp_path / "signatures.json").write_text(json.dumps(
        {"signatures": [{"id": "x", "strokes": [[[0, "a", 1]]]}, {"strokes": [[[0, 0, 1]]]}, 3], "removed": [1]}))
    assert signatures.load() == ([], [])


def test_merging_with_the_tablet():
    ours = signatures.add(drawn())
    gone = signatures.add(drawn(5))
    signatures.remove(gone["id"])
    new_there = dict(drawn(20), id="tablet-new")
    removed_there = signatures.add(drawn(30))
    signatures.merge({"signatures": [new_there, gone, {"id": "bad"}],
                      "removed": [removed_there["id"]]})
    found, removed = signatures.load()
    # Ours first, then the tablet's new one; what either side removed stays away.
    assert [s["id"] for s in found] == [ours["id"], "tablet-new"]
    assert set(removed) == {gone["id"], removed_there["id"]}
    assert signatures.message() == {"signatures": found, "removed": removed}


def test_a_signature_whose_numbers_are_past_any_page_is_not_kept():
    # Finite, and yet inf once scaled onto a page (1e307 times its width):
    # the signing window's snapshot failed on it each time. A pressure of
    # 1e9 is no pressure either.
    import math
    from plusone.ink import bounds, place
    huge = dict(drawn(), id="huge", strokes=[[[0.0, 0.0, 0.5], [1e307, 0.0, 0.5]]])
    pressed = dict(drawn(), id="pressed", strokes=[[[0.0, 0.0, 0.5], [1.0, 0.2, 1e9]]])
    signatures.merge({"signatures": [huge, pressed, dict(drawn(), id="fine")], "removed": []})
    found, _removed = signatures.load()
    assert [s["id"] for s in found] == ["fine"]
    assert all(math.isfinite(v) for v in bounds(place(found[0], 100, 100)[0]))


def test_a_tablet_s_pressure_past_full_is_kept_as_full():
    # Android's pressure goes past 1.0 as the stylus's calibration says,
    # and the tablet keeps it as it came: the signature is one all the
    # same, and is drawn at full pressure.
    from plusone.ink import place
    theirs = dict(drawn(), id="tablet", strokes=[[[0.0, 0.0, 0.5], [1.0, 0.2, 1.02]]])
    signatures.merge({"signatures": [theirs], "removed": []})
    found, _removed = signatures.load()
    assert [s["id"] for s in found] == ["tablet"] and found[0]["strokes"][0][1] == [1.0, 0.2, 1.0]
    assert max(p for _x, _y, p in place(found[0], 100, 100)[0].points) == 1.0
    assert signatures.message()["signatures"][0]["strokes"] == found[0]["strokes"]
