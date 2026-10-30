"""The desktop in Plus One's app on the tablet.

Each encoded frame (H.264, no MPEG-TS) is cut into UDP packets and sent the
moment the encoder has it, sealed with AES-GCM under a key handed to the app
over the paired TLS link for this session (DisplaySession.kt has the
packets' layout). The app decodes each frame as soon as it is whole and
shows it as soon as it is decoded, and says how long that took: frames
carry the time they were captured, and pings match the two clocks.
"""
import argparse
import collections
import json
import math
import os
import select
import struct
import threading
import time

import gi

gi.require_version("Gst", "1.0")
from gi.repository import GLib, Gst  # noqa: E402

from . import link, log, media, once, pen  # noqa: E402
from .link import channels  # noqa: E402
from .link.datagrams import NONCE, TAG, Datagrams  # noqa: E402

try:
    import plusone_link
except ImportError:  # a Plus One without the link (docs/LINK.md): the link of before only
    plusone_link = None

VIDEO, KEYFRAME, PING, PONG, STATS, INPUT = 1, 2, 3, 4, 5, 6
VIDEO_HEADER = struct.Struct("!BBIHHq")  # kind, flags, frame, index, count, captured
# H.264 per packet: with the header, nonce and tag, one Ethernet frame.
PAYLOAD = 1472 - VIDEO_HEADER.size - NONCE.size - TAG
MIN_KEYFRAME_GAP = 0.25  # seconds; the app asks again until one comes
# A keyframe counts as still on its way this long at most (s): realtime waits
# 100 ms at most in the link (quic.rs LATE); past it what waits is not its
# (a transfer's backlog, on the stream).
KEY_FLYING_MOST = 0.5
STATS_FORMAT = struct.Struct("!BII6H")
FINGER = struct.Struct("!BHH")  # id, x, y (0..65535 of the screen)
PEN_SAMPLE = 9                  # the S Pen's report after its ID (DisplayInput.kt)
TOUCH_MAX = 65535
MAX_FINGERS = 10
INPUT_SILENCE = 0.4  # seconds: the app repeats what touches every 0.1 s; quiet this long, all is lifted
IDLE_WAIT = 0.25     # seconds the listener sleeps while nothing touches (stop() waits as long)
STATS_EVERY = 30     # the app's reports (one a second) logged: one in so many, and each with a loss
# The fingers as on a touchpad (Input): a tap within TAP_TIME, having moved
# less than TAP_SLOP of the screen (two fingers: TAP_MOVE), clicks. One
# finger moved further drags, once DRAG_WAIT passed (a second may come:
# fingers never land together; at once if it moved DRAG_FAST); a second
# within PROMOTE lets the drag go. Held for HOLD: the right button. Two
# fingers decide once their middle moved, or their spread changed,
# GESTURE_START of the screen's width: a pinch if the spread changed more,
# else a scroll. Three swipe once moved SWIPE. A scroll flicked (faster than FLING_START at the
# lift, over the last VELOCITY_WINDOW) goes on, slowing down by e every
# FLING_TAU, until FLING_STOP.
TAP_TIME = 0.3   # seconds
TAP_SLOP = 0.012
TAP_MOVE = 0.02
DRAG_WAIT = 0.1  # seconds
DRAG_FAST = 0.05
PROMOTE = 0.25   # seconds
HOLD = 0.6       # seconds
GESTURE_START = 0.015
SWIPE = 0.1
VELOCITY_WINDOW = 0.1  # seconds
FLING_START = 300      # monitor pixels a second
FLING_TAU = 0.35       # seconds
FLING_STOP = 20        # monitor pixels a second
KEY_SUPER, KEY_ESCAPE = 0xffeb, 0xff1b
KEY_PAGE_UP, KEY_PAGE_DOWN = 0xff55, 0xff56
# The bitrate follows the network (RateControl): from the one set (its
# most) down to MIN_KBPS while the app reports frames lost or the delay
# growing, up again slowly while it does not. The least: a home Wi-Fi on
# 2.4 GHz far from its router took about 1 Mbit/s, under the 1500 of before
# (the stream stayed above what the network takes, frames lost every second).
MIN_KBPS = 800
QUEUE_MS = 60    # data waiting to go out past this many ms of the bitrate: the next frames are not encoded
# The kernel counts a packet waiting to go out with its buffers (TIOCOUTQ:
# about 2.3 KB for 1472 bytes), not its bytes alone.
TRUESIZE = 3 / 2
# A packet's bytes on the way past its H.264: our header, the link's seal,
# quinn's own count of a datagram (or UDP's and IP's).
PACKET_EXTRA = 80
# What the link holds of realtime before it drops it, late: LATE of its
# controller's rate, never more than quinn's room for datagrams (quic.rs
# LATE, DATAGRAM_SEND_BUFFER); of the budget as told, TOLD (told past 10 %
# of a move: the rate may be that much under it).
LATE = 0.1
LINK_ROOM = 512 << 10
TOLD = 0.9
# A frame bigger than all that goes once no more than this waits before it
# (on a budget too low for its bitrate it would never go); a keyframe so
# only at MIN_KBPS (above it, the bitrate down until its keyframe fits).
ALONE = 8 << 10
# A keyframe is reckoned at the most of the last KEY_SIZES the encoder made,
# each in average frames at its bitrate (bytes on the wire), never under
# VBV_FRAMES' worth (KEY_WORTH): the encoder's are not VBV_FRAMES as asked
# (live, a video starting in a browser: two or three times that), and one
# made on that reckoning had its tail cut by the link, the next as well.
KEY_SIZES = 3
KEY_WORTH = media.VBV_FRAMES * (1 + PACKET_EXTRA / PAYLOAD)
# A keyframe the link dropped realtime after (its count up since it went)
# that the app asks for again soon after (within the app's own gap between
# asks, its time on the way out and two round trips): lost. The next is
# made at KEY_BACKOFF of the bitrate, alone in the link (the frames after it
# left out until it is out).
KEY_BACKOFF = 0.5
# A frame not encoded, by why (the stats' line says how many of each).
LEFT_OUT = {"queue": "the link's queue", "budget": "over the budget", "keyframe": "for a keyframe to fit",
            "alone": "behind a keyframe going alone"}
# An encoded frame not sent, by why.
DROPPED = {"late": "would not have fitted", "broken": "after one dropped"}


# Widths offered for the desktop in the app, each at the tablet's proportions
# where that comes out in whole, even pixels; the tablet's own size first.
WIDTHS = (3840, 2560, 2304, 1920, 1680, 1440, 1280)
REFERENCE = 1920 * 1200  # the size the bitrate presets are for
HIDPI_WIDTH = 2560       # from this wide, the monitor at scale 2 (text as at half the size, sharp)


def sizes(screen):
    """The sizes the desktop can have on a tablet of this screen (w, h),
    largest first, all at its proportions (to within 0.5%: even heights)."""
    width, height = max(screen), min(screen)
    found = [(width, height)]
    for w in WIDTHS:
        h = round(w * height / width / 2) * 2
        if w < width and abs(w / h - width / height) < 0.005 * width / height:
            found.append((w, h))
    return found


def auto_size(screen):
    """The size closest to 1920x1200 in pixels, at the tablet's proportions."""
    return min(sizes(screen), key=lambda size: abs(size[0] * size[1] - REFERENCE))


def scale_for(size):
    return 2 if size[0] >= HIDPI_WIDTH else 1


def bitrate_for(bitrate, size):
    """The presets' bitrate (kbit/s, for 1920x1200) for this many pixels."""
    return int(bitrate * max(1.0, size[0] * size[1] / REFERENCE))


class Gesture:
    """What the fingers do, from the first one down to the last one up."""

    def __init__(self, since):
        self.since = since
        self.kind = "pending"  # then drag, scroll, pinch, swipe or done
        self.start = {}        # finger -> where it came down
        self.most = 0          # fingers at once, at most
        self.moved = 0.0       # the farthest a finger went from where it came down, of the screen
        self.fingers = {}      # as they are now
        self.pair = None       # the first two fingers' distance, together
        self.last = None       # their middle, as last scrolled from
        self.samples = collections.deque()  # (time, dx, dy) of the scroll, for its speed
        self.touching = {}     # a pinch's fingers, as the desktop has them

    def update(self, fingers, size):
        self.fingers = fingers
        for finger, where in fingers.items():
            self.start.setdefault(finger, where)
        self.most = max(self.most, len(fingers))
        for finger, (x, y) in fingers.items():
            x0, y0 = self.start[finger]
            self.moved = max(self.moved, abs(x - x0) / size[0] + abs(y - y0) / size[1])
        if len(fingers) == 2 and self.pair is None:
            self.pair = _spread(fingers), _middle(fingers)


def _middle(fingers):
    points = list(fingers.values())
    return sum(p[0] for p in points) / len(points), sum(p[1] for p in points) / len(points)


def _spread(fingers):
    (x1, y1), (x2, y2) = list(fingers.values())[:2]
    return max(1.0, math.hypot(x2 - x1, y2 - y1))


