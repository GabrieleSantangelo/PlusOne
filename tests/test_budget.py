"""plusone/link/channels.py's Budget: plus-one-link's word on the link's
budget heard until close(), whatever ends its watch meanwhile."""
import threading
import time
import types

import pytest

pytest.importorskip("plusone_link")
import plusone_link  # noqa: E402

from plusone.link import channels  # noqa: E402

DEVICE = "6a2665bc" * 8


class Watch:
    """A watch's connection, as plusone_link's: its thread ends with it
    (plus-one-link hung up, or close())."""

    def __init__(self, on_event):
        self.on_event, self.joined, self.closed = on_event, 0, False
        self._ended = threading.Event()
        self.thread = types.SimpleNamespace(join=self._join)

    def _join(self):
        self.joined += 1
        self._ended.wait()

    def hang_up(self):
        self._ended.set()

    def close(self):
        self.closed = True
        self._ended.set()


class FakeLink:
    """plus-one-link's connection, as Budget uses it: the budget it says
    (rate), the watches made, when each watch() was tried; not there for
    its first missing tries. during(): in the next watch(); asking(): in
    the next devices()."""

    def __init__(self, rate=9000, missing=0):
        self.rate, self.missing = rate, missing
        self.watches, self.tries, self.asked = [], [], 0
        self.during = self.asking = None

    def watch(self, on_event):
        self.tries.append(time.monotonic())
        if self.during:
            self.during()
        if self.missing:
            self.missing -= 1
            raise plusone_link.LinkError("no plus-one-link")
        watch = Watch(on_event)
        self.watches.append(watch)
        return watch

    def devices(self):
        self.asked += 1
        if self.asking:
            self.asking()
        return [{"id": DEVICE, "rate_kbps": self.rate}]


def until(condition, seconds=5):
    deadline = time.monotonic() + seconds
    while not condition():
        assert time.monotonic() < deadline, "never"
        time.sleep(0.01)


@pytest.fixture(autouse=True)
def soon(monkeypatch):
    """The 15 s from one watch to the next, 50 ms here."""
    monkeypatch.setattr(channels, "AGAIN", 0.05, raising=False)


def test_a_watch_plus_one_link_hung_up_is_made_again_and_what_moved_meanwhile_told():
    link = FakeLink(9000)
    budget = channels.Budget(link, DEVICE)
    told = []
    budget.watch(told.append)
    try:
        link.watches[0].on_event({"event": "budget", "device": DEVICE, "kbps": 12000})
        assert told == [12000] and budget.kbps() == 12000
        link.rate = 4000  # a cut while nothing watched: its event missed
        link.watches[0].hang_up()
        until(lambda: told == [12000, 4000])
        assert len(link.watches) == 2 and link.watches[0].closed  # its descriptor not left open
        assert budget.kbps() == 4000  # not the 12000 of before
        link.watches[1].on_event({"event": "budget", "device": DEVICE, "kbps": 5000})
        assert told == [12000, 4000, 5000] and budget.kbps() == 5000
        # Hung up again, nothing moved meanwhile: watched again, nothing told.
        asked = link.asked
        link.rate = 5000
        link.watches[1].hang_up()
        until(lambda: len(link.watches) == 3 and link.asked > asked)
        time.sleep(0.05)
        assert told == [12000, 4000, 5000]
    finally:
        budget.close()


def test_plus_one_link_not_there_is_looked_for_again_a_while_after_not_at_once():
    link = FakeLink(9000, missing=2)
    budget = channels.Budget(link, DEVICE)
    told = []
    budget.watch(told.append)
    try:
        until(lambda: told == [9000])  # watched at last: what it says, told
        assert len(link.tries) == 3 and len(link.watches) == 1
        assert all(b - a >= channels.AGAIN * 0.9 for a, b in zip(link.tries, link.tries[1:]))
    finally:
        budget.close()


@pytest.mark.parametrize("missing", [0, 1000])
def test_closed_nothing_is_watched_any_more(missing):
    link = FakeLink(missing=missing)
    budget = channels.Budget(link, DEVICE)
    budget.watch(lambda kbps: None)
    budget.close()
    assert all(watch.closed for watch in link.watches)
    budget._thread.join(1)
    assert not budget._thread.is_alive()
    time.sleep(0.2)  # AGAIN four times
    assert len(link.tries) == 1


def test_closed_while_a_watch_is_made_again_that_watch_is_not_left_open():
    link = FakeLink()
    budget = channels.Budget(link, DEVICE)
    told = []
    budget.watch(told.append)
    link.during = budget.close
    link.watches[0].hang_up()
    until(lambda: len(link.watches) == 2)
    budget._thread.join(1)
    assert not budget._thread.is_alive()
    assert link.watches[1].closed and told == []
    time.sleep(0.2)
    assert len(link.tries) == 2


def test_an_event_since_the_watch_was_made_again_is_not_undone_by_what_was_asked():
    link = FakeLink(9000)
    budget = channels.Budget(link, DEVICE)
    told = []
    budget.watch(told.append)
    try:
        link.rate = 4000
        link.asking = lambda: link.watches[-1].on_event({"event": "budget", "device": DEVICE, "kbps": 3000})
        link.watches[0].hang_up()
        until(lambda: told == [3000])
        time.sleep(0.05)
        link.asking = None
        assert told == [3000] and budget.kbps() == 3000  # the event's, newer than the answer
    finally:
        budget.close()


def test_a_watch_that_lasts_is_waited_on_not_looked_at_again_and_again():
    link = FakeLink()
    budget = channels.Budget(link, DEVICE)
    budget.watch(lambda kbps: None)
    try:
        time.sleep(0.3)  # AGAIN six times
        assert len(link.tries) == 1 and link.watches[0].joined == 1 and link.asked == 0
    finally:
        budget.close()
