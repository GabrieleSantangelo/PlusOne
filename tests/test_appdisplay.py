"""The app display's packets: their layout, sealed both ways, replays refused."""
import collections
import json
import socket
import struct
import threading
import time
import types

import pytest

from plusone import appdisplay  # noqa: E402
from plusone.link.datagrams import FROM_APP, FROM_PC, Datagrams  # noqa: E402

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
    # Mutter's then, to map the pen onto), and as the desktop is shown or
    # may be touched again (the pen let go of with the rest meanwhile).
    prepared = []
    buffer = types.SimpleNamespace(get_size=lambda: 3, extract_dup=lambda at, size: b"abc",
                                   has_flags=lambda flags: False)
    sink = types.SimpleNamespace(emit=lambda signal: types.SimpleNamespace(get_buffer=lambda: buffer))
    streamer = types.SimpleNamespace(_captured=collections.deque(), channel=types.SimpleNamespace(send=lambda d: None),
                                     _frame=0, _encoded=[], _state_lock=threading.Lock(), shown=True, active=True,
                                     gated=False, _capture=lambda: None, force_keyframe=lambda: None,
                                     on_visible=lambda shown: None,
                                     input=types.SimpleNamespace(prepare=lambda: prepared.append(1),
                                                                 close=lambda: None))
    for _ in range(3):
        appdisplay.AppStreamer._send(streamer, sink)
    assert prepared == [1]
    appdisplay.AppStreamer.visible(streamer, False)
    appdisplay.AppStreamer.visible(streamer, True)
    assert prepared == [1, 1]
    appdisplay.AppStreamer.gate(streamer, True)
    appdisplay.AppStreamer.gate(streamer, False)
    assert prepared == [1, 1, 1]


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
    rate = appdisplay.RateControl(20000)
    assert rate.report(60, 0, 30, 20, budget=10000, now=0) == 8500  # a share of it
    assert rate.report(55, 3, 30, 20, budget=10000, now=1) is None  # a frame lost: no cut
    assert rate.report(60, 0, 30, 20, budget=10300, now=9) is None  # a few percent: no change
    assert rate.report(60, 0, 30, 20, budget=40000, now=9) == 20000  # never past the one set
    assert rate.report(60, 0, 30, 20, budget=500, now=9) == appdisplay.MIN_KBPS  # nor below the least


def test_each_change_is_a_keyframe_so_the_bitrate_changes_seldom():
    rate = appdisplay.RateControl(20000)
    assert rate.budget(10000, now=0) == 8500
    assert rate.budget(9000, now=0.1) is None  # a cut it still fits in: no keyframe for it
    assert rate.budget(8000, now=0.2) == 6800  # past it: down at once, whenever the last change
    assert rate.budget(9000, now=3) is None  # up by less than UP: not worth a keyframe
    assert rate.budget(10000, now=3) == 8500  # by UP and more...
    assert rate.budget(14000, now=4) is None  # ...but UP_AFTER after the last change
    assert rate.budget(14000, now=5) == 11900
    rate.kbps, rate.changed_at = 18500, 0
    assert rate.budget(40000, now=9) == 20000  # the last few percent to the most
    rate.kbps = 9000
    assert rate.report(55, 3, 30, 20) == 6000  # no budget (a TCP path): a frame lost cuts, as before


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
    # The streamer: the new budget at once, between the reports.
    bitrates = []
    others = [0]
    streamer = types.SimpleNamespace(rate=appdisplay.RateControl(20000), _rate_lock=threading.Lock(), _running=True,
                                     _bitrate=bitrates.append, channel=types.SimpleNamespace(others_kbps=lambda: others[0]))
    appdisplay.AppStreamer._budget(streamer, 10000)
    appdisplay.AppStreamer._budget(streamer, 10300)  # a few percent: no change
    appdisplay.AppStreamer._budget(streamer, None)
    others[0] = 1700  # the speakers play: the screen leaves them theirs
    appdisplay.AppStreamer._budget(streamer, 10000)
    streamer._running = False
    appdisplay.AppStreamer._budget(streamer, 4000)
    assert bitrates == [8500, 6800]


def test_on_quic_a_frame_is_left_out_before_the_link_would_drop_it():
    # A 10 Mbit/s budget: QUEUE_MS of it is 75 KB; the link drops realtime
    # past 100 ms of it (125 KB). Of the bitrate (20 Mbit/s), it was 225 KB.
    queued = [90 << 10]
    streamer = types.SimpleNamespace(channel=types.SimpleNamespace(queued=lambda: queued[0]), skipped=0,
                                     rate=appdisplay.RateControl(20000), _budget_kbps=10000)
    assert appdisplay.AppStreamer._frame_in(streamer, None, None) == appdisplay.Gst.PadProbeReturn.DROP
    assert streamer.skipped == 1
    streamer._budget_kbps = None  # no budget (TCP): the bitrate's, the kernel's bytes as before
    queued[0] = 200 << 10
    streamer._captured = []
    pad = types.SimpleNamespace(get_sticky_event=lambda *_: None)
    info = types.SimpleNamespace(get_buffer=lambda: types.SimpleNamespace(pts=0))
    assert appdisplay.AppStreamer._frame_in(streamer, pad, info) == appdisplay.Gst.PadProbeReturn.OK


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


def test_keyframes_asked_again_wait_for_the_last_to_come(monkeypatch):
    clock, asked = [100.0], []
    monkeypatch.setattr(appdisplay, "time", types.SimpleNamespace(monotonic=lambda: clock[0]))
    streamer = types.SimpleNamespace(channel=types.SimpleNamespace(rtt=lambda: 0.2), fps=60, _last_keyframe=0.0,
                                     keyframes=types.SimpleNamespace(request=lambda: asked.append(clock[0])))
    for t in (100.0, 100.3, 100.4):  # a round trip and a half, and the keyframe's own 4/60 s: 0.367 s
        clock[0] = t
        appdisplay.AppStreamer.force_keyframe(streamer)
    assert asked == [100.0, 100.4]
    streamer.channel = types.SimpleNamespace()  # the link of before: no round trip known
    for t in (101.0, 101.2, 101.3):
        clock[0] = t
        appdisplay.AppStreamer.force_keyframe(streamer)
    assert asked[2:] == [101.0, 101.3]


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
        INPUT="input", MAX_DATAGRAM=plusone_link.MAX_DATAGRAM))
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