class Input:
    """Fingers and the pen from the app, onto the virtual monitor. Each
    packet says what touches the screen now (DisplayInput.kt); the fingers
    work as on a laptop's touchpad, where they touch:
    - one tapped: the left button; moved: a drag with it (text selected, a
      window moved); held still: the right button;
    - two tapped: the right button; moved together: scrolling, smooth, that
      goes on after they lift (faster flicked, farther), until a finger
      stops it; moved apart or together: a pinch, as touches (zoom);
    - three swiped: up the overview, down out of it, sideways another
      workspace."""

    def __init__(self, monitor, size, make_pen, clock=time.monotonic):
        self.monitor, self.size, self.make_pen, self.clock = monitor, size, make_pen, clock
        self.scale = scale_for(size)  # the monitor's pixels per desktop pixel
        self.pen = None       # the pen's device once made (prepare()); False: none to be had
        self.pen_last = None  # the last report, while the pen is in range
        # The pen made on a thread of its own, closed on the listening one:
        # one on its way (its turn: close() makes it an older one's, closed
        # as it comes), and none made again after close(last=True).
        self._pen_lock = threading.Lock()
        self._making, self._turn, self._last = False, 0, False
        self.heard = clock()
        self.gesture = None
        self.fling = None     # [vx, vy, at]: a scroll going on by itself, monitor pixels a second

    def packet(self, body):
        """body: an INPUT packet; False if it is not a well-formed one."""
        try:
            count, at = body[1], 2
            if count > MAX_FINGERS:
                return False
            fingers = {}
            for _ in range(count):
                finger, x, y = FINGER.unpack_from(body, at)
                fingers[finger] = (x / TOUCH_MAX * self.size[0], y / TOUCH_MAX * self.size[1])
                at += FINGER.size
            samples = body[at]
            at += 1
            if len(body) != at + samples * PEN_SAMPLE:
                return False
        except (IndexError, struct.error):
            return False
        self.heard = self.clock()
        self._touch(fingers)
        for i in range(samples):
            self._pen(bytes([pen.REPORT_ID]) + body[at + i * PEN_SAMPLE:at + (i + 1) * PEN_SAMPLE])
        return True

    def busy(self):
        """Whether check() has something to look after: fingers, a scroll
        going on by itself, the pen near."""
        return self.gesture is not None or self.fling is not None or self.pen_last is not None

    def check(self):
        """Every few ms while busy(): what time decides (a finger held, a
        scroll slowing down); and nothing heard for a while, whatever
        touches is lifted (the app's last packets were lost, or it is gone)."""
        now = self.clock()
        if self.fling is not None:
            self._fling(now)
        if self.gesture is not None:
            if now - self.heard >= INPUT_SILENCE:
                self._touch({}, lost=True)
            else:
                self._step(self.gesture, now)
        if self.pen_last is not None and now - self.heard >= INPUT_SILENCE:
            self._pen(self.pen_last[:1] + b"\x00" + self.pen_last[2:])

    def let_go(self):
        """Whatever touches let go of (the app paused, the desktop locked);
        the pen's device stays, made once (D-Bus, the helper's polkit, the
        tablet's settings written)."""
        if self.gesture is not None:
            self._touch({}, lost=True)
        if self.fling is not None:
            self._stop_fling()
        if self.pen_last is not None:
            self._pen(self.pen_last[:1] + b"\x00" + self.pen_last[2:])

    def close(self, last=False):
        """let_go(), and the pen's device closed (another user's session in
        front, the session's end): made again by prepare(), or as the pen
        comes near; last: never again."""
        self.let_go()
        with self._pen_lock:
            device, self.pen = self.pen, None
            self._making, self._turn, self._last = False, self._turn + 1, self._last or last
        if device:
            device.close()

    # -- fingers ---------------------------------------------------------------
    def _touch(self, fingers, lost=False):
        now = self.clock()
        if self.gesture is None:
            if not fingers:
                return
            if self.fling is not None:
                self._stop_fling()  # a finger down stops a scroll going on, as on a phone
            self.gesture = Gesture(now)
        g = self.gesture
        g.update(fingers, self.size)
        if fingers:
            self._step(g, now)
        else:
            self.gesture = None
            self._end(g, now, lost)

    def _step(self, g, now):
        fingers = g.fingers
        if g.kind == "drag" and len(fingers) >= 2 and now - g.since < PROMOTE:
            # A second finger, late: it was two from the start (a pinch, a
            # scroll), not a drag.
            self.monitor.pointer_button(pen.BTN_LEFT, False)
            g.kind, g.pair = "pending", (_spread(fingers), _middle(fingers))
        if g.kind == "pending":
            self._decide(g, now)
        if g.kind == "drag" and fingers:
            self.monitor.pointer_move(*next(iter(fingers.values())))
        elif g.kind == "scroll":
            if len(fingers) != 2:  # one lifted: no more scrolling from these
                self._scroll_end(g, now, fling=False)
                g.kind = "done"
            else:
                (x, y), (lx, ly) = _middle(fingers), g.last
                if (x, y) != (lx, ly):
                    g.last = (x, y)
                    g.samples.append((now, x - lx, y - ly))
                    while g.samples and now - g.samples[0][0] > VELOCITY_WINDOW:
                        g.samples.popleft()
                    # Natural: the page follows the fingers.
                    self.monitor.pointer_axis((lx - x) / self.scale, (ly - y) / self.scale)
        elif g.kind == "pinch":
            for finger, where in fingers.items():
                if where != g.touching.get(finger):
                    self.monitor.touch_motion(finger, *where)
                    g.touching[finger] = where
            for finger in [f for f in g.touching if f not in fingers]:
                self.monitor.touch_up(finger)
                del g.touching[finger]
        elif g.kind == "swipe" and len(fingers) >= 3:
            (x, y), (x0, y0) = _middle(fingers), g.last
            dx, dy = (x - x0) / self.size[0], (y - y0) / self.size[1]
            if max(abs(dx), abs(dy)) >= SWIPE:
                g.kind = "done"
                if abs(dy) > abs(dx):
                    self._keys([KEY_SUPER] if dy < 0 else [KEY_ESCAPE])  # the overview, or out of it
                else:  # the workspace to the right with the fingers going left, as on the touchpad
                    self._keys([KEY_SUPER, KEY_PAGE_DOWN if dx < 0 else KEY_PAGE_UP])

    def _decide(self, g, now):
        fingers = g.fingers
        if g.most >= 3:
            if len(fingers) >= 3:
                g.kind, g.last = "swipe", _middle(fingers)
            return
        if g.most == 2:
            if len(fingers) != 2:
                return
            spread, (x0, y0) = g.pair
            (x, y) = _middle(fingers)
            shift = math.hypot(x - x0, y - y0)
            change = abs(_spread(fingers) - spread)
            if max(shift, change) < GESTURE_START * self.size[0]:
                return
            if change > shift:
                # A pinch on a touchscreen: browsers, maps and viewers zoom
                # the picture under the fingers, as a touchpad's pinch does
                # (Ctrl and the wheel would zoom the page, reflowed).
                g.kind = "pinch"
                for finger in fingers:
                    self.monitor.touch_down(finger, *g.start[finger])
                    g.touching[finger] = g.start[finger]
            else:
                # From where they began: the way that decided it scrolls too.
                g.kind, g.last = "scroll", (x0, y0)
                self.monitor.pointer_move(x, y)  # the scroll goes to what is under the fingers
            return
        if g.moved >= TAP_SLOP and (now - g.since >= DRAG_WAIT or g.moved >= DRAG_FAST):
            g.kind = "drag"
            self.monitor.pointer_move(*next(iter(g.start.values())))
            self.monitor.pointer_button(pen.BTN_LEFT, True)
        elif now - g.since >= HOLD:
            g.kind = "done"
            self._click(next(iter(g.start.values())), pen.BTN_RIGHT)

    def _end(self, g, now, lost):
        quick = now - g.since < TAP_TIME and not lost
        if g.kind == "pending":
            if g.most == 1 and quick and g.moved < TAP_SLOP:
                self._click(next(iter(g.start.values())), pen.BTN_LEFT)
            elif g.most == 2 and quick and g.moved < TAP_MOVE:
                self._click(next(iter(g.start.values())), pen.BTN_RIGHT)
        elif g.kind == "drag":
            self.monitor.pointer_button(pen.BTN_LEFT, False)
        elif g.kind == "scroll":
            self._scroll_end(g, now, fling=not lost)
        elif g.kind == "pinch":
            for finger in g.touching:
                self.monitor.touch_up(finger)

    def _scroll_end(self, g, now, fling):
        samples = [s for s in g.samples if now - s[0] <= VELOCITY_WINDOW]
        if fling and len(samples) >= 2:
            span = max(samples[-1][0] - samples[0][0], 1 / 120)
            vx = sum(s[1] for s in samples[1:]) / span
            vy = sum(s[2] for s in samples[1:]) / span
            if math.hypot(vx, vy) >= FLING_START:
                self.fling = [vx, vy, now]
                return
        self.monitor.pointer_axis(0.0, 0.0, True)  # finish

    def _fling(self, now):
        vx, vy, at = self.fling
        dt = now - at
        decay = math.exp(-dt / FLING_TAU)
        # What the speed covers over dt, slowing down all the while.
        dx, dy = vx * FLING_TAU * (1 - decay), vy * FLING_TAU * (1 - decay)
        vx, vy = vx * decay, vy * decay
        self.monitor.pointer_axis(-dx / self.scale, -dy / self.scale)
        if math.hypot(vx, vy) < FLING_STOP:
            self._stop_fling()
        else:
            self.fling = [vx, vy, now]

    def _stop_fling(self):
        self.fling = None
        self.monitor.pointer_axis(0.0, 0.0, True)  # finish

    def _click(self, where, button):
        self.monitor.pointer_move(*where)
        self.monitor.pointer_button(button, True)
        self.monitor.pointer_button(button, False)

    def _keys(self, keys):
        for key in keys:
            self.monitor.keysym(key, True)
        for key in reversed(keys):
            self.monitor.keysym(key, False)

    # -- the pen ---------------------------------------------------------------
    def prepare(self):
        """The pen's device made now, on a thread of its own, unless it is
        there or on its way: making it takes D-Bus calls and the helper's
        polkit (up to 25 s), which on the listening thread, at the first
        stroke, held the fingers, the keyframes and the stats meanwhile."""
        with self._pen_lock:
            if self.pen is not None or self._making or self._last:
                return
            self._making = True
            turn = self._turn
        threading.Thread(target=self._make_pen, args=(turn,), name="plusone-pen", daemon=True).start()

    def _make_pen(self, turn):
        try:
            made = self.make_pen()
        except Exception as e:  # noqa: BLE001 - no pen, the fingers still work
            log("app display: no pen:", repr(e))
            made = False
        with self._pen_lock:
            if turn == self._turn:
                self.pen, self._making = made, False
                return
        if made:
            made.close()  # let go of meanwhile (close()): not left open

    def _pen(self, report):
        device = self.pen
        if device:
            device.report(report)
        elif device is None and report[1] & pen.IN_RANGE:
            self.prepare()  # closed since (another user's session was in front): made again, not waited for
        pen.feed.publish(report)  # the signing window draws with them as they come
        self.pen_last = report if report[1] & pen.IN_RANGE else None


