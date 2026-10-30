"""The app display's packets: their layout, sealed both ways, replays refused."""
import collections
import json
import math
import os
import socket
import struct
import threading
import time
import types

import pytest

from plusone import appdisplay  # noqa: E402
from plusone.link.datagrams import FROM_APP, FROM_PC, Datagrams  # noqa: E402


@pytest.fixture(autouse=True)
def l36_as_it_ships(monkeypatch):
    """The desktop as it ships, whatever the shell running the tests set
    (PLUSONE_L36, its knobs)."""
    for name in [name for name in os.environ if name.startswith("PLUSONE_L36")]:
        monkeypatch.delenv(name)

crypto = pytest.importorskip("cryptography.hazmat.primitives.ciphers.aead")
AESGCM = crypto.AESGCM

KEY = bytes(range(32))


def channel_to(peer):
    channel = Datagrams(KEY)
    channel.app = peer.getsockname()
    return channel


def test_a_video_packet_as_the_app_reads_it():
    app = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    app.bind(("127.0.0.1", 0))
    app.settimeout(2)
    channel = channel_to(app)
    try:
        body = appdisplay.VIDEO_HEADER.pack(appdisplay.VIDEO, 1, 7, 0, 3, 123456789) + b"h264"
        channel.send(body)
        channel.send(body)
        first, second = app.recv(2048), app.recv(2048)
    finally:
        channel.close()
        app.close()
    # DisplaySession.kt: nonce = direction (4 bytes) and count (8), then the sealed body.
    assert struct.unpack("!IQ", first[:12]) == (FROM_PC, 0)
    assert struct.unpack("!IQ", second[:12]) == (FROM_PC, 1)  # never the same nonce twice
    assert AESGCM(KEY).decrypt(first[:12], first[12:], None) == body
    assert len(appdisplay.VIDEO_HEADER.pack(1, 0, 0, 0, 1, 0)) + appdisplay.PAYLOAD + 12 + 16 <= 1472


def test_only_the_apps_fresh_packets_are_taken():
    app = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    app.bind(("127.0.0.1", 0))
    channel = channel_to(app)
    pc = ("127.0.0.1", channel.port)

    def from_app(count, body, key=KEY, direction=FROM_APP):
        nonce = struct.pack("!IQ", direction, count)
        app.sendto(nonce + AESGCM(key).encrypt(nonce, body, None), pc)

    try:
        from_app(5, b"\x02")
        assert channel.receive(1) == b"\x02"
        from_app(5, b"\x02")                       # replayed
        from_app(4, b"\x02")                       # older
        from_app(6, b"\x02", key=bytes(32))        # another key
        from_app(7, b"\x02", direction=FROM_PC)  # its own packets sent back
        assert channel.receive(0.3) is None
        assert channel.receive(0.3) is None
        assert channel.receive(0.3) is None
        assert channel.receive(0.3) is None
        from_app(8, b"\x03" + bytes(8))
        assert channel.receive(1) == b"\x03" + bytes(8)
    finally:
        channel.close()
        app.close()


class FakeMonitor:
    def __init__(self):
        self.calls = []

    def __getattr__(self, name):
        return lambda *args: self.calls.append((name, *args))


def fingers(*points):
    """An INPUT packet with these fingers (id, x, y in 0..1) and no pen."""
    body = bytes([appdisplay.INPUT, len(points)])
    for finger, x, y in points:
        body += appdisplay.FINGER.pack(finger, int(x * appdisplay.TOUCH_MAX), int(y * appdisplay.TOUCH_MAX))
    return body + b"\x00"


class Clock:
    def __init__(self):
        self.now = 100.0

    def __call__(self):
        return self.now


def touch_input():
    clock, monitor = Clock(), FakeMonitor()
    return appdisplay.Input(monitor, (1000, 1000), lambda: None, clock=clock), monitor, clock


def test_a_finger_tapped_is_the_left_button():
    # Not a touch tapped, which some programs (Qt's) take as hover: a click,
    # where the finger came down.
    touch, monitor, clock = touch_input()
    assert touch.packet(fingers((0, 0.5, 0.5)))
    assert monitor.calls == []  # a drag, or a second finger, may come
    clock.now += 0.08
    touch.packet(fingers((0, 0.505, 0.5)))  # a tap's wobble
    clock.now += 0.03
    touch.packet(fingers())
    assert monitor.calls == [("pointer_move", pytest.approx(500, abs=0.1), pytest.approx(500, abs=0.1)),
                             ("pointer_button", appdisplay.pen.BTN_LEFT, True),
                             ("pointer_button", appdisplay.pen.BTN_LEFT, False)]


def test_a_finger_that_moves_drags_with_the_left_button():
    touch, monitor, clock = touch_input()
    touch.packet(fingers((0, 0.5, 0.5)))
    clock.now += 0.04
    touch.packet(fingers((0, 0.52, 0.5)))
    assert monitor.calls == []  # a second finger may still come
    clock.now += appdisplay.DRAG_WAIT
    touch.check()  # text selected, a window moved: from where it began
    assert [c[0] for c in monitor.calls] == ["pointer_move", "pointer_button", "pointer_move"]
    assert monitor.calls[0][1] == pytest.approx(500, abs=0.1)
    assert monitor.calls[1][1:] == (appdisplay.pen.BTN_LEFT, True)
    touch.packet(fingers())
    assert monitor.calls[-1] == ("pointer_button", appdisplay.pen.BTN_LEFT, False)


def test_a_finger_held_is_the_right_button():
    touch, monitor, clock = touch_input()
    touch.packet(fingers((0, 0.5, 0.5)))
    clock.now += appdisplay.HOLD / 2
    touch.packet(fingers((0, 0.5, 0.5)))  # the app says it again every 0.1 s
    assert monitor.calls == []
    clock.now += appdisplay.HOLD / 2 + 0.01
    touch.check()
    assert [c[1:] for c in monitor.calls if c[0] == "pointer_button"] == [
        (appdisplay.pen.BTN_RIGHT, True), (appdisplay.pen.BTN_RIGHT, False)]
    touch.packet(fingers())
    assert len(monitor.calls) == 3  # lifted: nothing more


def scroll(touch, clock, step=0.02, times=5):
    """Two fingers moved up together, a packet every 10 ms."""
    y = 0.5
    touch.packet(fingers((0, 0.4, y), (1, 0.6, y)))
    for _ in range(times):
        clock.now += 0.01
        y -= step
        touch.packet(fingers((0, 0.4, y), (1, 0.6, y)))


