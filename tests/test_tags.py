"""The tags the app announces instead of its id (tags.py; the same vector
in Tags.kt's and mdns.rs's tests)."""
from plusone.link import nearby, tags

APP, PC = "aa" * 32, "bb" * 32


def test_the_shared_vector():
    assert tags.tag(APP, PC, 2_000_000) == "19ab5569a757519b"
    assert tags.tag(APP, PC, 2_000_001) == "c2cc4faea63bb065"


def test_a_quarter_of_an_hour_either_way_is_recognised():
    now = 2_000_000 * tags.EPOCH + 10
    assert tags.recognised({tags.tag(APP, PC, 1_999_999)}, APP, PC, now)
    assert tags.recognised({tags.tag(APP, PC, 2_000_001)}, APP, PC, now)
    assert not tags.recognised({tags.tag(APP, PC, 1_999_998)}, APP, PC, now)
    assert not tags.recognised({tags.tag(APP, "cc" * 32, 2_000_000)}, APP, PC, now)  # another PC's


def txt(**fields):
    return [f"{k}={v}".encode() for k, v in fields.items()]


def test_only_this_pcs_tablets_are_told_by_their_tags():
    watching = nearby.Nearby(paired=lambda: {APP: "Tab"}, ours=lambda: PC)
    mine = tags.tag(APP, PC, tags.current())
    assert watching._which(txt(t=f"0123456789abcdef,{mine}")) == (APP[:16], "Tab", 0)
    assert watching._which(txt(t="0123456789abcdef")) is None  # someone else's tablet: nothing of it
    # Its pairing open: it says who it is, for a while.
    assert watching._which(txt(t="", pair=1, id="dd" * 8, name="New")) == ("dd" * 8, "New", 0)
    # An app from before tags: as it says.
    assert watching._which(txt(id="ee" * 8, name="Old", v=1028)) == ("ee" * 8, "Old", 1028)