class RateControl:
    """The bitrate the network takes, from the app's report each second:
    down by a third at once when frames were lost, the delay through the
    network grew (a queue filling somewhere), or SKIPPED frames or more were
    not encoded for what waited to go out here (the same queue, seen before
    it shows in the delay: 15 s sooner, measured on a phone's hotspot); up
    by a tenth after a few clean seconds, never above the one set (most)
    nor below MIN_KBPS. A frame or two not encoded (after a keyframe, say)
    is not clean either: no going up into a queue.

    Where a queue showed (ceiling: not a frame lost now and then, which
    any Wi-Fi does) is remembered for CEILING_FOR reports: up to NEAR of it
    the bitrate goes back quickly, past that only by PROBE every few clean
    seconds (it went back to the most, and gave up again, every 30 s on a
    phone's hotspot: the picture stuttered each time). Past the ceiling
    with no trouble, or once it is old, it is forgotten. A drop well under
    it (the same queue, a second later) leaves it where it was.
    The delay through the network: the app's capture-to-decoded less its
    first-packet-to-decoded (decoding, which does not change with the
    network), against the least of the last seconds; only from a second
    with enough frames: a still desktop's one or two say little (Wi-Fi's
    own jitter), and it went down for nothing.

    Each change of the encoder's bitrate is a keyframe (Intel's VA
    encoders start a new sequence on it: measured, vah264lpenc and
    vaapih264enc alike), a burst of up to VBV_FRAMES frames just when the
    network took less: the picture stuttered at each of the controller's
    cuts, and at each of its steps up. Following a budget, the bitrate
    goes down only once it no longer FITS in it (its share leaves 15 %: a
    smaller cut costs nothing), and has not for HOLD; whole frames are left
    out meanwhile to stay within the budget (Allowance), none late; with
    PAY, down sooner once those are worth PAY frames (a keyframe is
    VBV_FRAMES). The controller cut by 30 % six times a minute on a good
    Wi-Fi, most of them taken back within the second, and each one's
    keyframe made the frames after it late, lost, asked for again: held a
    second, as l36 was first made. But with keyframes made only once they
    fit and none after one still on its way (AppStreamer), a keyframe costs
    about its own bytes, and on the emulated grid's chain (adsl, lossy,
    spikes; pathbench --variant) every 250 ms of hold left out more frames
    than the keyframes it saved: none, frames shown within 2 points of the
    desktop before l36 (1000 ms: 5 fewer, 500: 3), a fifth fewer keyframes
    (500 ms: more than a quarter), the delay's 99th percentile 3 ms lower
    (500 ms: 6 higher). Up only by UP at least, UP_AFTER after its last
    change. A stream starts at the budget's share (start()). HOLD, PAY and
    FITS as L36 has them."""

    SHARE = 0.85    # of a path's budget (its congestion controller's), the video's
    STEADY = 0.05   # up to the most: by this much at least
    FITS = 0.95     # of the budget (less the others'): over it, down to the share
    UP = 1.15       # the share over the bitrate by this much: up to it (the controller
                    # goes 1.5 times what goes through at most: 1.25 held at 9.5 Mbit/s
                    # for 90 s on a hotspot taking 50)...
    UP_AFTER = 2.0  # ...this long (s) after the last change at least
    HOLD = 0.0      # s: the budget under what the bitrate needs this long before it goes down (L36)...
    PAY = 0         # ...or once the frames left out for it are worth this many (0: never)
    GROWN_MS = 25   # over the least: a queue
    CLEAN = 3       # reports in a row without loss nor queue before going up
    HISTORY = 10    # reports the least is taken over
    FRAMES = 10     # shown in the second, for its delay to count
    SKIPPED = 3     # frames not encoded in the second: the network behind
    NEAR = 0.85     # of the ceiling: up to here quickly
    PROBE = 1.03    # past it, up by this...
    PROBE_EVERY = 10  # ...after this many clean reports
    CEILING_FOR = 60  # reports (seconds) a ceiling holds

    def __init__(self, most, fps=60, tuning=None):
        tuning = tuning or L36()
        self.hold, self.pay, self.fits, self.fps = tuning.hold, tuning.pay if tuning.on else 0, tuning.share, fps
        self.most = most
        self.kbps = most
        self.clean = 0
        self.ceiling = None  # where a queue last showed
        self.ceiling_age = 0  # reports since
        self.delays = collections.deque(maxlen=self.HISTORY)
        self.changed_at = -math.inf  # the bitrate's last change for a budget (time.monotonic())
        self.why = None  # what the last change returned was for, for the log
        self.last = None  # the last budget (kbit/s) and the others' share
        self.under = None  # since when the bitrate no longer fits in the budget (time.monotonic())
        self.over = 0.0  # bytes it went over the budget by since (what the allowance leaves out)
        self.over_at = None  # (time.monotonic()) counted up to

    def report(self, shown, lost, capture_to_decoded, first_to_decoded, skipped=0, budget=None, others=0, now=None):
        """A report, and the frames not encoded since the last; the new
        bitrate if it changed, else None. budget: what the path's congestion
        controller allows (kbit/s), if it has one (QUIC's GCC): the bitrate
        is SHARE of it, and a frame lost is a keyframe's business, not the
        bitrate's (a Wi-Fi loses one now and then; cut by a third each time,
        the picture went from 20 to 4 Mbit/s in 4 s on a hotspot taking 36,
        and took 50 s to come back). others: what else of ours goes to the
        tablet now (kbit/s), out of the budget's share."""
        if budget:
            return self.budget(budget, others, now)
        delay = capture_to_decoded - first_to_decoded if shown >= self.FRAMES else None
        if delay is not None:
            self.delays.append(delay)
        grown = delay is not None and len(self.delays) >= 3 and delay > min(self.delays) + self.GROWN_MS
        self.ceiling_age += 1
        if self.ceiling and self.ceiling_age > self.CEILING_FOR:
            self.ceiling = None
        queue = grown or skipped >= self.SKIPPED
        if lost or queue:
            self.clean = 0
            if queue and (self.ceiling is None or self.kbps >= self.ceiling * self.NEAR):
                self.ceiling, self.ceiling_age = self.kbps, 0
            self.why = "; ".join(why for why in (lost and f"{lost} frames lost", grown and "the delay grew",
                                                 skipped >= self.SKIPPED and f"{skipped} not encoded") if why)
            return self._set(self.kbps * 2 // 3)
        if skipped:
            return None  # not clean, but not a reason to count again from naught
        self.clean += 1
        if self.kbps >= self.most:
            return None
        if self.ceiling and self.kbps >= self.ceiling * self.NEAR:
            if self.clean < self.PROBE_EVERY:
                return None
            self.clean = 0
            self.why = "clean, past where a queue showed"
            kbps = self._set(int(self.kbps * self.PROBE))
            if self.kbps >= self.ceiling:
                self.ceiling = None  # it takes as much now
            return kbps
        if self.clean >= self.CLEAN:
            self.clean = 0
            up = self.kbps * 11 // 10 + 100
            if self.ceiling:
                up = min(up, int(self.ceiling * self.NEAR))
            self.why = f"clean for {self.CLEAN} s"
            return self._set(max(up, self.kbps + 1))
        return None

    def budget(self, budget, others=0, now=None):
        """The path's congestion controller's budget (kbit/s), now (also as
        plus-one-link tells it changed, between reports), and what else of
        ours goes to the tablet (the speakers): the new bitrate if it
        changed, else None."""
        now = time.monotonic() if now is None else now
        self.clean, self.ceiling = 0, None
        if self.under is not None and self.last is not None:  # over the last budget until now
            self.over += max(0.0, self.kbps - self.allowed(*self.last)) * 125 * (now - self.over_at)
        self.last = (budget, others)
        target = max(MIN_KBPS, min(self.most, int(budget * self.SHARE - others)))
        if self.kbps <= self.allowed(budget, others):
            self.under = None
        elif self.under is None:
            self.under, self.over = now, 0.0
        self.over_at = now
        if target < self.kbps:
            paid = self.pay and self.over >= self.pay * self.kbps * 125 / self.fps
            if self.under is None or now - self.under < self.hold and not paid:
                return None
        elif (target < self.kbps * self.UP and (target < self.most or target < self.kbps * (1 + self.STEADY))
              or now - self.changed_at < self.UP_AFTER):
            return None
        under, self.under = self.under, None
        kbps = self._set(target)
        if kbps is not None:
            self.changed_at = now
            self.why = (f"the budget {budget} kbit/s, the others' {others:.0f}"
                        + (f"; under it for {now - under:.1f} s" if under is not None and now > under else ""))
        return kbps

    def start(self, budget, others=0):
        """The bitrate a stream starts at, on a path whose budget is known:
        its share at once (the first frame is a keyframe anyway: no change
        to hold back), not the most and a hold of frames left out."""
        self.kbps = max(MIN_KBPS, min(self.most, int(budget * self.SHARE - others)))
        self.changed_at, self.last = time.monotonic(), (budget, others)
        return self.kbps

    def tick(self, now=None):
        """Between the budget's events (it may tell nothing new for long):
        the new bitrate once the budget has been under it for HOLD, else
        None."""
        if self.under is None or self.last is None:
            return None
        return self.budget(*self.last, now=now)

    def unbudgeted(self):
        """The path has no controller of its own now (TCP, a cable)."""
        self.last = self.under = None

    def lower(self, kbps, why, now=None):
        """Down to kbps (never under MIN_KBPS) for a keyframe that would not
        get through at the bitrate (AppStreamer): up again as ever, UP_AFTER
        on at the earliest. The bitrate now."""
        if self._set(kbps) is not None:
            self.changed_at, self.under, self.why = time.monotonic() if now is None else now, None, why
        return self.kbps

    def allowed(self, budget, others=0):
        """What the video may send within a budget (kbit/s): FITS of it,
        less the others'."""
        return max(0.0, (budget - others) * self.fits)

    def _set(self, kbps):
        kbps = max(MIN_KBPS, min(self.most, kbps))
        if kbps == self.kbps:
            return None
        self.kbps = kbps
        return kbps


class Allowance:
    """What the video may still send within the path's budget (bytes), a
    bucket: it fills at what RateControl.allowed() leaves the video, up to
    DEPTH frames' worth (a keyframe's VBV_FRAMES, and one), and each frame
    sent takes its bytes out. A frame goes into the encoder only while the
    bucket is not in debt: over the budget (a cut the bitrate has not
    followed yet, HOLD), whole frames are left out, the next encoded
    against the last (nothing breaks), and what goes stays within the
    budget. No budget (TCP, the link of before): no limit."""

    DEPTH = media.VBV_FRAMES + 1

    def __init__(self, fps, depth=DEPTH):
        self.fps, self.depth = fps, depth
        self.kbps = None  # what fills it; None: no budget
        self.level = 0.0
        self.at = None
        self.lock = threading.Lock()  # the budget's thread, and the streaming one

    def follow(self, kbps, now=None):
        """The video's part of the budget now (kbit/s), or None."""
        now = time.monotonic() if now is None else now
        with self.lock:
            if kbps is None:
                self.kbps, self.level, self.at = None, 0.0, None
                return
            if self.kbps is None:
                self.kbps, self.level, self.at = kbps, self._depth(kbps), now  # full at first
                return
            self._fill(now)
            self.kbps = kbps
            self.level = min(self.level, self._depth(kbps))

    def allows(self, now):
        with self.lock:
            if self.kbps is None:
                return True
            self._fill(now)
            return self.level >= 0

    def take(self, size, now):
        with self.lock:
            if self.kbps is not None:
                self._fill(now)
                self.level -= size

    def _depth(self, kbps):
        return kbps * 125 * self.depth / self.fps

    def _fill(self, now):
        self.level = min(self._depth(self.kbps), self.level + self.kbps * 125 * max(0.0, now - self.at))
        self.at = now


class L36:
    """How the desktop takes the link's cuts (l36), from the environment as
    each stream starts: a live A/B without rebuilding, said in the log.
    PLUSONE_L36=0: as before l36, the bitrate down at once whenever it no
    longer fits in the budget (a keyframe on Intel's encoders), every frame
    encoded sent, keyframes made as asked. Else (the default) whole frames
    are left out to stay within the budget (Allowance) until the bitrate
    follows a cut, keyframes are made once they fit (as big as the last
    ones the encoder made, the bitrate down first if they never could),
    one the link cut and the app asked again for is followed by a smaller
    one, alone, whole frames go to the link, and the stream starts at the
    budget's share. Its knobs, the frames left out (each kept within its
    range, said if not):
    PLUSONE_L36_HOLD_MS: how long the budget must stay under the bitrate
    before it goes down (RateControl.HOLD: none as it ships), on an encoder
    whose every change of bitrate is a keyframe (Intel's: elsewhere, none);
    PLUSONE_L36_PAY: down sooner once the frames left out for it are worth
    this many frames at the bitrate (0: never; a change costs a keyframe,
    media.VBV_FRAMES of them);
    PLUSONE_L36_DEPTH: the allowance's depth, in frames (past 5, a burst
    nearly as long as the link holds realtime for: LATE);
    PLUSONE_L36_FITS: what the video may send of the budget less the
    others', in percent (never past all of it), and what the bitrate must
    stay under."""

    # attribute: (environment, default, least, most)
    KNOBS = {"hold_ms": ("PLUSONE_L36_HOLD_MS", round(RateControl.HOLD * 1000), 0, 3000),
             "pay": ("PLUSONE_L36_PAY", RateControl.PAY, 0, 30),
             "depth": ("PLUSONE_L36_DEPTH", Allowance.DEPTH, 1, 5),
             "fits": ("PLUSONE_L36_FITS", round(RateControl.FITS * 100), 80, 100)}

    def __init__(self, on=True, **knobs):
        self.on = on
        self.wrong = []  # what the environment asked that was not taken, for the log
        for name, (_env, default, least, most) in self.KNOBS.items():
            setattr(self, name, max(least, min(most, knobs.get(name, default))))

    @classmethod
    def from_env(cls, environ=None):
        environ = os.environ if environ is None else environ
        on = environ.get("PLUSONE_L36", "1").strip() != "0"
        knobs, wrong = {}, []
        for name, (env, default, least, most) in cls.KNOBS.items():
            value = environ.get(env)
            if value is None:
                continue
            try:
                number = int(value)
            except ValueError:
                wrong.append(f"{env}={value} (not a whole number)")
                continue
            if not least <= number <= most:
                wrong.append(f"{env}={value} (kept within {least}-{most})")
            knobs[name] = number
        tuning = cls(on, **knobs)
        tuning.wrong = wrong
        return tuning

    @property
    def hold(self):
        """s; before l36, none."""
        return self.hold_ms / 1000 if self.on else 0.0

    @property
    def share(self):
        """Of the budget less the others', what the video may send."""
        return self.fits / 100 if self.on else RateControl.FITS

    def describe(self, restarts=True):
        """restarts: each change of the encoder's bitrate is a keyframe (else
        no hold: AppStreamer._encoder_restarts)."""
        if not self.on:
            said = "l36 off (PLUSONE_L36=0): the bitrate down at once, every frame sent"
        else:
            held = (f"hold {self.hold_ms} ms, pay {self.pay or 'off'}{' frames' if self.pay else ''}" if restarts
                    else "no hold (a change of bitrate is no keyframe on this encoder)")
            said = f"l36 on, {held}, allowance {self.depth} frames at {self.fits} % of the budget"
        return said + "".join(f"; {wrong}" for wrong in self.wrong)


class LinkPackets:
    """The desktop's packets over the link (plus-one-link, docs/LINK.md),
    as Datagrams carries them over the link of before: VIDEO, PING and PONG
    out and KEYFRAME, PONG and STATS in on the media channel, INPUT in on
    an input channel (the newest state kept, never queued); queued() is
    plus-one-link's backlog to the tablet. The media channel a VIDEO one
    (plusone_link: each frame a group, on a QUIC stream of its own where
    the path takes them; the app reads it as realtime), or REALTIME where
    plus-one-link has no VIDEO: each message as it is."""

    def __init__(self, connection, device, control, media, input):
        self.connection, self.device = connection, device
        self.control, self.media, self.input = control, media, input
        # On VIDEO each message carries the link's head too (taken off
        # before the wire): no message past MAX_DATAGRAM, head and all.
        self.grouped = getattr(media, "kind", None) == plusone_link.VIDEO
        self.payload = (plusone_link.MAX_DATAGRAM - VIDEO_HEADER.size
                        - (plusone_link.HEAD.size if self.grouped else 0))
        self._open = [media, input]
        self.errors = set()
        self.budget = channels.Budget(connection, device)

    def send(self, body):
        """A message of its own (PING, PONG): on VIDEO, of no group."""
        self._sent(self.media.send, body)

    def send_in_frame(self, body, frame, keyframe, last):
        """One of frame's packets (its number), whether it is a keyframe and
        whether this is its last: on VIDEO, the frame's group."""
        self._sent(self.media.send_in_group, frame, keyframe, last, body)

    def _sent(self, send, *args):
        try:
            send(*args)
        except OSError as e:
            if e.errno not in self.errors:
                self.errors.add(e.errno)
                log("app display: cannot send:", e.strerror)

    def queued(self):
        try:
            return self.connection.queued(self.device)
        except (OSError, ValueError, plusone_link.LinkError):
            return 0

    def dropped(self):
        """Realtime the link dropped so far (late, or no room), or None if
        not known."""
        return self._device().get("dropped")

    def rtt(self):
        """The session's round trip (seconds), or None if not known."""
        ms = self._device().get("rtt_ms")
        return ms / 1000 if ms else None

    def budget_kbps(self):
        """What the path's congestion controller allows now (kbit/s): a
        QUIC path's (GCC), else None (the stream of before has none)."""
        return self.budget.kbps()

    def others_kbps(self):
        """What else of ours goes to the tablet now (kbit/s): the speakers'."""
        return channels.sending_kbps(self.device)

    def watch_budget(self, on_budget):
        """on_budget(kbit/s, or None: no controller of its own) each time
        plus-one-link says the path's budget changed (channels.Budget)."""
        self.budget.watch(on_budget)

    def _device(self):
        try:
            return next((d for d in self.connection.devices() if d["id"] == self.device), None) or {}
        except (OSError, ValueError, plusone_link.LinkError):
            return {}

    def receive(self, timeout):
        """A body from the app, or None (nothing in time, or the channels over)."""
        try:
            ready = select.select(self._open, [], [], timeout)[0] if self._open else []
        except (OSError, ValueError):  # closed
            return None
        if not self._open:
            time.sleep(timeout)
        for channel in ready:
            try:
                body = channel.recv()
            except OSError:
                body = b""
            if body:
                return body
            self._open.remove(channel)  # its end: not waited on again
        return None

    def close(self):
        self.budget.close()
        for channel in (self.control, self.media, self.input):
            channel.close()
        self.connection.close()


class AppStreamer:
    """The virtual monitor (or a test pattern), encoded, to the app."""

    def __init__(self, source, channel, width, height, fps, bitrate, encoder, on_stats=None, input=None,
                 on_visible=None, preview=False, on_error=None):
        """input: an Input for the fingers and the pen, or None.
        on_visible(shown): from the listening thread, when the app starts or
        stops showing the desktop (left for another app, its screen off).
        on_error(message): on the main loop, if the pipeline fails."""
        self.channel = channel
        self.on_stats = on_stats or (lambda stats: None)
        self.on_visible = on_visible or (lambda shown: None)
        self.on_error = on_error or (lambda message: None)
        self.input = input
        self.shown = True
        # seat.py: closed, the app's input reaches nothing (and what touches
        # is let go); inactive, nothing is captured either.
        self.gated, self.active, self._let_go = False, True, False
        # visible() on the listening thread, gate() on the main loop: each
        # one's flag, read by the other, and the state they make together.
        self._state_lock = threading.Lock()
        if source == "test":
            video = (f"videotestsrc is-live=true pattern=ball ! "
                     f"video/x-raw,format={media.CAPTURE_FORMAT},width={width},height={height},"
                     f"framerate={fps}/1 ! "
                     f"timeoverlay font-desc=\"Sans 90\" valignment=center halignment=center")
            if preview:
                # The same picture in a window here: a photo of both screens
                # reads the delay to the tablet's glass off the two timers.
                video += (" ! tee name=preview ! queue max-size-buffers=1 leaky=downstream ! "
                          "videoscale ! video/x-raw,width=960,height=600 ! videoconvert ! "
                          "autovideosink sync=false preview.")
        else:
            # Mutter gives the virtual monitor the stream's max-framerate as
            # its refresh rate; asking for more than the tablet shows only
            # queued frames there (measured with Second Screen: 120 ran at up
            # to 90 fps, the delay grew).
            video = (f"pipewiresrc path={source} do-timestamp=true always-copy=false keepalive-time=100 ! "
                     f"video/x-raw,format={media.CAPTURE_FORMAT},width={width},height={height},"
                     f"max-framerate={fps}/1")
        self._rates(channel, fps, bitrate)  # (the bitrate it starts at, self.kbps: the budget's share, if told)
        element, options, raw_format = media.ENCODERS[encoder]
        enc = f"{element} name=enc " + options.format(bitrate=self.kbps, gop=fps * media.KEYFRAME_SECONDS)
        convert = media.CONVERT.get(encoder, f"videoconvert n-threads=4 ! video/x-raw,format={raw_format}")
        self.pipeline = Gst.parse_launch(
            f"{video} ! queue max-size-buffers=1 leaky=downstream ! {convert} ! "
            f"{enc} ! video/x-h264,profile=high ! h264parse config-interval=-1 ! "
            f"video/x-h264,stream-format=byte-stream,alignment=au ! "
            f"appsink name=out emit-signals=true sync=false async=false max-buffers=0")
        enc = self.enc = self.pipeline.get_by_name("enc")
        media.cap_keyframes(enc, self.kbps, fps)
        self._encoder_restarts(enc.get_factory().get_name() in media.RESTARTS)
        self.keyframes = media.Keyframes(enc, on_flush=self._captured.clear)
        self._in_pad = enc.get_static_pad("sink")
        self._in_probe = self._in_pad.add_probe(Gst.PadProbeType.BUFFER, self._frame_in)
        self._sink = self.pipeline.get_by_name("out")
        self._handler = self._sink.connect("new-sample", self._send)
        bus = self.pipeline.get_bus()
        bus.add_signal_watch()
        self._bus_handler = bus.connect("message::error", self._failed)
        self._feedback = threading.Thread(target=self._listen, daemon=True)
        to = (f"{channel.app[0]}:{channel.app[1]}" if isinstance(channel, Datagrams)
              else "the link, a frame a group" if getattr(channel, "grouped", False) else "the link")
        log(f"app display: {width}x{height}@{fps} {encoder} {self.kbps} kbit/s"
            + (f" (the budget's share; at most {bitrate})" if self.kbps != bitrate else "") + f" -> {to}")
        log(f"app display: {self.l36.describe(self._restarts)}")

    def _rates(self, channel, fps, bitrate):
        """What decides which frames go and at what bitrate (not GStreamer's:
        the tests make a streamer of it alone)."""
        self.channel, self.fps = channel, fps
        self.l36 = L36.from_env()  # PLUSONE_L36=0: as before l36, for a live A/B
        self.rate = RateControl(bitrate, fps, self.l36)
        self.kbps = bitrate  # the encoder's
        # The stats' thread, the link's budget events, the streaming thread
        # (a bitrate change made with the next frame in).
        self._rate_lock = threading.RLock()
        self._restarts = False  # each change of the bitrate is a keyframe (media.RESTARTS)
        self._pending = None  # a bitrate decided, (kbit/s, why), not made yet
        self._others = 0  # what else of ours goes to the tablet (kbit/s), as last asked
        self.skipped = collections.Counter()  # frames not encoded, by why (LEFT_OUT)
        # The path's budget (kbit/s), if its controller is its own (QUIC's):
        # what waits counts against it, and in bytes as they are.
        self._budget_kbps = channel.budget_kbps() if hasattr(channel, "budget_kbps") else None
        self.allowance = Allowance(fps, self.l36.depth)
        if self._budget_kbps and self.l36.on:
            self._others = channel.others_kbps() if hasattr(channel, "others_kbps") else 0
            self.kbps = self.rate.start(self._budget_kbps, self._others)
            self._follow(self._budget_kbps, self._others)
        self._frame = 0
        self._last_keyframe = 0.0  # one asked for, or sent
        self._keyframes = 0  # sent since the last stats
        self._key_at = None  # when the last keyframe went to the link (time.monotonic())
        self._after_key = 0  # bytes sent since
        self._key_flying = False  # it still waits to go out, as of the last frame in
        self._asks_held = 0  # the app's asks not answered for one on its way, since the last stats
        self._key_wanted = False  # a keyframe to make, once the whole of it fits in the link (_keyframe_in)
        self._key_made = -math.inf  # when one was last made (time.monotonic())
        self._broken = False  # a frame was dropped: the next ones are of no use until a keyframe
        self._queued = 0  # what waited in the link at the last frame in (bytes)
        self._key_worths = collections.deque(maxlen=KEY_SIZES)  # the last keyframes, in average frames (_key_worth)
        self._key_size = 0  # the last keyframe sent (bytes on the wire)
        self._key_dropped = None  # the link's count of realtime dropped as it went (LinkPackets.dropped)
        self._alone_from = None  # since a keyframe was lost: the next goes alone (_left_out)
        self._keys_lost = 0  # since the last stats
        self.dropped = collections.Counter()  # encoded frames not sent, by why (DROPPED)
        self._encoded = []  # ms from capture to sent, since the last stats
        # When each frame going into the encoder was captured, in order: one
        # comes out per one in (no B frames), and a flush drops those held.
        # (Its output timestamps could not be traced back after a flush.)
        self._captured = collections.deque()
        self._running = True

    def _encoder_restarts(self, restarts):
        """Whether each change of the encoder's bitrate is a keyframe (Intel's,
        media.RESTARTS): only then is a cut held before the bitrate goes
        down (HOLD, PAY). Elsewhere (NVENC, x264) a change is made in place:
        down at once, no frames left out for a keyframe never made."""
        self._restarts = restarts
        if not restarts:
            self.rate.hold = self.rate.pay = 0

    def start(self):
        with self._state_lock:
            self._capture()  # gate() may have come first: another user's session in front
        self._feedback.start()
        if hasattr(self.channel, "watch_budget"):
            self.channel.watch_budget(self._budget)

    def _budget(self, kbps):
        """plus-one-link says the path's budget changed: followed at once,
        not at the next report (a second later: the queue filled meanwhile)."""
        self._budget_kbps = kbps
        if not self._running:
            return
        if not kbps:
            with self._rate_lock:
                self.rate.unbudgeted()
                self.allowance.follow(None)
            return
        others = self._others = self.channel.others_kbps() if hasattr(self.channel, "others_kbps") else 0
        with self._rate_lock:
            self._follow(kbps, others)
            changed = self.rate.budget(kbps, others)
            if changed is not None:
                self._bitrate(changed, self.rate.why)

    def _follow(self, budget, others):
        """The allowance filling at what the budget leaves the video; before
        l36, no allowance (every frame went)."""
        self.allowance.follow(self.rate.allowed(budget, others) if self.l36.on else None)

    def _tick(self):
        """From the listening thread, a few times a second, and with each
        frame in while over the budget: the bitrate down once the budget
        has been under it for HOLD (or the frames left out pay for it)."""
        if not self.l36.on:
            return
        with self._rate_lock:
            changed = self.rate.tick()
            if changed is not None:
                self._bitrate(changed, self.rate.why)

    def _bitrate(self, kbps, why):
        """The encoder's bitrate to change. On an encoder whose every change
        is a keyframe (RESTARTS), with the next frame in once the last
        keyframe is out (_frame_in): one into the queue the last still waits
        in made the frames behind it late, and the app asked again."""
        with self._rate_lock:
            self._pending = (kbps, why)
            if not self._restarts or not self.l36.on:
                self._make_bitrate()

    def _make_bitrate(self):
        """The bitrate decided, made now, and said (each change, from the
        budget's events too: the live tests count them)."""
        with self._rate_lock:
            pending, self._pending = self._pending, None
            if pending is None or pending[0] == self.kbps:
                return
            kbps, why = pending
            log(f"app display: {self.kbps} -> {kbps} kbit/s, as the network takes ({why})")
            self.kbps = kbps
            self.enc.set_property("bitrate", kbps)
            media.cap_keyframes(self.enc, kbps, self.fps)

    def stop(self):
        self._running = False
        self._feedback.join(timeout=2)
        if self.input:
            self.input.close(last=True)
        self.pipeline.set_state(Gst.State.NULL)
        self._sink.disconnect(self._handler)
        bus = self.pipeline.get_bus()
        bus.disconnect(self._bus_handler)
        bus.remove_signal_watch()
        self.keyframes.remove()
        self._in_pad.remove_probe(self._in_probe)

    def _failed(self, _bus, message):
        error, debug = message.parse_error()
        log("app display: pipeline error:", error.message, debug or "")
        self.on_error(f"{message.src.get_name()}: {error.message}")

    def _frame_in(self, pad, info):
        now = time.monotonic()
        why = self._left_out(now) or self._keyframe_in(pad, now)
        if why:
            self.skipped[why] += 1
            return Gst.PadProbeReturn.DROP
        # pipewiresrc and a live test source stamp buffers with the running
        # time at capture, and the pipeline's clock is the monotonic one.
        buf = info.get_buffer()
        event = pad.get_sticky_event(Gst.EventType.SEGMENT, 0)
        running = event.parse_segment().to_running_time(Gst.Format.TIME, buf.pts) if event else Gst.CLOCK_TIME_NONE
        self._captured.append(self.pipeline.get_base_time() + running
                              if running != Gst.CLOCK_TIME_NONE else time.monotonic_ns())
        while len(self._captured) > 16:  # never out of step for long
            self._captured.popleft()
        return Gst.PadProbeReturn.OK

    def _left_out(self, now):
        """Why the frame going into the encoder now is not encoded, or None.
        A frame that never went in breaks nothing; the next one is encoded
        against the last.
        "queue": the network behind (more waiting to go out than QUEUE_MS
        of the bitrate), rather than queued after the others (the delay
        would grow, and grow). On a path with a budget, QUEUE_MS of it,
        under what the link holds realtime for (quic.rs LATE, 100 ms of the
        same rate): past it the link drops a frame's packets, and the
        frames after are broken.
        "budget": over the budget's allowance (Allowance).
        "alone": a keyframe made after one was lost (force_keyframe) still
        waits to go out: the whole link its own.
        Before l36: QUEUE_MS of the budget, 64 KB at least, alone."""
        budget = self._budget_kbps
        queued = self._queued = self.channel.queued()
        if not self.l36.on:
            self._key_flying = False
            most = (min(self.rate.kbps, budget) * QUEUE_MS // 8 if budget
                    else self.rate.kbps * QUEUE_MS // 8 * TRUESIZE)
            return "queue" if queued > max(64 << 10, most) else None
        if self.rate.under is not None:
            self._tick()  # the hold to the frame, not to the listener's next wake (up to IDLE_WAIT)
        self._key_flying = self._flying(queued, now)
        if self._alone_from is not None and self._key_at is not None and self._key_at >= self._alone_from:
            if self._key_flying:
                return "alone"
            self._alone_from = None  # out: the frames after it go
        if budget:
            # And room left for an average frame under what the link holds
            # (on a low budget, the 64 KB let in frames the link cut).
            most = max(64 << 10, min(self.rate.kbps, budget) * QUEUE_MS // 8)
            most = max(ALONE, min(most, self._holds(budget) - self.kbps * 125 // self.fps))
        else:
            most = max(64 << 10, self.rate.kbps * QUEUE_MS // 8 * TRUESIZE)
        if queued > most:
            return "queue"
        if not self.allowance.allows(now):
            return "budget"
        return None

    @staticmethod
    def _holds(budget):
        """What the link holds of realtime before it drops it as late (bytes),
        on a path with a budget (kbit/s)."""
        return min(budget * 125 * LATE, LINK_ROOM) * TOLD

    def _fits(self, size, queued, budget):
        """Whether a frame of size goes to the link whole: none of it late."""
        return not budget or queued + size <= self._holds(budget) or queued <= ALONE

    def _key_fits(self, size, queued, budget, kbps):
        """Whether a keyframe of size, at kbps, goes to the link whole: all of
        it within what the link holds behind what waits; alone (ALONE) only
        at MIN_KBPS, where none can be made smaller."""
        return not budget or queued + size <= self._holds(budget) or kbps <= MIN_KBPS and queued <= ALONE

    def _key_worth(self):
        """A keyframe as reckoned, in average frames at its bitrate (bytes on
        the wire): the most of the last KEY_SIZES, KEY_WORTH at least."""
        return max([KEY_WORTH, *self._key_worths])

    def _key_kbps(self, budget):
        """The most bitrate whose keyframe, as reckoned, fits in what the link
        holds with ALONE waiting before it (kbit/s)."""
        return int((self._holds(budget) - ALONE) * self.fps / (125 * self._key_worth()))

    def _key_time(self):
        """The last keyframe's time on its way out at the budget (s); before
        one went, VBV_FRAMES' at the bitrate."""
        if not self._key_size:
            return media.VBV_FRAMES / self.fps
        return self._key_size / ((self._budget_kbps or self.kbps) * 125)

    def _keyframe_in(self, pad, now):
        """A keyframe with this frame if one is due (asked for, or the stream
        broken; a bitrate change on Intel's is one too), once the last is out
        (_flying) and the whole of it fits in the link behind what waits now:
        as reckoned from the last ones the encoder made (_key_worth), at its
        bitrate. A bitrate whose keyframe could not fit even with nothing
        waiting goes down first (_key_room), the change made with it.
        "keyframe" if the frame is left out meanwhile: the app waits for one,
        and a frame that is not one is of no use to it (the queue empties
        sooner); else None. Before l36, made as asked."""
        if not self.l36.on:
            return None
        due = self._key_wanted or (self._broken and now - self._key_made >= MIN_KEYFRAME_GAP)
        if not (due or self._pending is not None):
            return None  # (a keyframe made and not out yet: the frames after it go behind it)
        budget = self._budget_kbps
        with self._rate_lock:
            if budget:
                self._key_room(budget, due, now)
            change = self._pending is not None
            kbps = self._pending[0] if change else self.kbps
        size = self._key_worth() * kbps * 125 / self.fps
        if self._key_flying or not self._key_fits(size, self._queued, budget, kbps):
            return "keyframe" if due else None
        if change:
            self._make_bitrate()  # its keyframe with this frame
        if due:
            self._key_wanted, self._key_made = False, now
            self.keyframes.now(pad)
        return None

    def _key_room(self, budget, due, now):
        """Under _rate_lock: the bitrate a keyframe goes at (a change's, or
        the bitrate for one due) never past the one whose keyframe, as
        reckoned, fits in what the link holds (_key_kbps). Past it, down to
        it (RateControl.lower: up again UP_AFTER on, as ever), the change
        made with the keyframe; a change up only so far, or not at all.
        Sent as it was, the link cut its tail, and the app asked again."""
        kbps = self._pending[0] if self._pending is not None else self.kbps
        most = max(MIN_KBPS, self._key_kbps(budget))
        if kbps <= most or not (due or self._pending is not None):
            return
        why = f"its keyframe, {self._key_worth():.1f} frames' worth, to fit in what the link holds"
        kbps = self.rate.lower(min(most, self.rate.kbps), why, now)
        self._pending = (kbps, why) if kbps != self.kbps else None

    def _flying(self, queued, now):
        """Whether the last keyframe still waits to go out: more waits than
        went after it (ours, and the others' at their rate; the kernel's
        count of a packet with its buffers, off the link), for
        KEY_FLYING_MOST at most."""
        if self._key_at is None or now - self._key_at >= KEY_FLYING_MOST:
            return False
        after = self._after_key + self._others * 125 * (now - self._key_at)
        return queued > after * (1 if self._budget_kbps else TRUESIZE)

    def _send(self, appsink):
        """Streaming thread: one frame, out at once."""
        sample = appsink.emit("pull-sample")
        if sample is None:
            return Gst.FlowReturn.EOS
        buf = sample.get_buffer()
        data = buf.extract_dup(0, buf.get_size())
        captured = self._captured.popleft() if self._captured else time.monotonic_ns()
        self._out(data, not buf.has_flags(Gst.BufferFlags.DELTA_UNIT), captured)
        return Gst.FlowReturn.OK

    def _out(self, data, keyframe, captured):
        """An encoded frame to the link whole, or not at all. The link drops
        realtime that would wait past LATE packet by packet: a frame's tail
        lost, a keyframe's above all, and every frame after it broken. A
        frame that would not fit whole is dropped here, and those after it
        until a keyframe (decoded against one the app never had, they would
        show garbage: it sees no frame missing that never came at all); the
        keyframe is made as soon as it fits (_keyframe_in). The app keeps
        the last picture meanwhile, and its frame numbers run on. A
        keyframe's size is kept (_key_worth), and it goes only if all of it
        fits behind what waits in the link now (_key_fits); the link's
        count of what it dropped as it went, for force_keyframe. Before
        l36, every frame sent."""
        now = time.monotonic()
        payload = getattr(self.channel, "payload", PAYLOAD)
        count = -(-len(data) // payload)
        size = len(data) + count * PACKET_EXTRA
        on = self.l36.on
        if on and self._broken and not keyframe:
            self.dropped["broken"] += 1
            return
        budget = self._budget_kbps
        if on and keyframe:
            self._key_worths.append(size * self.fps / (self.kbps * 125))
        if on and budget:
            if keyframe:
                fits = self._key_fits(size, self.channel.queued(), budget, self.kbps)
            else:
                # What waited as it went in, if that leaves room to spare (the
                # frame comes out before the next goes in); else asked again.
                clear = self._queued + size + ALONE <= self._holds(budget)
                fits = self._fits(size, self._queued if clear else self.channel.queued(), budget)
            if not fits:
                self.dropped["late"] += 1
                self._broken = self._key_wanted = True
                return
        if on and keyframe:
            self._key_dropped = self.channel.dropped() if hasattr(self.channel, "dropped") else None
        number = self._frame = self._frame + 1
        if number == 1 and self.input is not None:
            # The monitor is Mutter's now (the pen is mapped onto it): the
            # pen made ahead of the first stroke, not on the listening thread.
            self.input.prepare()
        self._keyframes += keyframe
        # Over the link, each packet told its frame (the frame's number, a
        # keyframe, its last): on VIDEO, the frame a group of its own.
        in_frame = getattr(self.channel, "send_in_frame", None)
        for index in range(count):
            header = VIDEO_HEADER.pack(VIDEO, int(keyframe), number, index, count, captured)
            packet = header + data[index * payload:(index + 1) * payload]
            if in_frame is None:
                self.channel.send(packet)
            else:
                in_frame(packet, number, keyframe, index == count - 1)
        self.allowance.take(size, now)
        if keyframe:  # whatever made it (asked, a bitrate change, the periodic one): it answers the app
            self._key_at, self._after_key = now, 0
            if on:
                self._last_keyframe = max(self._last_keyframe, now)
                self._key_size = size
            self._broken = self._key_wanted = False
        else:
            self._after_key += size
        self._encoded.append((time.monotonic_ns() - captured) / 1e6)

    def _listen(self):
        while self._running:
            # Input.check() every 20 ms while something touches or waits (a
            # first finger waits 80 ms at most for a second); else seldom.
            busy = self.input is not None and self.input.busy()
            body = self.channel.receive(0.02 if busy else IDLE_WAIT)
            if self.gated and self.input and not self._let_go:
                self.input.let_go()  # whatever touches, let go (within 20 ms while it touched)
            if not self.active and self.input and self.input.pen:
                self.input.close()  # another user's session in front: the pen's device too (SEC-22)
            self._let_go = self.gated
            if self.input:
                self.input.check()
            self._tick()
            if not body:
                continue
            kind = body[0]
            if kind == INPUT:
                if self.gated:
                    continue  # dropped, not queued
                if self.input and not self.input.packet(body):
                    log("app display: a malformed input packet")
            elif kind == KEYFRAME:
                log("app display: the app asks for a keyframe")
                self.force_keyframe()
            elif kind == PING and len(body) in (9, 10):
                self.channel.send(struct.pack("!Bqq", PONG, struct.unpack_from("!q", body, 1)[0],
                                              time.monotonic_ns()))
                if len(body) == 10:
                    self.visible(bool(body[9]))
            elif kind == STATS and len(body) == STATS_FORMAT.size:
                _kind, shown, lost, *ms = STATS_FORMAT.unpack(body)
                encoded, self._encoded = sorted(self._encoded), []
                left_out, self.skipped = self.skipped, collections.Counter()
                skipped = sum(left_out.values())
                budget = self.channel.budget_kbps() if hasattr(self.channel, "budget_kbps") else None
                others = self._others = self.channel.others_kbps() if hasattr(self.channel, "others_kbps") else 0
                keyframes, self._keyframes = self._keyframes, 0
                asks_held, self._asks_held = self._asks_held, 0
                keys_lost, self._keys_lost = self._keys_lost, 0
                dropped, self.dropped = self.dropped, collections.Counter()
                with self._rate_lock:
                    if budget:
                        self._follow(budget, others)  # the others' share moves
                    kbps = self.rate.report(shown, lost, ms[0] / 10, ms[3] / 10, skipped, budget, others)
                    if kbps is not None:
                        self._bitrate(kbps, self.rate.why)
                self.on_stats({"shown": shown, "lost": lost,
                               "capture_to_decoded": [v / 10 for v in ms[:3]],
                               "whole_to_decoded": [v / 10 for v in ms[3:]],
                               "capture_to_sent": encoded[len(encoded) // 2] if encoded else None,
                               "skipped": skipped, "left_out": dict(left_out), "kbps": self.rate.kbps,
                               "keyframes": keyframes, "asks_held": asks_held, "keys_lost": keys_lost,
                               "dropped": dict(dropped)})

    def visible(self, shown):
        """The app shows the desktop, or not: nothing is captured or encoded
        while it does not (Mutter's monitor stays, windows and all)."""
        with self._state_lock:
            if shown == self.shown:
                return
            self.shown = shown
            self._capture()
        if not shown and self.input:
            self.input.let_go()  # nothing left touching
        if shown:
            self.force_keyframe()
        self.on_visible(shown)

    def gate(self, closed, active=True):
        """The desktop may not be touched now (seat.py): the app's input is
        dropped, not queued, and the listening thread lets go of whatever
        touches; inactive (another user's session in front), nothing is
        captured either, and the pen's device is closed."""
        was, self.gated = self.gated, closed
        if was and not closed and self.input and self._frame:
            self.input.prepare()  # closed if another user's session was in front: ready again before it comes
        with self._state_lock:
            if active == self.active:
                return
            self.active = active
            self._capture()
            resumed = active and self.shown
        if resumed:
            self.force_keyframe()

    def _capture(self):
        """Under _state_lock: captured only while the app shows the desktop
        and the seat is ours, whichever of the two changed last."""
        self.pipeline.set_state(Gst.State.PLAYING if self.shown and self.active else Gst.State.PAUSED)

    def force_keyframe(self):
        # Not again before the last one can have come: a round trip, and its
        # own time on the way (a keyframe is up to VBV_FRAMES frames' worth).
        # The app asks at each frame it cannot decode; each answered was one
        # more keyframe into the queue the first one waits in (seen on a slow
        # home Wi-Fi: lost, asked, more lost, asked again every second). Nor
        # while the last one, whatever made it, still waits to go out (a
        # bitrate change's, say: tonight's live test asked within 3 s of a
        # cut 67 times of 73); the app asks again if it still needs one.
        # With l36, the keyframe's own time is the last one's at the budget,
        # if longer (one three times the reckoning was answered every 270 ms
        # live); and one the link cut, asked for again soon after, was lost:
        # the next smaller, alone (_lost_keyframe), not the same again.
        if self._key_flying:
            self._asks_held += 1
            return
        now = time.monotonic()
        rtt = self.channel.rtt() if hasattr(self.channel, "rtt") else None
        if self.l36.on:
            gap = max(MIN_KEYFRAME_GAP, 1.5 * (rtt or 0) + max(media.VBV_FRAMES / self.fps, self._key_time()))
        else:
            gap = max(MIN_KEYFRAME_GAP, 1.5 * rtt + media.VBV_FRAMES / self.fps) if rtt else MIN_KEYFRAME_GAP
        if now - self._last_keyframe < gap:
            return
        self._last_keyframe = now
        if self.l36.on:
            if self._cut(now, rtt or 0):
                self._lost_keyframe(now)
            self._key_wanted = True  # with the first frame in that it fits with
        else:
            self.keyframes.request()

    def _cut(self, now, rtt):
        """Whether the last keyframe sent never came whole: the link dropped
        realtime since it went (LinkPackets.dropped), and the app asks again
        within the gap it keeps between asks (MIN_KEYFRAME_GAP, as
        DisplaySession.kt's KEYFRAME_AGAIN), the keyframe's time on its way
        out and two round trips. (An ask alone may be for a frame lost on the
        air after it: a halving each time cost the bitrate for nothing.)
        Each read once: the streaming thread sets them as a keyframe goes
        (the count None if the link did not answer)."""
        key_at, before = self._key_at, self._key_dropped
        if key_at is None or before is None:
            return False
        if now > key_at + MIN_KEYFRAME_GAP + self._key_time() + 2 * rtt:
            return False
        dropped = self.channel.dropped()
        return dropped is not None and dropped > before

    def _lost_keyframe(self, now):
        """The last keyframe sent was cut: the next made at KEY_BACKOFF of
        the bitrate (on Intel's the change is that keyframe; up again
        UP_AFTER on, and only as far as its keyframe fits, _key_room) and
        alone: the frames after it left out until it is out (_left_out)."""
        self._keys_lost += 1
        self._key_dropped = None  # (once)
        self._alone_from = now
        with self._rate_lock:
            why = "a keyframe the link cut, asked for again"
            self._bitrate(self.rate.lower(min(int(self.kbps * KEY_BACKOFF), self.rate.kbps), why, now), why)


def _left_out(stats):
    causes = stats.get("left_out") or {}
    return ", ".join(f"{n} {LEFT_OUT.get(why, why)}" for why, n in sorted(causes.items())) or "the network behind"


def _dropped(stats):
    return ", ".join(f"{n} {DROPPED.get(why, why)}" for why, n in sorted(stats["dropped"].items()))


def describe(stats):
    decoded = stats["capture_to_decoded"]
    sent = stats["capture_to_sent"]
    return (f"{stats['shown']} fps, lost {stats['lost']}; capture to decoded {decoded[0]:.1f} ms "
            f"(90%: {decoded[1]:.1f}, max {decoded[2]:.1f}); of which capture to sent "
            f"{sent if sent is None else round(sent, 1)} ms, first packet to decoded {stats['whole_to_decoded'][0]:.1f} ms"
            + (f"; {stats['skipped']} not encoded ({_left_out(stats)})" if stats.get("skipped") else "")
            + (f"; {stats['kbps']} kbit/s" if "kbps" in stats else "")
            + (f"; {stats['keyframes']} keyframe{'s' if stats['keyframes'] > 1 else ''}" if stats.get("keyframes") else "")
            + (f"; {stats['asks_held']} asked while one was on its way" if stats.get("asks_held") else "")
            + (f"; {stats['keys_lost']} cut by the link, the next smaller" if stats.get("keys_lost") else "")
            + (f"; {sum(stats['dropped'].values())} encoded not sent ({_dropped(stats)})" if stats.get("dropped") else ""))


class AppDisplay:
    """A display session with the paired tablet's app: the virtual monitor
    (or a test pattern), its stream, the fingers and the pen, for as long as
    the app shows it. mode: (width, height, fps), the size None for the one
    closest to 1920x1200 at the tablet's proportions (also when the size
    asked for is not at them: another tablet). Callbacks run on the main
    loop: on_started((width, height, fps), tablet's screen),
    on_ended(error or None, started), on_paused(paused)."""

    def __init__(self, targets, app_id, mode, bitrate, encoder, touch=True, test=False, preview=False,
                 on_started=None, on_ended=None, on_paused=None):
        self.targets, self.app_id, self.mode = targets, app_id, mode
        self.bitrate, self.encoder, self.touch, self.test = bitrate, encoder, touch, test
        self.preview = preview
        self.on_started = on_started or (lambda mode, screen: None)
        self.on_ended = on_ended or (lambda error, started: None)
        self.on_paused = on_paused or (lambda paused: None)
        self.channel = self.monitor = self.streamer = self.link = None
        self.tablet_size = None  # what the app said its screen is
        self.started = False
        self.gated, self.seat_active = False, True  # seat.py's, for the streamer
        self._reports = 0
        self._stopped = threading.Event()

    def start(self):
        threading.Thread(target=self._run, daemon=True).start()

    def gate(self, closed, active=True):
        """On the main loop: seat.py's say (AppStreamer.gate), now and for
        a streamer made later."""
        self.gated, self.seat_active = closed, active
        if self.streamer is not None:
            self.streamer.gate(closed, active)

    def stop(self):
        """On the main loop: everything ends now; the app is told by the
        link's thread (it sends "stop" and closes)."""
        if self._stopped.is_set():
            return
        self._stopped.set()
        self._teardown()

    # -- the link's thread ------------------------------------------------------
    def _over_the_link(self):
        """The session over the link (plus-one-link), when it runs and the
        app serves the desktop on it: the channels, its answer; else None,
        for the link of before."""
        if plusone_link is None:
            return None
        try:
            connection = plusone_link.Link()
        except plusone_link.LinkError:
            return None
        channels = []
        try:
            device = next((d["id"] for d in connection.devices() if d["id"].startswith(self.app_id)), None)
            if device is None:
                raise plusone_link.LinkError("not a paired tablet")
            # PLUSONE_LINK_VIDEO_STREAMS=0 (the live A/B's way before q3,
            # plus-one-link's switch too): the media channel a realtime one,
            # its packets exactly as before VIDEO (no head, the payload whole).
            media = (plusone_link.REALTIME if os.environ.get("PLUSONE_LINK_VIDEO_STREAMS", "1").strip() == "0"
                     else plusone_link.VIDEO)
            for name, kind in (("control", plusone_link.CONTROL), ("media", media),
                               ("input", plusone_link.INPUT)):
                try:
                    channels.append(connection.open(device, "display", name, kind))
                except plusone_link.LinkError as e:
                    if kind != plusone_link.VIDEO:
                        raise
                    # A plus-one-link from before VIDEO: the frames as realtime, as before it.
                    log("app display: the frames as realtime, not video:", e)
                    channels.append(connection.open(device, "display", name, plusone_link.REALTIME))
            packets = LinkPackets(connection, device, *channels)
            packets.control.send(json.dumps({"type": "display"}).encode())
            answer = packets.control.recv(timeout=10)
            reply = json.loads(answer) if answer else {}
            if not isinstance(reply, dict):
                raise ValueError(f"the app answered {answer[:80]!r}")
        except Exception as e:  # noqa: BLE001 - whatever it was, nothing left open
            for channel in channels:
                channel.close()
            connection.close()
            if not isinstance(e, plusone_link.LinkError):
                raise
            log("app display: not over the link:", e)
            return None
        return packets, reply

    def _run(self):
        error = None
        try:
            over_link = self._over_the_link()
            if over_link is not None:
                self._run_over_the_link(*over_link)
                return
            self.link = link.connect(self.targets, self.app_id)
            if self._stopped.is_set():
                return
            self.channel = Datagrams()
            if self._stopped.is_set():
                return  # stop() came meanwhile: its teardown may not have seen the channel
            reply = self.link.request({"type": "display", "key": self.channel.key.hex(), "port": self.channel.port})
            if reply.get("type") != "display-ready":
                raise ValueError(f"the app answered {reply.get('type')!r}")
            address = self.link.peer
            self.channel.app = (address, int(reply["port"]))
            self.tablet_size = (int(reply["width"]), int(reply["height"]))
            log(f"app display: {address} shows {self.tablet_size[0]}x{self.tablet_size[1]}"
                f" at {float(reply.get('refresh', 0)):.0f} Hz")
            width, height, fps = self.mode
            if (width, height) not in sizes(self.tablet_size):
                width, height = auto_size(self.tablet_size)
            self.mode = (width, height, fps)
            GLib.idle_add(once(self._open))
            # Until the app hangs up (Back on the tablet), is gone, or stop().
            self.link.hold(until=self._stopped.is_set)
            try:
                self.link.send({"type": "stop"})
            except OSError:
                pass
        except Exception as e:  # noqa: BLE001 - not there, refused, or gone: said to the caller
            error = e
        finally:
            if self.link is not None:
                self.link.close()
            if self._stopped.is_set() and self.channel is not None:
                self.channel.close()  # stopped before teardown could see it (twice is harmless)
            if not self._stopped.is_set():
                GLib.idle_add(once(self._ended, error))

    def _run_over_the_link(self, packets, reply):
        """The session on the link's channels, as _run's (its errors raised
        to it), the control channel held instead of the link of before."""
        self.channel = packets
        if self._stopped.is_set():
            return
        if reply.get("type") == "refused":
            raise link.Refused(str(reply.get("reason", "")))  # why_not's guidance, as over the link of before
        if reply.get("type") != "display-ready":
            raise ValueError(f"the app answered {reply.get('type') or reply.get('reason')!r}")
        self.tablet_size = (int(reply["width"]), int(reply["height"]))
        log(f"app display: over the link, {self.tablet_size[0]}x{self.tablet_size[1]}"
            f" at {float(reply.get('refresh', 0)):.0f} Hz")
        width, height, fps = self.mode
        if (width, height) not in sizes(self.tablet_size):
            width, height = auto_size(self.tablet_size)
        self.mode = (width, height, fps)
        GLib.idle_add(once(self._open))
        # Until the app ends it (its control channel's end), or stop().
        while not self._stopped.is_set():
            try:
                if packets.control.recv(timeout=0.5) == b"":
                    return
            except (OSError, ValueError):
                return

    # -- the main loop ---------------------------------------------------------
    def _open(self):
        if self._stopped.is_set():
            return
        if self.test:
            self._stream("test")
            return
        from .display import VirtualMonitor
        self.monitor = VirtualMonitor(on_ready=self._stream,
                                      on_closed=lambda: self._ended(RuntimeError("the screen sharing was stopped")))
        try:
            self.monitor.start()
        except GLib.Error as e:
            self._ended(e)

    def _stream(self, source):
        if self._stopped.is_set() or self.streamer is not None:
            return
        width, height, fps = self.mode
        touch = None
        if self.touch and self.monitor is not None:
            monitor = self.monitor
            touch = Input(monitor, (width, height), lambda: pen.for_monitor(monitor, (width, height)))
        try:
            self.streamer = AppStreamer(source, self.channel, width, height, fps,
                                        bitrate_for(self.bitrate, (width, height)), self.encoder,
                                        on_stats=self._stats,
                                        input=touch,
                                        on_visible=lambda shown: GLib.idle_add(once(self.on_paused, not shown)),
                                        preview=self.preview,
                                        on_error=lambda message: self._ended(RuntimeError(message)))
            self.streamer.gate(self.gated, self.seat_active)
            self.streamer.start()
        except Exception as e:  # noqa: BLE001 - said, not left half-started
            self._ended(e)
            return
        self.started = True
        if self.monitor is not None and scale_for((width, height)) != 1:
            GLib.timeout_add(300, self._scale, 5)
        self.on_started(self.mode, self.tablet_size)

    def _stats(self, stats):
        """From the listening thread: the app's report of the last second."""
        self._reports += 1
        # Each second something went wrong, or a keyframe went (the live
        # tests follow the chain: a cut, frames not encoded, lost, asked for).
        if (stats["lost"] or stats.get("skipped") or stats.get("keyframes") or stats.get("dropped")
                or stats.get("keys_lost") or self._reports % STATS_EVERY == 1):
            log("app display:", describe(stats))

    def _scale(self, tries):
        """The virtual monitor appears in the display configuration a
        moment after its stream: a few tries."""
        if self._stopped.is_set():
            return False
        from .display import set_virtual_monitor_scale
        try:
            if set_virtual_monitor_scale(scale_for(self.mode[:2])):
                return False
        except GLib.Error as e:
            log("display: scale not set:", e.message)
            return False
        if tries > 1:
            GLib.timeout_add(500, self._scale, tries - 1)
        return False

    def _ended(self, error=None):
        if self._stopped.is_set():
            return
        self._stopped.set()
        self._teardown()
        self.on_ended(error, self.started)

    def _teardown(self):
        if self.streamer is not None:
            self.streamer.stop()
            self.streamer = None
        if self.monitor is not None:
            self.monitor.stop()
            self.monitor = None
        if self.channel is not None:
            self.channel.close()


def main(argv=None):
    """The desktop (or a test pattern) to the paired tablet's app, with its
    fingers and pen, its delay measured."""
    ap = argparse.ArgumentParser(prog="python3 -m plusone.appdisplay")
    ap.add_argument("address", help="the tablet's address on the network")
    ap.add_argument("--port", type=int, default=link.APP_PORT)
    ap.add_argument("--size", default="auto", help="WxH at the tablet's proportions, or auto")
    ap.add_argument("--fps", type=int, default=60)
    ap.add_argument("--bitrate", type=int, default=20000, help="kbit/s")
    ap.add_argument("--encoder", default="auto")
    ap.add_argument("--seconds", type=int, default=20)
    ap.add_argument("--test", action="store_true", help="a test pattern instead of a virtual monitor")
    ap.add_argument("--preview", action="store_true", help="the test pattern in a window here too, to photograph")
    args = ap.parse_args(argv)
    Gst.init(None)
    paired = link.Paired().all()
    if len(paired) != 1:
        raise SystemExit("pair exactly one tablet first")
    width, height = (None, None) if args.size == "auto" else map(int, args.size.split("x"))
    loop = GLib.MainLoop()
    session = AppDisplay([(args.address, args.port)], next(iter(paired))[:16], (width, height, args.fps),
                         args.bitrate, media.pick_encoder(args.encoder), test=args.test, preview=args.preview,
                         on_ended=lambda error, started: (log("app display: ended", repr(error)), loop.quit()))
    session.start()
    GLib.timeout_add_seconds(args.seconds, loop.quit)
    try:
        loop.run()
    except KeyboardInterrupt:
        pass
    finally:
        session.stop()
        time.sleep(1)  # the link's thread tells the app


if __name__ == "__main__":
    main()