def test_two_fingers_scroll_and_a_flick_goes_on():
    touch, monitor, clock = touch_input()
    scroll(touch, clock)
    axes = [c for c in monitor.calls if c[0] == "pointer_axis"]
    assert monitor.calls[0][0] == "pointer_move" and axes  # what is under the fingers scrolls
    assert all(c[1] == 0 and c[2] > 0 for c in axes)  # fingers up: the page with them (further down)
    clock.now += 0.005
    touch.packet(fingers())  # flicked: it goes on by itself
    assert touch.fling is not None and touch.busy()
    moved = []
    for _ in range(200):
        clock.now += 0.02
        touch.check()
        moved += [c[2] for c in monitor.calls[-1:] if c[0] == "pointer_axis"]
        if touch.fling is None:
            break
    assert touch.fling is None and monitor.calls[-1] == ("pointer_axis", 0.0, 0.0, True)  # finished
    assert moved[0] > moved[len(moved) // 2] > 0  # slowing down
    assert sum(moved) > 100  # 2 metres a second for 0.35 s: about 700 px of 1000


def test_a_slow_scroll_stops_where_the_fingers_lift():
    touch, monitor, clock = touch_input()
    scroll(touch, clock, step=0.001, times=30)
    clock.now += 0.2  # held still a moment before lifting
    touch.packet(fingers((0, 0.4, 0.47), (1, 0.6, 0.47)))
    touch.packet(fingers())
    assert touch.fling is None and monitor.calls[-1] == ("pointer_axis", 0.0, 0.0, True)


def test_a_finger_down_stops_a_flick():
    touch, monitor, clock = touch_input()
    scroll(touch, clock)
    touch.packet(fingers())
    assert touch.fling is not None
    clock.now += 0.02
    touch.packet(fingers((0, 0.5, 0.5)))
    assert touch.fling is None and monitor.calls[-1] == ("pointer_axis", 0.0, 0.0, True)


def test_a_pinch_is_a_pinch_on_the_desktop():
    # As touches, from where the fingers began: a browser zooms the picture
    # under them, as a touchpad's pinch does.
    touch, monitor, clock = touch_input()
    touch.packet(fingers((0, 0.45, 0.5), (1, 0.55, 0.5)))  # 100 px apart
    for spread in (0.06, 0.07):
        clock.now += 0.02
        touch.packet(fingers((0, 0.5 - spread, 0.5), (1, 0.5 + spread, 0.5)))
    touch.packet(fingers())
    assert [c[:2] for c in monitor.calls] == [("touch_down", 0), ("touch_down", 1),
                                               ("touch_motion", 0), ("touch_motion", 1),
                                               ("touch_motion", 0), ("touch_motion", 1),
                                               ("touch_up", 0), ("touch_up", 1)]
    assert monitor.calls[0][2] == pytest.approx(450, abs=0.1)  # where it began


def test_a_pinch_whose_second_finger_came_late_is_a_pinch():
    # The first finger lands and moves a little before the second: not a
    # drag (a selection), a pinch.
    touch, monitor, clock = touch_input()
    touch.packet(fingers((0, 0.45, 0.5)))
    clock.now += 0.03
    touch.packet(fingers((0, 0.44, 0.5)))  # moved 1%: a drag, were it alone
    clock.now += 0.03
    touch.packet(fingers((0, 0.44, 0.5), (1, 0.55, 0.5)))
    for x in (0.57, 0.6, 0.63):
        clock.now += 0.02
        touch.packet(fingers((0, 0.43, 0.5), (1, x, 0.5)))
    assert "pointer_button" not in [c[0] for c in monitor.calls]
    assert [c[0] for c in monitor.calls][:2] == ["touch_down", "touch_down"]


def test_a_drag_a_second_finger_soon_joins_is_let_go():
    touch, monitor, clock = touch_input()
    touch.packet(fingers((0, 0.4, 0.5)))
    clock.now += 0.03
    touch.packet(fingers((0, 0.48, 0.5)))  # fast: a drag at once
    assert ("pointer_button", appdisplay.pen.BTN_LEFT, True) in monitor.calls
    clock.now += 0.05
    touch.packet(fingers((0, 0.48, 0.5), (1, 0.6, 0.5)))
    assert monitor.calls[-1] == ("pointer_button", appdisplay.pen.BTN_LEFT, False)
    clock.now += 0.02
    touch.packet(fingers((0, 0.48, 0.45), (1, 0.6, 0.45)))  # and they scroll
    assert monitor.calls[-1][0] == "pointer_axis"


def test_far_apart_fingers_pinching_are_not_a_scroll():
    # One finger still, the other moving out: the spread changes twice as
    # much as the middle moves, however far apart they are.
    touch, monitor, clock = touch_input()
    touch.packet(fingers((0, 0.1, 0.5), (1, 0.8, 0.5)))  # 700 px apart
    for x in (0.81, 0.82, 0.84):
        clock.now += 0.02
        touch.packet(fingers((0, 0.1, 0.5), (1, x, 0.5)))
    assert monitor.calls[0][0] == "touch_down"


def test_three_fingers_swiped_up_show_the_overview():
    touch, monitor, clock = touch_input()
    touch.packet(fingers((0, 0.4, 0.6), (1, 0.5, 0.6), (2, 0.6, 0.6)))
    clock.now += 0.05
    touch.packet(fingers((0, 0.4, 0.45), (1, 0.5, 0.45), (2, 0.6, 0.45)))
    touch.packet(fingers((0, 0.4, 0.3), (1, 0.5, 0.3), (2, 0.6, 0.3)))  # once only
    touch.packet(fingers())
    assert [c[1:] for c in monitor.calls] == [(appdisplay.KEY_SUPER, True), (appdisplay.KEY_SUPER, False)]


def test_two_fingers_resting_do_nothing():
    touch, monitor, clock = touch_input()
    touch.packet(fingers((0, 0.3, 0.4), (1, 0.4, 0.4)))
    clock.now += appdisplay.TAP_TIME + 0.01
    touch.packet(fingers((0, 0.3, 0.4), (1, 0.4, 0.4)))
    touch.packet(fingers())
    assert monitor.calls == []


def test_silence_lifts_what_touches():
    touch, monitor, clock = touch_input()
    touch.packet(fingers((0, 0.5, 0.5)))
    clock.now += 0.04
    touch.packet(fingers((0, 0.6, 0.5)))  # a drag
    clock.now += appdisplay.INPUT_SILENCE
    touch.check()
    assert monitor.calls[-1] == ("pointer_button", appdisplay.pen.BTN_LEFT, False) and not touch.busy()


def test_the_listener_looks_often_only_while_something_touches():
    touch, _monitor, clock = touch_input()
    assert not touch.busy()
    touch.packet(fingers((0, 0.5, 0.5)))
    assert touch.busy()  # a tap, a drag or a hold: time tells
    touch.packet(fingers())
    assert not touch.busy()


def soon(condition, timeout=5):
    end = time.monotonic() + timeout
    while not condition() and time.monotonic() < end:
        time.sleep(0.005)
    return condition()


def pen_near(bits=appdisplay.pen.IN_RANGE):
    """An INPUT packet with no finger and one sample of the pen."""
    return bytes([appdisplay.INPUT, 0, 1, bits]) + struct.pack("<HHHbb", 16000, 16000, 0, 0, 0)


def test_the_pen_is_made_off_the_listening_thread():
    # Making the pen (D-Bus, the helper's polkit) takes up to 25 s: the
    # listening thread, which carries the fingers, keyframes and stats,
    # never waits for it; the pen's reports reach it once it is there.
    makers, go, reports = [], threading.Event(), []
    device = types.SimpleNamespace(report=reports.append, close=lambda: None)
    touch = appdisplay.Input(FakeMonitor(), (1000, 1000),
                             lambda: makers.append(threading.current_thread()) or go.wait(5) and device)
    listening = threading.Thread(target=lambda: (touch.packet(pen_near()), touch.packet(fingers((0, 0.5, 0.5)))))
    listening.start()
    listening.join(1)
    held = listening.is_alive()
    go.set()
    listening.join(5)
    assert not held and makers and makers[0] is not listening
    assert soon(lambda: touch.pen is device)
    touch.packet(pen_near(appdisplay.pen.IN_RANGE | appdisplay.pen.TIP))
    assert len(reports) == 1


def test_the_pen_is_made_as_the_desktop_comes_up():
    # Ahead of the first stroke: as the first frame goes (the monitor is
    # Mutter's then, to map the pen onto), and as the desktop may be
    # touched again (closed if another user's session was in front).
    prepared = []
    buffer = types.SimpleNamespace(get_size=lambda: 3, extract_dup=lambda at, size: b"abc",
                                   has_flags=lambda flags: False)
    sink = types.SimpleNamespace(emit=lambda signal: types.SimpleNamespace(get_buffer=lambda: buffer))
    streamer = streamer_on(FakeLink(budget=None))
    streamer.__dict__.update(_state_lock=threading.Lock(), shown=True, active=True, gated=False,
                             _captured=collections.deque(), _encoded=[], _frame=0, _keyframes=0,
                             _capture=lambda: None, on_visible=lambda shown: None,
                             input=types.SimpleNamespace(prepare=lambda: prepared.append(1), let_go=lambda: None))
    for _ in range(3):
        appdisplay.AppStreamer._send(streamer, sink)
    assert prepared == [1]
    appdisplay.AppStreamer.gate(streamer, True)
    appdisplay.AppStreamer.gate(streamer, False)
    assert prepared == [1, 1]


def test_a_pen_made_once_the_desktop_ended_is_closed():
    go, made, closed = threading.Event(), [], []
    device = types.SimpleNamespace(report=lambda report: None, close=lambda: closed.append(1))
    touch = appdisplay.Input(FakeMonitor(), (1000, 1000), lambda: made.append(1) or go.wait(5) and device)
    touch.prepare()
    touch.close(last=True)  # the desktop ended while it was made
    go.set()
    assert soon(lambda: closed == [1])
    touch.prepare()
    touch.packet(pen_near())  # none made again
    time.sleep(0.05)
    assert made == [1] and touch.pen is None


def test_sizes_keep_the_tablet_s_proportions():
    assert appdisplay.sizes((2880, 1800))[:4] == [(2880, 1800), (2560, 1600), (2304, 1440), (1920, 1200)]
    assert appdisplay.sizes((1800, 2880))[0] == (2880, 1800)  # portrait reported: landscape
    for w, h in appdisplay.sizes((2800, 1752)):
        assert h % 2 == 0 and abs(w / h - 2800 / 1752) < 0.01
    assert appdisplay.auto_size((2880, 1800)) == (1920, 1200)
    assert appdisplay.auto_size((1920, 1080)) == (1920, 1080)


def test_big_sizes_are_hidpi_with_the_bitrate_to_match():
    assert appdisplay.scale_for((2880, 1800)) == 2 and appdisplay.scale_for((1920, 1200)) == 1
    assert appdisplay.bitrate_for(20000, (2880, 1800)) == 45000
    assert appdisplay.bitrate_for(20000, (1280, 800)) == 20000  # never less than asked


# A cut held a second before the bitrate goes down (PLUSONE_L36_HOLD_MS=1000,
# l36 as first made; it ships with none): the hold's own tests.
HELD = appdisplay.L36(hold_ms=1000)


def held(monkeypatch):
    monkeypatch.setenv("PLUSONE_L36_HOLD_MS", "1000")


def test_the_bitrate_follows_the_network():
    rate = appdisplay.RateControl(20000)
    for _ in range(5):  # clean: stays at the most
        assert rate.report(60, 0, 30, 20) is None
    assert rate.report(55, 3, 30, 20) == 13333  # frames lost: down by a third, at once
    assert rate.report(60, 0, 80, 20) == 8888  # the delay through the network grew: a queue
    assert rate.report(0, 40, 0, 0) == 5925  # nothing shown, all lost
    for _ in range(2):
        assert rate.report(60, 0, 30, 20) is None
    assert rate.report(60, 0, 30, 20) == 6617  # clean for a while: up by a tenth
    rate.kbps, rate.ceiling = 19950, None
    for _ in range(3):
        result = rate.report(60, 0, 30, 20)
    assert result == 20000 and rate.kbps == 20000  # never past the one set
    rate.kbps = appdisplay.MIN_KBPS
    assert rate.report(60, 9, 30, 20) is None  # nor below the least


def test_on_a_path_with_its_own_congestion_control_the_bitrate_is_its_budget():
    rate = appdisplay.RateControl(20000, tuning=HELD)
    assert rate.report(60, 0, 30, 20, budget=10000, now=0) is None  # over it: a second first (frames left out)
    assert rate.report(60, 0, 30, 20, budget=10000, now=1) == 8500  # a share of it
    assert rate.report(55, 3, 30, 20, budget=10000, now=2) is None  # a frame lost: no cut
    assert rate.report(60, 0, 30, 20, budget=10300, now=9) is None  # a few percent: no change
    assert rate.report(60, 0, 30, 20, budget=40000, now=9) == 20000  # never past the one set
    assert rate.report(60, 0, 30, 20, budget=500, now=9) is None
    assert rate.report(60, 0, 30, 20, budget=500, now=10) == appdisplay.MIN_KBPS  # nor below the least


def test_each_change_is_a_keyframe_so_the_bitrate_changes_seldom():
    rate = appdisplay.RateControl(20000, tuning=HELD)
    rate.budget(10000, now=-1)
    assert rate.budget(10000, now=0) == 8500
    assert rate.budget(9000, now=0.1) is None  # a cut it still fits in: no keyframe for it
    assert rate.budget(8000, now=0.2) is None  # past it: a second first...
    assert rate.tick(now=1.2) == 6800  # ...then down, whenever the last change
    assert rate.budget(9000, now=4) is None  # up by less than UP: not worth a keyframe
    assert rate.budget(10000, now=4) == 8500  # by UP and more...
    assert rate.budget(14000, now=5) is None  # ...but UP_AFTER after the last change
    assert rate.budget(14000, now=6) == 11900
    rate.kbps, rate.changed_at = 18500, 0
    assert rate.budget(40000, now=9) == 20000  # the last few percent to the most
    rate.kbps = 9000
    assert rate.report(55, 3, 30, 20) == 6000  # no budget (a TCP path): a frame lost cuts, as before


class FakeLink:
    """LinkPackets as the streamer uses it: what waits, the budget, the
    others' share, the round trip, the realtime it dropped; what was sent."""

    def __init__(self, budget=10000, queued=0, others=0, rtt=None):
        self.budget, self.waiting, self.others, self.round_trip = budget, queued, others, rtt
        self.sent, self.drops = [], 0

    def queued(self):
        return self.waiting

    def dropped(self):
        return self.drops

    def budget_kbps(self):
        return self.budget

    def others_kbps(self):
        return self.others

    def rtt(self):
        return self.round_trip

    def send(self, body):
        self.sent.append(body)


class FakeEncoder:
    def __init__(self):
        self.props = {}

    def set_property(self, name, value):
        self.props[name] = value

    def find_property(self, name):
        return None


def streamer_on(link, bitrate=20000, fps=60):
    """A streamer of what decides its frames and bitrate, no pipeline."""
    streamer = object.__new__(appdisplay.AppStreamer)
    streamer._rates(link, fps, bitrate)
    streamer.enc, streamer.input = FakeEncoder(), None
    return streamer


class FakeWatchedLink:
    """plus-one-link's connection, as LinkPackets uses it: its events by hand."""

    def __init__(self):
        self.on_event, self.asked, self.closed = None, 0, False

    def watch(self, on_event):
        self.on_event = on_event
        ended = threading.Event()  # its connection's end: close()'s here
        return types.SimpleNamespace(close=lambda: (setattr(self, "closed", True), ended.set()),
                                     thread=types.SimpleNamespace(join=ended.wait))

    def devices(self):
        self.asked += 1
        return [{"id": "tab", "rate_kbps": 9000}]

    def close(self):
        pass


def test_the_links_budget_is_followed_as_it_changes():
    link = FakeWatchedLink()
    channel = types.SimpleNamespace(close=lambda: None)
    packets = appdisplay.LinkPackets(link, "tab", channel, channel, channel)
    assert packets.budget_kbps() == 9000 and link.asked == 1  # not told yet: asked
    told = []
    packets.watch_budget(told.append)
    link.on_event({"event": "budget", "device": "other", "kbps": 1000})
    link.on_event({"event": "up", "device": "tab"})
    assert told == []
    link.on_event({"event": "budget", "device": "tab", "kbps": 12000})
    assert told == [12000] and packets.budget_kbps() == 12000
    link.on_event({"event": "budget", "device": "tab", "kbps": None})  # on a path of no controller of its own
    assert told == [12000, None] and packets.budget_kbps() is None
    assert link.asked == 1  # told: never asked again
    packets.close()
    assert link.closed



def test_the_streamer_follows_the_budget_between_the_reports(monkeypatch):
    held(monkeypatch)
    clock = Clock()
    monkeypatch.setattr(appdisplay, "time", types.SimpleNamespace(monotonic=clock))
    link = FakeLink(budget=None)
    streamer = streamer_on(link)
    bitrates = []
    streamer._bitrate = lambda kbps, why: bitrates.append(kbps)
    streamer._budget(10000)  # over it: frames left out, a second first
    streamer._budget(10300)  # a few percent: no change
    assert streamer.allowance.kbps == 10300 * 0.95
    streamer._budget(None)  # a path of no controller of its own: no hold, no limit
    assert streamer.allowance.kbps is None
    clock.now += 2
    streamer._tick()
    link.others = 1700  # the speakers play: the screen leaves them theirs
    streamer._budget(10000)
    clock.now += 1
    streamer._tick()
    streamer._running = False
    streamer._budget(4000)
    assert bitrates == [6800]


def test_every_change_of_the_bitrate_is_said_with_its_cause(monkeypatch):
    # The live tests count them: those from the budget's events wrote
    # nothing (13 said of 51 in 10 minutes), and each is a keyframe.
    held(monkeypatch)
    said = []
    monkeypatch.setattr(appdisplay, "log", lambda *args: said.append(" ".join(map(str, args))))
    clock = Clock()
    monkeypatch.setattr(appdisplay, "time", types.SimpleNamespace(monotonic=clock))
    streamer = streamer_on(FakeLink(budget=None, others=1700))
    streamer._budget(10000)
    clock.now += 0.5
    streamer._budget(9800)  # no change: nothing said
    clock.now += 0.5
    streamer._tick()
    assert said == ["app display: 20000 -> 6630 kbit/s, as the network takes "
                    "(the budget 9800 kbit/s, the others' 1700; under it for 1.0 s)"]
    rate = appdisplay.RateControl(20000)
    rate.report(55, 3, 30, 20)
    assert rate.why == "3 frames lost"
    rate.report(40, 0, 30, 20, skipped=15)
    assert rate.why == "15 not encoded"
    # Each second a keyframe went is said, with how many.
    display = types.SimpleNamespace(_reports=1)
    appdisplay.AppDisplay._stats(display, {"shown": 60, "lost": 0, "capture_to_decoded": [20, 30, 40],
                                           "whole_to_decoded": [10, 12, 14], "capture_to_sent": 5, "skipped": 0,
                                           "kbps": 6800, "keyframes": 2})
    assert said[-1].endswith("; 6800 kbit/s; 2 keyframes")


def test_on_quic_a_frame_is_left_out_before_the_link_would_drop_it():
    # A 10 Mbit/s budget: QUEUE_MS of it is 75 KB; the link drops realtime
    # past 100 ms of it (125 KB). Of the bitrate (20 Mbit/s), it was 225 KB.
    link = FakeLink(budget=10000, queued=90 << 10)
    streamer = streamer_on(link)
    streamer.rate.kbps = streamer.kbps = 20000  # (it started at the budget's share: the budget since cut)
    assert streamer._frame_in(None, None) == appdisplay.Gst.PadProbeReturn.DROP
    assert streamer.skipped == {"queue": 1}
    streamer._budget(None)  # no budget (TCP): the bitrate's, the kernel's bytes as before
    link.waiting = 200 << 10
    streamer._captured = []
    pad = types.SimpleNamespace(get_sticky_event=lambda *_: None)
    info = types.SimpleNamespace(get_buffer=lambda: types.SimpleNamespace(pts=0))
    assert streamer._frame_in(pad, info) == appdisplay.Gst.PadProbeReturn.OK


def test_a_single_cut_is_absorbed_without_a_change_of_bitrate():
    # GCC cut by 30 % six times a minute on a good Wi-Fi, most taken back
    # within the second; each bitrate change was a keyframe at the worst time.
    rate = appdisplay.RateControl(20000, tuning=HELD)
    rate.kbps = 8500  # fits in 10000
    assert rate.budget(7000, now=10) is None  # cut: it no longer fits...
    assert rate.tick(now=10.5) is None
    assert rate.budget(10000, now=10.8) is None  # ...taken back within the second
    assert rate.tick(now=12) is None and rate.kbps == 8500
    assert rate.budget(7000, now=20) is None  # a cut that holds...
    assert rate.budget(7200, now=20.5) is None  # (still under: the second goes on)
    assert rate.tick(now=20.9) is None
    assert rate.tick(now=21.0) == 6120  # ...a second: down to the share of the budget now
    assert rate.tick(now=22) is None  # once


def offer_frames(streamer, clock, seconds, fps=60, keyframe_every=None):
    """Frames into the encoder each 1/fps for seconds, those let in sent at
    the bitrate's average size (a keyframe VBV_FRAMES of them): (frames
    sent, bytes sent)."""
    frames = sent = 0
    for i in range(int(seconds * fps)):
        clock.now += 1 / fps
        if streamer._left_out(clock.now):
            continue
        keyframe = keyframe_every is not None and frames % keyframe_every == 0
        size = streamer.kbps * 125 // fps * (appdisplay.media.VBV_FRAMES if keyframe else 1)
        packets = -(-size // appdisplay.PAYLOAD)
        streamer.allowance.take(size + packets * appdisplay.PACKET_EXTRA, clock.now)
        frames, sent = frames + 1, sent + size + packets * appdisplay.PACKET_EXTRA
    return frames, sent


def test_under_a_cut_whole_frames_are_left_out_within_the_budget(monkeypatch):
    held(monkeypatch)
    clock = Clock()
    monkeypatch.setattr(appdisplay, "time", types.SimpleNamespace(monotonic=clock))
    link = FakeLink(budget=10000)
    streamer = streamer_on(link, bitrate=8500)
    streamer.rate.kbps = 8500
    # Fitting the budget: every frame goes, a keyframe each second too.
    assert offer_frames(streamer, clock, 2, keyframe_every=60)[0] == 120
    # A cut to 7000: the bitrate stays for the second, the frames do not.
    streamer._budget(7000)
    frames, sent = offer_frames(streamer, clock, 1)
    allowed = 7000 * appdisplay.RateControl.FITS * 125
    assert sent <= allowed + streamer.allowance._depth(7000 * 0.95)  # never past the budget
    assert 40 <= frames <= 50 and streamer.kbps == 8500  # 48 of 60
    # Said each second, by why.
    assert appdisplay.describe({"shown": 48, "lost": 0, "capture_to_decoded": [20, 30, 40],
                                "whole_to_decoded": [10, 12, 14], "capture_to_sent": 5, "skipped": 13,
                                "left_out": {"budget": 12, "queue": 1}, "kbps": 8500}).endswith(
        "; 13 not encoded (12 over the budget, 1 the link's queue); 8500 kbit/s")


def test_l36_s_switch_and_knobs_are_read_from_the_environment():
    shipped = appdisplay.L36.from_env({})
    assert shipped.on and shipped.wrong == [] and shipped.describe().startswith("l36 on, hold ")
    off = appdisplay.L36.from_env({"PLUSONE_L36": "0", "PLUSONE_L36_HOLD_MS": "300", "PLUSONE_L36_FITS": "90"})
    assert not off.on and off.hold == 0 and off.share == appdisplay.RateControl.FITS  # the knobs, unused
    assert off.describe() == "l36 off (PLUSONE_L36=0): the bitrate down at once, every frame sent"
    tuned = appdisplay.L36.from_env({"PLUSONE_L36": "1", "PLUSONE_L36_HOLD_MS": "300", "PLUSONE_L36_PAY": "6",
                                     "PLUSONE_L36_DEPTH": "9", "PLUSONE_L36_FITS": "most"})
    assert (tuned.hold, tuned.pay, tuned.depth, tuned.share) == (0.3, 6, 5, 0.95)
    assert tuned.describe() == ("l36 on, hold 300 ms, pay 6 frames, allowance 5 frames at 95 % of the budget; "
                                "PLUSONE_L36_DEPTH=9 (kept within 1-5); PLUSONE_L36_FITS=most (not a whole number)")
    rate = appdisplay.RateControl(20000, 60, appdisplay.L36(fits=90))
    assert rate.allowed(10000, 1000) == 8100


def test_with_l36_off_the_desktop_runs_as_before_it(monkeypatch):
    # PLUSONE_L36=0, a live A/B without rebuilding: the bitrate follows the
    # budget at once (on Intel's, its keyframe at once too), no allowance,
    # every frame encoded sent, keyframes made as the app asks.
    monkeypatch.setenv("PLUSONE_L36", "0")
    streamer, link, clock, made, said = keyframe_streamer(monkeypatch)
    assert streamer.allowance.kbps is None  # no allowance
    streamer._budget(7000)
    assert streamer.enc.props["bitrate"] == streamer.kbps == 5950 and streamer.allowance.kbps is None
    assert said[-1].startswith("app display: 8500 -> 5950 kbit/s, as the network takes (the budget 7000")
    streamer._out(bytes(40000), True, 0)
    link.waiting = 200 << 10  # the keyframe still waits, past what the link holds of 7 Mbit/s
    clock.now += 0.3
    streamer.force_keyframe()
    assert made == [clock.now] and streamer._asks_held == 0  # another, at once
    streamer._out(bytes(15000), False, 0)
    packets = -(-40000 // appdisplay.PAYLOAD) - (-15000 // appdisplay.PAYLOAD)
    assert streamer.dropped == {} and len(link.sent) == packets  # sent whole all the same
    # The same, l36 on: held, the change and the frame both.
    monkeypatch.setenv("PLUSONE_L36", "1")
    streamer, link, clock, made, said = keyframe_streamer(monkeypatch)
    streamer._budget(7000)
    assert "bitrate" not in streamer.enc.props and streamer.allowance.kbps == 7000 * 0.95
    streamer._out(bytes(40000), True, 0)
    link.waiting = 200 << 10
    clock.now += 0.3
    assert not frame_in(streamer, clock) and made == []  # left out: the last keyframe still waits
    streamer._out(bytes(15000), False, 0)
    assert streamer.dropped == {"late": 1}


def test_the_hold_is_a_knob_counted_to_the_frame(monkeypatch):
    monkeypatch.setenv("PLUSONE_L36_HOLD_MS", "300")
    clock = Clock()
    monkeypatch.setattr(appdisplay, "time", types.SimpleNamespace(monotonic=clock))
    monkeypatch.setattr(appdisplay, "log", lambda *args: None)
    streamer = streamer_on(FakeLink(budget=10000), bitrate=8500)
    streamer._budget(7000)  # 8500 no longer fits in 6650
    offer_frames(streamer, clock, 0.28)
    assert streamer.kbps == 8500  # held...
    offer_frames(streamer, clock, 0.05)
    assert streamer.kbps == 5950  # ...300 ms: down with that frame, not at the listener's next wake (250 ms)


def test_as_it_ships_a_cut_lowers_the_bitrate_at_once_its_keyframe_once_it_fits(monkeypatch):
    # The grid's chain (adsl, lossy, spikes, 30 runs each): every 250 ms of
    # hold left out more frames than the keyframes it saved cost; none, the
    # frames shown as before l36 within 2 points, a fifth fewer keyframes.
    streamer, link, clock, made, said = keyframe_streamer(monkeypatch)
    assert streamer.rate.hold == 0
    streamer._out(bytes(60000), True, 0)  # a keyframe...
    link.waiting = 61000  # ...still on its way
    streamer._budget(7000)  # 8500 no longer fits in 6650: down at once, its keyframe (Intel's) once that one is out
    assert streamer.rate.kbps == 5950 and streamer.kbps == 8500 and said == []
    assert frame_in(streamer, clock)  # meanwhile frames go as they are, within the allowance
    link.waiting = 0
    assert frame_in(streamer, clock) and streamer.kbps == 5950
    assert said == ["app display: 8500 -> 5950 kbit/s, as the network takes (the budget 7000 kbit/s, the others' 0)"]


def test_no_hold_where_a_change_of_bitrate_is_no_keyframe(monkeypatch):
    # NVENC, x264: the bitrate changed in place; held, a cut only left out
    # frames for a keyframe that never comes.
    held(monkeypatch)
    clock = Clock()
    monkeypatch.setattr(appdisplay, "time", types.SimpleNamespace(monotonic=clock))
    monkeypatch.setattr(appdisplay, "log", lambda *args: None)
    streamer = streamer_on(FakeLink(budget=10000), bitrate=20000)
    streamer._encoder_restarts(False)
    streamer._budget(7000)
    assert streamer.kbps == 5950  # at once
    assert streamer.l36.describe(restarts=False).startswith("l36 on, no hold (a change of bitrate is no keyframe")
    streamer = streamer_on(FakeLink(budget=10000), bitrate=20000)
    streamer._encoder_restarts(True)  # Intel's
    streamer._budget(7000)
    assert streamer.kbps == streamer.rate.kbps == 8500 and streamer.rate.hold == 1.0


def test_a_stream_starts_at_the_budget_s_share(monkeypatch):
    # The first frame is a keyframe anyway: no change to hold back. From the
    # most (the preset's 20 Mbit/s on a 10 Mbit/s budget), the first second
    # left out half its frames, then a keyframe to go down.
    clock = Clock()
    monkeypatch.setattr(appdisplay, "time", types.SimpleNamespace(monotonic=clock))
    streamer = streamer_on(FakeLink(budget=10000, others=1000), bitrate=20000)
    assert streamer.kbps == streamer.rate.kbps == 7500  # 0.85 of the budget, less the speakers'
    assert offer_frames(streamer, clock, 1, keyframe_every=60)[0] == 60
    assert streamer_on(FakeLink(budget=None), bitrate=20000).kbps == 20000  # no budget: the most, as ever


def test_a_deep_cut_goes_down_once_the_frames_left_out_pay_for_its_keyframe():
    # PLUSONE_L36_PAY: the hold costs the frames left out, the change a
    # keyframe; past PAY frames' worth, the change is the cheaper.
    rate = appdisplay.RateControl(20000, 60, appdisplay.L36(hold_ms=1000, pay=4))
    rate.kbps = 8500
    assert rate.budget(5000, now=10) is None  # halved: 8500 over 4750 by 3750 kbit/s, 7.8 KB a frame
    assert rate.tick(now=10.12) is None  # 56 KB over: under 4 frames of 8500 (71 KB)
    assert rate.tick(now=10.16) == 4250  # 75 KB: down, 160 ms on
    rate.kbps = 8500
    assert rate.budget(8800, now=20) is None  # a shallow cut: 140 kbit/s over, a frame's worth in 2 s
    assert rate.tick(now=20.9) is None
    assert rate.tick(now=21) == 7480  # the hold's 1 s all the same
    no_pay = appdisplay.RateControl(20000, 60, appdisplay.L36(hold_ms=1000))
    no_pay.kbps = 8500
    no_pay.budget(5000, now=10)
    assert no_pay.tick(now=10.9) is None


def test_frames_not_encoded_for_the_queue_here_lower_the_bitrate():
    rate = appdisplay.RateControl(20000)
    rate.kbps = 10000
    for _ in range(2):
        assert rate.report(60, 0, 30, 20) is None
    assert rate.report(60, 0, 30, 20, skipped=2) is None  # after a keyframe, say: not clean...
    assert rate.report(60, 0, 30, 20) == 11100  # ...nor a reason to count the clean ones again
    # Nothing lost yet, the delay as low as ever: the queue only shows here.
    assert rate.report(45, 0, 30, 20, skipped=14) == 7400


def test_the_bitrate_goes_back_slowly_where_the_network_gave_up():
    rate = appdisplay.RateControl(20000)
    assert rate.report(40, 0, 30, 20, skipped=15) == 13333  # gave up at 20000
    ups = [rate.report(60, 0, 30, 20) for _ in range(9)]
    assert [k for k in ups if k] == [14766, 16342, 17000]  # quickly, up to 85 % of it
    ups = [rate.report(60, 0, 30, 20) for _ in range(30)]
    assert [k for k in ups if k] == [17510, 18035, 18576]  # past that, 3 % every 10 s
    rate.kbps = 19500
    for _ in range(10):
        result = rate.report(60, 0, 30, 20)
    assert result == 20000 and rate.ceiling is None  # past it, no trouble: forgotten


def test_a_frame_lost_now_and_then_is_no_ceiling():
    """Any Wi-Fi loses a frame now and then: down, but no ceiling, so back
    up quickly (a ceiling for each drove the bitrate to the least)."""
    rate = appdisplay.RateControl(20000)
    kbps = []
    for second in range(300):
        rate.report(60, 1 if second % 30 == 29 else 0, 30, 20)
        kbps.append(rate.kbps)
    assert rate.ceiling is None and sum(kbps[-120:]) / 120 > 15000


def test_one_queue_over_two_reports_is_one_ceiling():
    rate = appdisplay.RateControl(20000)
    rate.report(40, 0, 30, 20, skipped=15)
    rate.report(40, 0, 30, 20, skipped=6)  # the same queue, a second later: lower still...
    assert rate.kbps == 8888 and rate.ceiling == 20000  # ...but where it showed stays


def test_a_keyframe_s_skipped_frame_does_not_keep_the_bitrate_down():
    rate = appdisplay.RateControl(20000)
    rate.report(40, 0, 30, 20, skipped=15)
    for second in range(120):  # a frame skipped with each keyframe, every 10 s
        rate.report(60, 0, 30, 20, skipped=1 if second % 10 == 9 else 0)
    assert rate.kbps == 20000 and rate.ceiling is None  # the ceiling went old, and it went back


def test_a_still_desktop_s_few_frames_do_not_move_the_bitrate():
    rate = appdisplay.RateControl(20000)
    for _ in range(4):
        rate.report(30, 0, 30, 20)
    assert rate.report(1, 0, 90, 20) is None  # one frame, late: Wi-Fi's jitter, not a queue
    assert rate.report(30, 0, 90, 20) == 13333  # a second of them late: one


def test_what_is_still_to_go_out_is_known():
    app = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    app.bind(("127.0.0.1", 0))
    channel = channel_to(app)
    try:
        assert channel.queued() == 0
        channel.send(b"\x05")
        assert channel.queued() >= 0
    finally:
        channel.close()
        app.close()
    assert channel.queued() == 0  # closed: nothing


class FakeKeyframes:
    """media.Keyframes: when each was made (the frame going in made one)."""

    def __init__(self, clock):
        self.clock, self.made, self.coming = clock, [], False

    def now(self, pad):
        self.made.append(self.clock.now)
        self.coming = True

    def request(self):  # made at once, wherever from (before: an ask answered at once)
        self.now(None)


def test_keyframes_asked_again_wait_for_the_last_to_come(monkeypatch):
    clock = Clock()
    monkeypatch.setattr(appdisplay, "time", types.SimpleNamespace(monotonic=clock, monotonic_ns=lambda: 0))
    streamer = streamer_on(FakeLink(rtt=0.2))
    streamer.keyframes = FakeKeyframes(clock)
    for t in (100.0, 100.3, 100.4):  # a round trip and a half, and the keyframe's own 4/60 s: 0.367 s
        clock.now = t
        streamer.force_keyframe()
        streamer._frame_in(PAD, INFO)  # made with the next frame in
    assert streamer.keyframes.made == [100.0, 100.4]
    link = streamer.channel
    streamer.channel = types.SimpleNamespace(queued=link.queued)  # the link of before: no round trip known
    for t in (101.0, 101.2, 101.3):
        clock.now = t
        streamer.force_keyframe()
        streamer._frame_in(PAD, INFO)
    assert streamer.keyframes.made[2:] == [101.0, 101.3]


def keyframe_streamer(monkeypatch, restarts=True):
    """A streamer on a 10 Mbit/s budget, its encoder's every change of
    bitrate a keyframe (Intel's), the keyframes asked of it and what it said."""
    clock, asked, said = Clock(), [], []
    monkeypatch.setattr(appdisplay, "time", types.SimpleNamespace(monotonic=clock,
                                                                   monotonic_ns=lambda: int(clock.now * 1e9)))
    monkeypatch.setattr(appdisplay, "log", lambda *args: said.append(" ".join(map(str, args))))
    link = FakeLink(budget=10000)
    streamer = streamer_on(link, bitrate=8500)
    streamer.rate.kbps, streamer._restarts = 8500, restarts
    streamer.keyframes = FakeKeyframes(clock)
    return streamer, link, clock, streamer.keyframes.made, said


PAD = types.SimpleNamespace(get_sticky_event=lambda *_: None)
INFO = types.SimpleNamespace(get_buffer=lambda: types.SimpleNamespace(pts=0))


def frame_in(streamer, clock, size=15000):
    """The next frame (1/60 s on) into the encoder: whether it went in; in,
    out to the link at once, of size (a keyframe if one was made with it:
    VBV_FRAMES times that)."""
    clock.now += 1 / 60
    if streamer._frame_in(PAD, INFO) != appdisplay.Gst.PadProbeReturn.OK:
        return False
    keyframe, streamer.keyframes.coming = streamer.keyframes.coming, False
    streamer._out(bytes(size * (appdisplay.media.VBV_FRAMES if keyframe else 1)), keyframe, 0)
    return True


def test_no_keyframe_asked_for_while_the_last_still_waits_to_go_out(monkeypatch):
    streamer, link, clock, asked, _said = keyframe_streamer(monkeypatch)
    streamer._out(bytes(40000), True, 0)  # a keyframe (a bitrate change's, say)
    clock.now += 0.3  # past the gap since it went (0.25 s: no round trip known)...
    link.waiting = 41000 + 15000  # ...still in the link's queue, and a frame after it
    assert frame_in(streamer, clock)
    streamer.force_keyframe()  # the app asks (it lost a frame before it)
    assert asked == [] and streamer._asks_held == 1  # not another into the queue the first waits in
    link.waiting = 15000  # out: what waits now went after it
    assert frame_in(streamer, clock)
    streamer.force_keyframe()
    assert frame_in(streamer, clock) and asked == [clock.now]  # with the next frame in


def test_a_keyframe_sent_answers_the_app_s_asks_for_a_while(monkeypatch):
    streamer, link, clock, asked, _said = keyframe_streamer(monkeypatch)
    streamer._out(bytes(60000), True, 0)  # a bitrate change's keyframe, out at once
    clock.now += 0.1
    assert frame_in(streamer, clock)
    streamer.force_keyframe()  # the app's ask crossed it on the way
    assert asked == []


def test_a_bitrate_change_waits_for_the_last_keyframe_to_go_out(monkeypatch):
    streamer, link, clock, _asked, said = keyframe_streamer(monkeypatch)
    streamer._out(bytes(60000), True, 0)  # a keyframe
    link.waiting = 61000
    streamer._bitrate(6800, "the budget")  # a change: on Intel's, another keyframe
    assert frame_in(streamer, clock) and frame_in(streamer, clock)  # frames go, as they are
    assert "bitrate" not in streamer.enc.props and said == []
    link.waiting = 20000  # the keyframe out: what waits went after it
    assert frame_in(streamer, clock)  # the change with this one
    assert streamer.enc.props["bitrate"] == 6800 and streamer.kbps == 6800
    assert said == ["app display: 8500 -> 6800 kbit/s, as the network takes (the budget)"]
    # NVENC, x264: a change is no keyframe, made at once.
    streamer, link, clock, _asked, said = keyframe_streamer(monkeypatch, restarts=False)
    streamer._out(bytes(60000), True, 0)
    link.waiting = 61000
    streamer._bitrate(6800, "the budget")
    assert streamer.enc.props["bitrate"] == 6800


APP = "abcd" * 4


class FakeChannel:
    def __init__(self, answer=b""):
        self.answer, self.closed = answer, False

    def send(self, data):
        pass

    def recv(self, timeout=None):
        if isinstance(self.answer, BaseException):
            raise self.answer
        return self.answer

    def close(self):
        self.closed = True


class FakeConnection:
    """plus-one-link with the paired tablet, whose app answers "display" with answer."""

    def __init__(self, answer):
        self.answer, self.opened, self.closed = answer, [], False

    def devices(self):
        return [{"id": APP + "0" * 48, "connected": True}]

    def open(self, device, feature, name, kind):
        self.opened.append(FakeChannel(self.answer if name == "control" else b""))
        return self.opened[-1]

    def close(self):
        self.closed = True


def over_the_link(monkeypatch, answer):
    import plusone_link
    connection = FakeConnection(answer)
    monkeypatch.setattr(appdisplay, "plusone_link", types.SimpleNamespace(
        Link=lambda: connection, LinkError=plusone_link.LinkError, CONTROL="control", REALTIME="realtime",
        INPUT="input", VIDEO="video", HEAD=plusone_link.HEAD, MAX_DATAGRAM=plusone_link.MAX_DATAGRAM))
    return appdisplay.AppDisplay([("10.0.0.2", 1)], APP, (None, None, 60), 20000, "x264"), connection


@pytest.mark.parametrize("answer", [b"not json", b"[]", BrokenPipeError()])
def test_a_garbled_answer_over_the_link_leaves_nothing_open(monkeypatch, answer):
    display, connection = over_the_link(monkeypatch, answer)
    with pytest.raises((ValueError, OSError)):
        display._over_the_link()
    assert [channel.closed for channel in connection.opened] == [True, True, True] and connection.closed


def test_the_app_refusing_over_the_link_says_why(monkeypatch):
    from plusone import engine, link
    display, _connection = over_the_link(monkeypatch, json.dumps({"type": "refused", "reason": "background"}).encode())
    packets, reply = display._over_the_link()
    with pytest.raises(link.Refused) as refused:
        display._run_over_the_link(packets, reply)
    assert engine.why_not(refused.value, "Tab") == ("On {name}, let Plus One display over other apps, or open it",
                                                    {"name": "Tab"})


def test_a_frame_that_would_not_fit_whole_is_dropped_whole_and_those_after_until_a_keyframe(monkeypatch):
    # A 10 Mbit/s budget: the link holds 125 KB of realtime (100 ms), 112.5 KB
    # of the budget told; past it, it drops packets one by one: a frame's tail.
    streamer, link, clock, made, said = keyframe_streamer(monkeypatch)
    streamer._out(bytes(40000), True, 0)
    sent = len(link.sent)
    link.waiting = 60000
    clock.now += 1 / 60
    assert streamer._frame_in(PAD, INFO) == appdisplay.Gst.PadProbeReturn.OK  # it fits as it goes in...
    streamer._budget(7000)  # ...the controller cuts by 30 %: the link holds 30 % less at once
    streamer._out(bytes(20000), False, 0)  # 60 KB waiting and 20 more: its tail would be cut
    assert len(link.sent) == sent and streamer.dropped == {"late": 1}  # none of it
    link.waiting = 0
    streamer._out(bytes(15000), False, 0)  # decoded against the one dropped: garbage
    assert len(link.sent) == sent and streamer.dropped == {"late": 1, "broken": 1}
    assert frame_in(streamer, clock, size=15000)  # a keyframe with the next frame in: it fits
    assert made == [clock.now] and not streamer._broken
    keyframe = appdisplay.VIDEO_HEADER.unpack_from(link.sent[sent])
    assert keyframe[1] == 1 and keyframe[2] == 2  # whole, and numbered on (the app sees nothing missing)
    assert frame_in(streamer, clock) and len(link.sent) > sent + keyframe[4]  # the frames after it go
    assert appdisplay.describe({"shown": 40, "lost": 0, "capture_to_decoded": [20, 30, 40],
                                "whole_to_decoded": [10, 12, 14], "capture_to_sent": 5, "kbps": 8500,
                                "dropped": {"late": 1, "broken": 1}}).endswith(
        "; 2 encoded not sent (1 after one dropped, 1 would not have fitted)")


def test_a_keyframe_is_made_only_once_the_whole_of_it_fits(monkeypatch):
    streamer, link, clock, made, _said = keyframe_streamer(monkeypatch)
    streamer.force_keyframe()  # the app asks
    link.waiting = 60000  # under QUEUE_MS: a frame would go; a keyframe (4 x 17.7 KB) would not fit
    assert not frame_in(streamer, clock)  # left out: no use to the app, which waits for a keyframe
    assert made == [] and streamer.skipped == {"keyframe": 1}
    link.waiting = 30000
    assert frame_in(streamer, clock) and made == [clock.now]


def test_on_a_low_budget_no_frame_goes_in_that_the_link_would_cut():
    # 3 Mbit/s: the link holds 37.5 KB of realtime; 64 KB waiting let frames in.
    link = FakeLink(budget=3000, queued=40000)
    streamer = streamer_on(link, bitrate=2550)
    streamer.rate.kbps = 2550
    assert streamer._left_out(100.0) == "queue"
    link.waiting = 20000
    assert streamer._left_out(100.1) is None


def frames_worth(streamer, frames):
    """Bytes of H.264 so many average frames at the bitrate weigh."""
    return int(frames * streamer.kbps * 125 / streamer.fps)


def key_frame_in(streamer, clock, key_frames):
    """As frame_in, a keyframe key_frames average frames at the bitrate it
    was made at."""
    clock.now += 1 / 60
    if streamer._frame_in(PAD, INFO) != appdisplay.Gst.PadProbeReturn.OK:
        return False
    keyframe, streamer.keyframes.coming = streamer.keyframes.coming, False
    streamer._out(bytes(frames_worth(streamer, key_frames if keyframe else 1)), keyframe, 0)
    return True


def test_a_keyframe_is_reckoned_at_the_size_of_the_last_ones_made(monkeypatch):
    # Live, a video starting in a browser: keyframes two or three times the
    # VBV_FRAMES reckoned, each made where only the reckoning fitted.
    streamer, link, clock, made, _said = keyframe_streamer(monkeypatch)
    streamer._budget(24000)  # the link holds 270 KB of realtime
    streamer._out(bytes(frames_worth(streamer, 12)), True, 0)  # three times the reckoning, 224 KB: it fits, it goes
    assert streamer.dropped == {} and streamer.kbps == 8500
    clock.now += 0.5
    streamer.force_keyframe()
    link.waiting = 50000  # 4 frames (75 KB) would fit behind it; 12 would not
    assert not frame_in(streamer, clock) and made == [] and streamer.skipped == {"keyframe": 1}
    link.waiting = 30000
    assert frame_in(streamer, clock) and made == [clock.now]


def test_a_keyframe_goes_only_if_it_fits_behind_what_waits_as_it_goes(monkeypatch):
    # What waited as the frame went in is not what waits as it comes out.
    streamer, link, clock, made, _said = keyframe_streamer(monkeypatch)
    streamer._queued = 0  # as it went in
    link.waiting = 60000  # now (the speakers', a burst): 60 KB and 70 more is past what the link holds (112.5)
    streamer._out(bytes(70000), True, 0)
    assert link.sent == [] and streamer.dropped == {"late": 1} and streamer._broken


def test_a_keyframe_that_could_never_fit_lowers_the_bitrate_first(monkeypatch):
    # Three times the reckoning, 212 KB at 8.5 Mbit/s, where the link holds
    # 125 KB (100 ms of 10 Mbit/s): sent once nothing waited, the link cut
    # its tail, every time.
    streamer, link, clock, made, said = keyframe_streamer(monkeypatch)
    streamer._out(bytes(frames_worth(streamer, 12)), True, 0)
    assert link.sent == [] and streamer.dropped == {"late": 1}  # not to be cut: not sent
    assert key_frame_in(streamer, clock, 12) and made == [clock.now]
    # Down to the bitrate whose keyframe fits (Intel's: the change is the
    # keyframe, with this frame), told to the rate controller (up again as ever).
    assert streamer.kbps == streamer.rate.kbps == streamer.enc.props["bitrate"] < 4500
    assert said[-1].startswith(f"app display: 8500 -> {streamer.kbps} kbit/s, as the network takes (its keyframe, 12.")
    assert said[-1].endswith("frames' worth, to fit in what the link holds)")
    assert link.sent and not streamer._broken  # 12 frames at that bitrate: whole, sent
    assert sum(len(p) - appdisplay.VIDEO_HEADER.size for p in link.sent) == frames_worth(streamer, 12)
    # A change up goes as far as its keyframe fits, no further.
    link.waiting = 0
    clock.now += appdisplay.RateControl.UP_AFTER
    streamer._budget(20000)
    assert key_frame_in(streamer, clock, 12)
    assert 8000 < streamer.kbps < 9000 and streamer.rate.kbps == streamer.kbps  # not 17000: the share


def test_a_keyframe_the_link_cut_and_asked_again_makes_the_next_smaller_and_alone(monkeypatch):
    streamer, link, clock, made, said = keyframe_streamer(monkeypatch)
    link.round_trip = 0.01
    streamer._out(bytes(60000), True, 0)  # fits, as far as the streamer can tell...
    link.drops += 20  # ...but the link cut its tail
    clock.now += 0.27  # the app asks again, as it does every 250 ms at most
    streamer.force_keyframe()
    assert streamer._key_wanted and streamer.rate.kbps == 4250  # lost: the next at half the bitrate
    assert frame_in(streamer, clock, size=8000) and made == [clock.now] and streamer.enc.props["bitrate"] == 4250
    assert said[-1] == ("app display: 8500 -> 4250 kbit/s, as the network takes "
                        "(a keyframe the link cut, asked for again)")
    link.waiting = 30000  # it still waits to go out: nothing after it
    assert not frame_in(streamer, clock) and streamer.skipped == {"alone": 1}
    link.waiting = 0
    assert frame_in(streamer, clock) and frame_in(streamer, clock)  # out: the frames after it go
    assert appdisplay.describe({"shown": 30, "lost": 0, "capture_to_decoded": [20, 30, 40],
                                "whole_to_decoded": [10, 12, 14], "capture_to_sent": 5, "kbps": 4250,
                                "keyframes": 2, "keys_lost": 1}).endswith(
        "; 4250 kbit/s; 2 keyframes; 1 cut by the link, the next smaller")


def test_an_ask_is_a_keyframe_lost_only_soon_after_and_with_the_link_s_drops(monkeypatch):
    # A frame lost on the air after a keyframe that came whole asks too: no
    # halving for it (on a lossy Wi-Fi, one each time).
    for drops, after in ((0, 0.27), (20, 0.6)):
        streamer, link, clock, made, _said = keyframe_streamer(monkeypatch)
        link.round_trip = 0.01
        streamer._out(bytes(60000), True, 0)
        link.drops += drops
        clock.now += after
        streamer.force_keyframe()
        assert streamer._key_wanted and streamer.rate.kbps == 8500 and streamer._alone_from is None


def test_no_keyframe_answers_faster_than_the_last_one_takes_to_go(monkeypatch):
    # No budget (TCP): its time at the bitrate. Three times the reckoning at
    # 20 Mbit/s is 0.2 s, and a round trip and a half of 100 ms.
    clock = Clock()
    monkeypatch.setattr(appdisplay, "time", types.SimpleNamespace(monotonic=clock, monotonic_ns=lambda: 0))
    streamer = streamer_on(FakeLink(budget=None, rtt=0.1))
    streamer.keyframes = FakeKeyframes(clock)
    streamer._out(bytes(frames_worth(streamer, 12)), True, 0)
    clock.now += 0.3
    streamer.force_keyframe()
    assert not streamer._key_wanted
    clock.now += 0.08
    streamer.force_keyframe()
    assert streamer._key_wanted


class CuttingLink(FakeLink):
    """A link draining at its budget that drops realtime past LATE of it,
    packet by packet, as quic.rs does; whether the frame going was whole."""

    def __init__(self, budget):
        super().__init__(budget=budget, rtt=0.01)
        self.whole = True

    def drain(self, seconds):
        self.waiting = max(0, self.waiting - self.budget * 125 * seconds)

    def send(self, body):
        size = len(body) + appdisplay.PACKET_EXTRA
        if self.waiting + size > self.budget * 125 * appdisplay.LATE:
            self.drops += 1
            self.whole = False
        else:
            self.waiting += size
            self.sent.append(body)


def live_like(monkeypatch, l36="1", seconds=6, bigger=3):
    """As live this morning: a picture that moves, the encoder's keyframes
    bigger times the reckoning (VBV_FRAMES), on a link that cuts what waits
    past LATE; the budget ramps up from 13.4 to 19 Mbit/s after 2.5 s (the
    controller out of a still picture). The app asks for a keyframe as
    DisplaySession.kt does: at a whole frame that is not one while it has
    no picture, 250 ms apart. (frames shown, keyframes whole, asks, seconds
    with none shown)."""
    monkeypatch.setenv("PLUSONE_L36", l36)
    clock = Clock()
    monkeypatch.setattr(appdisplay, "time", types.SimpleNamespace(monotonic=clock, monotonic_ns=lambda: 0))
    monkeypatch.setattr(appdisplay, "log", lambda *args: None)
    link = CuttingLink(13352)
    streamer = streamer_on(link)
    streamer._restarts = True
    streamer.keyframes = FakeKeyframes(clock)
    need, asked, asks, whole_keys, shown = True, -math.inf, 0, 0, [0] * seconds
    for i in range(seconds * 60):
        clock.now += 1 / 60
        link.drain(1 / 60)
        if i == 150:
            link.budget = 19009
            streamer._budget(19009)
        kbps = streamer.kbps
        if streamer._frame_in(PAD, INFO) != appdisplay.Gst.PadProbeReturn.OK:
            continue
        keyframe = streamer._frame == 0 or streamer.keyframes.coming or streamer.kbps != kbps
        streamer.keyframes.coming = False
        size = frames_worth(streamer, appdisplay.media.VBV_FRAMES * bigger if keyframe else 1)
        sent, link.whole = len(link.sent), True
        streamer._out(bytes(size), keyframe, 0)
        if len(link.sent) == sent and link.whole:
            continue  # not sent
        if not link.whole:
            need = True
        elif keyframe:
            need, whole_keys, shown[i // 60] = False, whole_keys + 1, shown[i // 60] + 1
        elif not need:
            shown[i // 60] += 1
        elif clock.now - asked >= 0.25:
            asked, asks = clock.now, asks + 1
            streamer.force_keyframe()
    return sum(shown), whole_keys, asks, shown.count(0)


@pytest.mark.parametrize("bigger", [2, 3])
def test_keyframes_bigger_than_reckoned_never_loop_through_the_link(monkeypatch, bigger):
    # Live: at 0 fps for 8 s, a keyframe asked every 270 ms, each answered,
    # each cut by the link (none came whole).
    shown, whole_keys, asks, frozen = live_like(monkeypatch, bigger=bigger)
    assert whole_keys >= 2 and frozen == 0 and shown > 200 and asks <= 3
    # As before l36: every keyframe cut, the app asking all along.
    old_shown, old_keys, old_asks, old_frozen = live_like(monkeypatch, "0", bigger=bigger)
    assert old_shown < shown and old_frozen > frozen


class SocketConnection:
    """plus-one-link with the paired tablet, its channels real ones
    (plusone_link.Channel) on socket pairs, the other ends in app by name
    (the control's with the app's answer in it); refusing: the classes it
    says no to, as one from before them does ("no class")."""

    def __init__(self, refusing=()):
        self.refusing, self.asked, self.app, self.closed = set(refusing), [], {}, False

    def devices(self):
        return [{"id": APP + "0" * 48, "connected": True}]

    def open(self, device, feature, name, kind):
        import plusone_link
        self.asked.append(kind)
        if kind in self.refusing:
            raise plusone_link.LinkError("no class")
        ours, theirs = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
        self.app[name] = theirs
        if name == "control":
            theirs.send(json.dumps({"type": "display-ready", "width": 2560, "height": 1600}).encode())
        return plusone_link.Channel(ours.detach(), device, feature, name, kind, len(self.asked))

    def close(self):
        self.closed = True
        for end in self.app.values():
            end.close()


def over_sockets(monkeypatch, refusing=()):
    """The display's channels as _over_the_link opens them, on sockets: (its
    packets, the connection), and a streamer on them (no pipeline)."""
    import plusone_link
    connection = SocketConnection(refusing)
    monkeypatch.setattr(plusone_link, "Link", lambda: connection)
    display = appdisplay.AppDisplay([("10.0.0.2", 1)], APP, (None, None, 60), 20000, "x264")
    packets, reply = display._over_the_link()
    assert reply["type"] == "display-ready"
    return packets, connection, streamer_on(packets)


def sent_on(end):
    """What came on a channel's other end so far."""
    got = []
    while True:
        try:
            got.append(end.recv(1 << 16, socket.MSG_DONTWAIT))
        except BlockingIOError:
            return got


PONG = struct.pack("!Bqq", appdisplay.PONG, 1, 2)


def test_over_the_link_each_frame_is_a_group_of_its_own(monkeypatch):
    # q3's option B: the media channel a VIDEO one, each frame's packets a
    # group (on QUIC, a stream of their own); every message with the link's
    # head before it, which the link takes off: the app reads them as before.
    import plusone_link
    from plusone_link import HEAD, IN_GROUP, KEY, LAST
    packets, connection, streamer = over_sockets(monkeypatch)
    assert connection.asked == ["control", "video", "input"] and packets.grouped
    payload = packets.payload
    assert payload == plusone_link.MAX_DATAGRAM - appdisplay.VIDEO_HEADER.size - HEAD.size
    streamer._out(bytes(2 * payload + 100), True, 111)  # a keyframe: three packets
    streamer._out(bytes(payload + 1), False, 222)        # a frame after it: two
    streamer._out(bytes(10), False, 333)                 # one
    packets.send(PONG)                                   # no frame's: of no group
    got = sent_on(connection.app["media"])
    assert [HEAD.unpack_from(m) for m in got] == [
        (IN_GROUP | KEY, 1), (IN_GROUP | KEY, 1), (IN_GROUP | KEY | LAST, 1),
        (IN_GROUP, 2), (IN_GROUP | LAST, 2),
        (IN_GROUP | LAST, 3),
        (0, 0)]
    # None past MAX_DATAGRAM, head and all; full ones exactly that.
    assert [len(m) for m in got[:3]] == [plusone_link.MAX_DATAGRAM] * 2 + [
        HEAD.size + appdisplay.VIDEO_HEADER.size + 100]
    assert max(len(m) for m in got) <= plusone_link.MAX_DATAGRAM
    # Behind the head, the packets as the app reads them.
    assert [appdisplay.VIDEO_HEADER.unpack_from(m, HEAD.size) for m in got[:6]] == [
        (appdisplay.VIDEO, 1, 1, 0, 3, 111), (appdisplay.VIDEO, 1, 1, 1, 3, 111), (appdisplay.VIDEO, 1, 1, 2, 3, 111),
        (appdisplay.VIDEO, 0, 2, 0, 2, 222), (appdisplay.VIDEO, 0, 2, 1, 2, 222),
        (appdisplay.VIDEO, 0, 3, 0, 1, 333)]
    assert got[6][HEAD.size:] == PONG
    # What the app sends comes as it was.
    connection.app["media"].send(PONG)
    assert packets.receive(1) == PONG
    packets.close()


def test_a_link_with_no_video_class_takes_the_frames_as_before(monkeypatch):
    # plus-one-link from before VIDEO refuses it: the media channel a
    # realtime one, its messages as they were (no head, the payload whole).
    import plusone_link
    packets, connection, streamer = over_sockets(monkeypatch, refusing={"video"})
    assert connection.asked == ["control", "video", "realtime", "input"] and not packets.grouped
    payload = packets.payload
    assert payload == plusone_link.MAX_DATAGRAM - appdisplay.VIDEO_HEADER.size
    streamer._out(bytes(2 * payload + 100), True, 111)
    packets.send(PONG)
    got = sent_on(connection.app["media"])
    assert [appdisplay.VIDEO_HEADER.unpack_from(m) for m in got[:3]] == [
        (appdisplay.VIDEO, 1, 1, 0, 3, 111), (appdisplay.VIDEO, 1, 1, 1, 3, 111), (appdisplay.VIDEO, 1, 1, 2, 3, 111)]
    assert [len(m) for m in got[:3]] == [plusone_link.MAX_DATAGRAM] * 2 + [appdisplay.VIDEO_HEADER.size + 100]
    assert got[3:] == [PONG]
    packets.close()


def test_video_streams_off_takes_the_frames_as_before(monkeypatch):
    # PLUSONE_LINK_VIDEO_STREAMS=0, the live A/B's old way: the media channel
    # asked as realtime at once, its messages as before VIDEO.
    import plusone_link
    monkeypatch.setenv("PLUSONE_LINK_VIDEO_STREAMS", "0")
    packets, connection, streamer = over_sockets(monkeypatch)
    assert connection.asked == ["control", "realtime", "input"] and not packets.grouped
    payload = packets.payload
    assert payload == plusone_link.MAX_DATAGRAM - appdisplay.VIDEO_HEADER.size
    streamer._out(bytes(payload + 1), True, 111)
    packets.send(PONG)
    got = sent_on(connection.app["media"])
    assert [appdisplay.VIDEO_HEADER.unpack_from(m) for m in got[:2]] == [
        (appdisplay.VIDEO, 1, 1, 0, 2, 111), (appdisplay.VIDEO, 1, 1, 1, 2, 111)]
    assert [len(m) for m in got[:2]] == [plusone_link.MAX_DATAGRAM, appdisplay.VIDEO_HEADER.size + 1]
    assert got[2:] == [PONG]
    packets.close()
