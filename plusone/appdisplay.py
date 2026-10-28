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
import select
import struct
import threading
import time

import gi

gi.require_version("Gst", "1.0")
from gi.repository import GLib, Gst  # noqa: E402

from . import link, log, media, once, pen  # noqa: E402
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
        self.pen = None
        self.pen_last = None  # the last report, while the pen is in range
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

    def close(self):
        if self.gesture is not None:
            self._touch({}, lost=True)
        if self.fling is not None:
            self._stop_fling()
        if self.pen_last is not None:
            self._pen(self.pen_last[:1] + b"\x00" + self.pen_last[2:])
        if self.pen:
            self.pen.close()
        self.pen = None

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
    def _pen(self, report):
        if self.pen is None:
            try:
                self.pen = self.make_pen()
            except Exception as e:  # noqa: BLE001 - no pen, the fingers still work
                log("app display: no pen:", repr(e))
                self.pen = False
        if self.pen:
            self.pen.report(report)
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
    own jitter), and it went down for nothing."""

    GROWN_MS = 25   # over the least: a queue
    CLEAN = 3       # reports in a row without loss nor queue before going up
    HISTORY = 10    # reports the least is taken over
    FRAMES = 10     # shown in the second, for its delay to count
    SKIPPED = 3     # frames not encoded in the second: the network behind
    NEAR = 0.85     # of the ceiling: up to here quickly
    PROBE = 1.03    # past it, up by this...
    PROBE_EVERY = 10  # ...after this many clean reports
    CEILING_FOR = 60  # reports (seconds) a ceiling holds

    def __init__(self, most):
        self.most = most
        self.kbps = most
        self.clean = 0
        self.ceiling = None  # where a queue last showed
        self.ceiling_age = 0  # reports since
        self.delays = collections.deque(maxlen=self.HISTORY)

    def report(self, shown, lost, capture_to_decoded, first_to_decoded, skipped=0):
        """A report, and the frames not encoded since the last; the new
        bitrate if it changed, else None."""
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
            kbps = self._set(int(self.kbps * self.PROBE))
            if self.kbps >= self.ceiling:
                self.ceiling = None  # it takes as much now
            return kbps
        if self.clean >= self.CLEAN:
            self.clean = 0
            up = self.kbps * 11 // 10 + 100
            if self.ceiling:
                up = min(up, int(self.ceiling * self.NEAR))
            return self._set(max(up, self.kbps + 1))
        return None

    def _set(self, kbps):
        kbps = max(MIN_KBPS, min(self.most, kbps))
        if kbps == self.kbps:
            return None
        self.kbps = kbps
        return kbps


class LinkPackets:
    """The desktop's packets over the link (plus-one-link, docs/LINK.md),
    as Datagrams carries them over the link of before: VIDEO, PING and PONG
    out and KEYFRAME, PONG and STATS in on a realtime channel, INPUT in on
    an input channel (the newest state kept, never queued); queued() is
    plus-one-link's backlog to the tablet."""

    def __init__(self, connection, device, control, media, input):
        self.connection, self.device = connection, device
        self.control, self.media, self.input = control, media, input
        self.payload = plusone_link.MAX_DATAGRAM - VIDEO_HEADER.size
        self._open = [media, input]
        self.errors = set()

    def send(self, body):
        try:
            self.media.send(body)
        except OSError as e:
            if e.errno not in self.errors:
                self.errors.add(e.errno)
                log("app display: cannot send:", e.strerror)

    def queued(self):
        try:
            return self.connection.queued(self.device)
        except (OSError, ValueError, plusone_link.LinkError):
            return 0

    def rtt(self):
        """The session's round trip (seconds), or None if not known."""
        try:
            device = next((d for d in self.connection.devices() if d["id"] == self.device), None)
        except (OSError, ValueError, plusone_link.LinkError):
            return None
        ms = device.get("rtt_ms") if device else None
        return ms / 1000 if ms else None

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
        element, options, raw_format = media.ENCODERS[encoder]
        enc = f"{element} name=enc " + options.format(bitrate=bitrate, gop=fps * media.KEYFRAME_SECONDS)
        convert = media.CONVERT.get(encoder, f"videoconvert n-threads=4 ! video/x-raw,format={raw_format}")
        self.pipeline = Gst.parse_launch(
            f"{video} ! queue max-size-buffers=1 leaky=downstream ! {convert} ! "
            f"{enc} ! video/x-h264,profile=high ! h264parse config-interval=-1 ! "
            f"video/x-h264,stream-format=byte-stream,alignment=au ! "
            f"appsink name=out emit-signals=true sync=false async=false max-buffers=0")
        enc = self.enc = self.pipeline.get_by_name("enc")
        media.cap_keyframes(enc, bitrate, fps)
        self.fps = fps
        self.rate = RateControl(bitrate)
        self.skipped = 0  # frames not encoded: the network behind
        # When each frame going into the encoder was captured, in order: one
        # comes out per one in (no B frames), and a flush drops those held.
        # (Its output timestamps could not be traced back after a flush.)
        self._captured = collections.deque()
        self.keyframes = media.Keyframes(enc, on_flush=self._captured.clear)
        self._in_pad = enc.get_static_pad("sink")
        self._in_probe = self._in_pad.add_probe(Gst.PadProbeType.BUFFER, self._frame_in)
        self._sink = self.pipeline.get_by_name("out")
        self._handler = self._sink.connect("new-sample", self._send)
        bus = self.pipeline.get_bus()
        bus.add_signal_watch()
        self._bus_handler = bus.connect("message::error", self._failed)
        self._frame = 0
        self._last_keyframe = 0.0
        self._encoded = []  # ms from capture to sent, since the last stats
        self._running = True
        self._feedback = threading.Thread(target=self._listen, daemon=True)
        to = f"{channel.app[0]}:{channel.app[1]}" if isinstance(channel, Datagrams) else "the link"
        log(f"app display: {width}x{height}@{fps} {encoder} {bitrate} kbit/s -> {to}")

    def start(self):
        self.pipeline.set_state(Gst.State.PLAYING)
        self._feedback.start()

    def stop(self):
        self._running = False
        self._feedback.join(timeout=2)
        if self.input:
            self.input.close()
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
        # The network behind (more waiting to go out than QUEUE_MS of the
        # bitrate): this frame is not encoded, rather than queued after the
        # others (the delay would grow, and grow). A frame that never went
        # in breaks nothing; the next one is encoded against the last.
        if self.channel.queued() > max(64 << 10, self.rate.kbps * QUEUE_MS // 8 * TRUESIZE):
            self.skipped += 1
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

    def _send(self, appsink):
        """Streaming thread: one frame, out at once."""
        sample = appsink.emit("pull-sample")
        if sample is None:
            return Gst.FlowReturn.EOS
        buf = sample.get_buffer()
        data = buf.extract_dup(0, buf.get_size())
        captured = self._captured.popleft() if self._captured else time.monotonic_ns()
        keyframe = not buf.has_flags(Gst.BufferFlags.DELTA_UNIT)
        payload = getattr(self.channel, "payload", PAYLOAD)
        count = -(-len(data) // payload)
        number = self._frame = self._frame + 1
        for index in range(count):
            header = VIDEO_HEADER.pack(VIDEO, int(keyframe), number, index, count, captured)
            self.channel.send(header + data[index * payload:(index + 1) * payload])
        self._encoded.append((time.monotonic_ns() - captured) / 1e6)
        return Gst.FlowReturn.OK

    def _listen(self):
        while self._running:
            # Input.check() every 20 ms while something touches or waits (a
            # first finger waits 80 ms at most for a second); else seldom.
            busy = self.input is not None and self.input.busy()
            body = self.channel.receive(0.02 if busy else IDLE_WAIT)
            if self.input:
                self.input.check()
            if not body:
                continue
            kind = body[0]
            if kind == INPUT:
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
                skipped, self.skipped = self.skipped, 0
                before = self.rate.kbps
                kbps = self.rate.report(shown, lost, ms[0] / 10, ms[3] / 10, skipped)
                if kbps is not None:
                    self.enc.set_property("bitrate", kbps)
                    media.cap_keyframes(self.enc, kbps, self.fps)
                self.on_stats({"shown": shown, "lost": lost,
                               "capture_to_decoded": [v / 10 for v in ms[:3]],
                               "whole_to_decoded": [v / 10 for v in ms[3:]],
                               "capture_to_sent": encoded[len(encoded) // 2] if encoded else None,
                               "skipped": skipped, "kbps": self.rate.kbps,
                               "changed": kbps is not None and (before, kbps)})

    def visible(self, shown):
        """The app shows the desktop, or not: nothing is captured or encoded
        while it does not (Mutter's monitor stays, windows and all)."""
        if shown == self.shown:
            return
        self.shown = shown
        if not shown and self.input:
            self.input.close()  # nothing left touching
        self.pipeline.set_state(Gst.State.PLAYING if shown else Gst.State.PAUSED)
        if shown:
            self.force_keyframe()
        self.on_visible(shown)

    def force_keyframe(self):
        # Not again before the last one can have come: a round trip, and its
        # own time on the way (a keyframe is up to VBV_FRAMES frames' worth).
        # The app asks at each frame it cannot decode; each answered was one
        # more keyframe into the queue the first one waits in (seen on a slow
        # home Wi-Fi: lost, asked, more lost, asked again every second).
        now = time.monotonic()
        rtt = self.channel.rtt() if hasattr(self.channel, "rtt") else None
        gap = max(MIN_KEYFRAME_GAP, 1.5 * rtt + media.VBV_FRAMES / self.fps) if rtt else MIN_KEYFRAME_GAP
        if now - self._last_keyframe < gap:
            return
        self._last_keyframe = now
        self.keyframes.request()


def describe(stats):
    decoded = stats["capture_to_decoded"]
    sent = stats["capture_to_sent"]
    return (f"{stats['shown']} fps, lost {stats['lost']}; capture to decoded {decoded[0]:.1f} ms "
            f"(90%: {decoded[1]:.1f}, max {decoded[2]:.1f}); of which capture to sent "
            f"{sent if sent is None else round(sent, 1)} ms, first packet to decoded {stats['whole_to_decoded'][0]:.1f} ms"
            + (f"; {stats['skipped']} not encoded (the network behind)" if stats.get("skipped") else "")
            + (f"; {stats['kbps']} kbit/s" if "kbps" in stats else ""))


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
        self._reports = 0
        self._stopped = threading.Event()

    def start(self):
        threading.Thread(target=self._run, daemon=True).start()

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
            for name, kind in (("control", plusone_link.CONTROL), ("media", plusone_link.REALTIME),
                               ("input", plusone_link.INPUT)):
                channels.append(connection.open(device, "display", name, kind))
        except plusone_link.LinkError as e:
            log("app display: not over the link:", e)
            for channel in channels:
                channel.close()
            connection.close()
            return None
        packets = LinkPackets(connection, device, *channels)
        packets.control.send(json.dumps({"type": "display"}).encode())
        answer = packets.control.recv(timeout=10)
        reply = json.loads(answer) if answer else {}
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
        if stats.get("changed"):
            before, now = stats["changed"]
            log(f"app display: {before} -> {now} kbit/s, as the network takes")
        if stats["lost"] or stats.get("skipped") or self._reports % STATS_EVERY == 1:
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
