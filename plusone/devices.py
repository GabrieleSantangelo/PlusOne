"""The tablet's camera, microphone and speakers as the PC's own: a webcam
("Plus One tablet camera", v4l2loopback's devices), a microphone ("Tablet
microphone", a virtual PipeWire source Plus One feeds) and an output
("Tablet speakers", a virtual sink Plus One plays from), each switched on in
the window. Nothing goes over the network until a program on the PC uses one:
then the tablet in use is asked for it, over WebRTC (rtc.py) for the camera
and the microphone, as plain PCM (pcm.py) for the speakers, and lets go of it
when the program does.
"""
import ctypes
import functools
import importlib.util
import glob
import itertools
import json
import os
import secrets
import shutil
import struct
import subprocess
import threading
import time
import types

import gi

gi.require_version("Gst", "1.0")
from gi.repository import Gio, GLib, Gst  # noqa: E402

from . import link, log, once  # noqa: E402
from .link import channels  # noqa: E402

CAMERA_LABEL = "Plus One tablet camera"  # data/plus-one-camera-options.conf
MIC_NAME = "plusone_mic"
SPEAKERS_NAME = "plusone_speakers"
QUALITIES = {"720p": (1280, 720, 2_500_000), "1080p": (1920, 1080, 5_000_000)}
FPS = 30
AGAIN_AFTER_EVENT = 500  # ms: a stream's pause or play, looked at again (AudioDevice._event)
RETRY = 3          # seconds between attempts to reach the tablet while wanted
REFUSED_RETRY = 10  # seconds, after the app said no (the user may change their mind there)
IN_USE_AFTER = 0.2  # seconds a program holds the device before the tablet is asked
IDLE_AFTER = 3      # seconds without one before the tablet stops
SILENT_FOR = 5      # seconds of a session without a packet before it is made again
LIVE_GAP = 0.5      # seconds without the tablet's picture before the webcam shows black
MIC_FEED = "plusone-mic-feed"  # the streams Plus One feeds the microphone with: MIC_FEED-<n>
# ns of sound that may wait before the oldest goes: what waits stays, as delay,
# once the sound comes as fast as it is played (160 ms measured 235 end to end).
MIC_BEHIND = 60_000_000
NOISE = ("off", "light", "neural")
# LADSPA plugins GStreamer finds (~/.ladspa, /usr/lib/ladspa...), by the end
# of their elements' names. DeepFilterNet's (MIT or Apache-2.0, not packaged:
# libdeep_filter_ladspa.so from its releases, in ~/.ladspa) takes the room's
# noise out of a voice with a neural network, 20 ms late, about half a core
# while a program records (measured). swh-plugins' compressor and limiter
# (Ubuntu's package).
DEEP_FILTER = "-deep-filter-mono"
COMPRESSOR = "-sc4m"
LIMITER = "-fastlookaheadlimiter"
# The level, from about -27 dBFS (libwebrtc's gain control on Android is a
# fixed +9 dB, the tablet a meter away) to about -18, as calls want: raised,
# a compressor that evens out loud and soft (RMS, 3:1 over -30 dB, 10 ms in,
# 300 ms out) and a limiter that keeps the peaks under -1 dBFS (it works on
# two channels: the one, twice). Measured: -17 to -19 dBFS, nothing over -1.
LEVEL = ("volume volume=3.98 "  # +12 dB
         "! {compressor} rms-peak=0 attack-time=10 release-time=300 threshold-level=-30 ratio=3 "
         "knee-radius=6 makeup-gain=9 "
         "! audioconvert ! audio/x-raw,format=F32LE,channels=2 ! {limiter} limit=-1 release-time=0.1 "
         "! audioconvert ! audio/x-raw,format=F32LE,channels=1")
# Without swh-plugins: a gain under which the voice's peaks stay under full
# scale (audiodynamic is no compressor: it bends each sample over its threshold).
PLAIN_LEVEL = "volume volume=2.0"  # +6 dB
# Software first: little work at these sizes, and decodebin's pick (NVIDIA's,
# here) held the whole pipeline still until the tablet's first frame.
H264_DECODERS = ("avdec_h264", "openh264dec", "vah264dec")


@functools.lru_cache(maxsize=None)
def ladspa(suffix):
    """The element of the LADSPA plugin whose element name ends so, or None
    (once per run: a scan of the whole registry)."""
    for factory in Gst.Registry.get().get_feature_list(Gst.ElementFactory):
        name = factory.get_name()
        if name.startswith("ladspa-") and name.endswith(suffix):
            return name
    return None


def neural_available():
    """Whether DeepFilterNet's plugin is there."""
    Gst.init(None)  # once is enough, again is harmless
    return ladspa(DEEP_FILTER) is not None


def voice(noise):
    """What the tablet's sound goes through (F32, mono, 48 kHz in and out),
    and the noise reduction it gets: "neural" only with DeepFilterNet's
    plugin, else the tablet's ("light")."""
    deep_filter = ladspa(DEEP_FILTER) if noise == "neural" else None
    if noise == "neural" and deep_filter is None:
        noise = "light"
    compressor, limiter = ladspa(COMPRESSOR), ladspa(LIMITER)
    level = LEVEL.format(compressor=compressor, limiter=limiter) if compressor and limiter else PLAIN_LEVEL
    # The network on a thread of its own: should it fall behind, the sound it
    # cannot take goes, not the packets webrtcbin's jitter buffer hands on.
    # The network's own thresholds (libDF's defaults; the plugin's let every
    # stage run on every frame): silence and clean speech cost less.
    neural = (f"queue max-size-time=100000000 max-size-buffers=0 max-size-bytes=0 leaky=downstream "
              f"! {deep_filter} min-processing-threshold=-10 max-erb-processing-threshold=30 "
              f"max-df-processing-threshold=20")
    return (f"{neural} ! {level}" if deep_filter else level), noise


def h264_decoder():
    return next((name for name in H264_DECODERS if Gst.ElementFactory.find(name)), None)


def webrtc():
    """rtc.py, or None if this PC lacks WebRTC's pieces (webrtcbin, libnice,
    the GstWebRTC bindings): the devices then say what is missing."""
    try:
        from . import rtc
    except (ImportError, ValueError):
        return None
    Gst.init(None)  # once is enough, again is harmless
    return rtc if Gst.ElementFactory.find("webrtcbin") and Gst.ElementFactory.find("nicesrc") else None


class Feed:
    """One device's session with the tablet in use, on a thread of its own
    while wanted, made again when it drops: receiver() makes the device's
    rtc.Receiver (or pcm.PcmSender), whose offer goes over the paired link
    with the request; on_ready() once the app answered; on_problem(reason:
    "off", "permission", "background", "unavailable", "away")."""

    def __init__(self, request, where, receiver, on_ready, on_problem):
        # One use (a program took the device): what the tablet's Stop holds
        # until the next (CaptureHandler.kt), whatever the requests again.
        self.request = dict(request, use=secrets.token_hex(4))
        self.where = where  # () -> (addresses, app id) of the tablet in use, or None
        self.receiver, self.on_ready, self.on_problem = receiver, on_ready, on_problem
        self._wanted = threading.Event()
        self._unwanted = threading.Event()  # its opposite, to wait on
        self._link = None
        self._thread = None

    def start(self):
        if self._wanted.is_set():
            return
        self._unwanted.clear()
        self._wanted.set()
        self._thread = threading.Thread(target=self._run, name=f"plusone-{self.request['type']}", daemon=True)
        self._thread.start()

    def stop(self):
        self._wanted.clear()
        self._unwanted.set()
        tablet, self._link = self._link, None
        if tablet is not None:
            tablet.shutdown()  # the session's thread wakes, and ends it

    def _wait(self, seconds):
        self._unwanted.wait(seconds)

    def _run(self):
        kind = self.request["type"]
        while self._wanted.is_set():
            delay = self._session(kind)
            if delay:
                self._wait(delay)  # after the session's link and webrtcbin are closed
        log(f"{kind}: the tablet stopped")

    def _session(self, kind):
        """One session with the tablet in use, until it ends: seconds to
        wait before the next, or None."""
        tablet = receiver = None
        try:
            where = self.where()
            if not where or not where[0]:
                self.on_problem("away")
                return RETRY
            # Over the link (plus-one-link) when it runs and the app serves
            # this there; else the link of before.
            tablet = self._link = channels.connect(where[1], kind) or link.connect(where[0], where[1])
            if not self._wanted.is_set():
                return None  # stopped while it connected: stop() had no link to close
            receiver = self.receiver()
            if hasattr(receiver, "peer"):
                receiver.peer = tablet.peer  # where pcm.PcmSender sends: the paired app's address
            if isinstance(tablet, channels.ChannelLink) and hasattr(receiver, "use_link"):
                receiver.use_link(tablet)  # its packets on the link's channels too
            reply = tablet.request(dict(self.request, sdp=receiver.offer()))
            if reply.get("type") != "answer":
                raise ValueError(f"the app answered {reply.get('type')!r}")
            receiver.answer(str(reply.get("sdp", "")))
            log(f"{kind}: to the tablet, as it is" if kind == "speakers" else f"{kind}: from the tablet, over WebRTC")
            self.on_ready()
            self._hold(kind, tablet, receiver)
            return None
        except link.Refused as e:
            log(f"{kind}: the tablet said no ({e})")
            self.on_problem(str(e))
            # Turned off or not allowed there: the user has to act; else soon.
            return REFUSED_RETRY if str(e) in ("off", "permission", "stopped") else RETRY
        except link.PairingRefused as e:
            # The tablet forgot this PC (or is another): pairing again is the user's.
            log(f"{kind}: the tablet does not know this PC ({e})")
            self.on_problem("unpaired")
            return REFUSED_RETRY
        except link.Incompatible as e:
            log(f"{kind}: {e}")
            self.on_problem("version")
            return REFUSED_RETRY
        except Exception as e:  # noqa: BLE001 - not there, not answering, or gone: again in a while
            if not self._wanted.is_set():
                return None
            log(f"{kind}: no tablet:", repr(e))
            self.on_problem("away")
            return RETRY
        finally:
            if receiver is not None:
                receiver.close()
            if tablet is not None:
                tablet.close()
            self._link = None

    def _hold(self, kind, tablet, receiver):
        """While wanted, the link up (link.hold) and the media flowing: the
        app hangs up by closing it, Plus One by closing it too."""
        said, heard, last = (0, 0, 0), 0, time.monotonic()

        def flowing():
            nonlocal said, heard, last
            problem = receiver.failed()
            if problem:
                raise ConnectionError(problem)
            received, *losses = receiver.losses()
            if received > heard:
                heard, last = received, time.monotonic()
            elif time.monotonic() - last > SILENT_FOR:
                # Connected, and yet nothing comes: made again.
                raise TimeoutError(f"nothing from the tablet for {SILENT_FOR} s")
            if tuple(losses) != said:
                said = tuple(losses)
                log(f"{kind}: {received} packets, lost %d, asked again for %d, asked for %d key frames" % said)

        tablet.hold(until=lambda: not self._wanted.is_set(), check=flowing, tick=1.0)


class Opened:
    """How many times a device file is open now, by anyone, from inotify's
    open and close events; on_change(count) on the main loop. baseline: how
    many of those are Plus One's own (started())."""
    IN_CLOSE_WRITE, IN_CLOSE_NOWRITE, IN_OPEN = 0x08, 0x10, 0x20

    def __init__(self, path, on_change):
        self.path = path
        self.on_change = on_change
        self.count = self.baseline = 0
        libc = ctypes.CDLL(None, use_errno=True)
        self.fd = libc.inotify_init1(os.O_NONBLOCK | os.O_CLOEXEC)
        if self.fd < 0:
            raise OSError(ctypes.get_errno(), "inotify")
        if libc.inotify_add_watch(self.fd, path.encode(), self.IN_OPEN | self.IN_CLOSE_WRITE
                                  | self.IN_CLOSE_NOWRITE) < 0:
            error = ctypes.get_errno()
            os.close(self.fd)
            raise OSError(error, "inotify")
        self.source = GLib.io_add_watch(GLib.IOChannel.unix_new(self.fd), GLib.PRIORITY_DEFAULT,
                                        GLib.IOCondition.IN, self._read)

    def drain(self):
        """The events waiting, counted now."""
        self._read(None, None)

    def started(self):
        """Plus One opened it: what is open now is Plus One's, and the
        programs' that had it open before the watch began (a Plus One
        restarted during a call), which inotify did not see open: counted
        now, or their closing would take the count below Plus One's own,
        and the next program's opening would not show."""
        self.drain()
        self.baseline = self.count
        self.count += holders(self.path)

    def _read(self, _channel, _condition):
        try:
            data = os.read(self.fd, 4096)
        except BlockingIOError:
            return True
        offset = 0
        while offset + 16 <= len(data):
            _wd, mask, _cookie, length = struct.unpack_from("iIII", data, offset)
            offset += 16 + length
            if mask & self.IN_OPEN:
                self.count += 1
            elif mask & (self.IN_CLOSE_WRITE | self.IN_CLOSE_NOWRITE):
                # Never below Plus One's own: one open before the watch, not
                # seen in /proc (another user's), closed.
                self.count = max(self.baseline, self.count - 1)
        self.on_change(self.count)
        return True

    def close(self):
        GLib.source_remove(self.source)
        os.close(self.fd)


def holders(path):
    """How many other processes have path open, as far as /proc shows."""
    count = 0
    for fds in glob.glob("/proc/[0-9]*/fd"):
        if fds == f"/proc/{os.getpid()}/fd":
            continue
        try:
            count += any(os.readlink(os.path.join(fds, fd)) == path for fd in os.listdir(fds))
        except OSError:
            pass  # gone, or not ours to look at
    return count


def holder_names(paths):
    """The names of the other processes that have one of paths open (as far
    as /proc shows): who uses the webcam, for the window."""
    names = set()
    for fds in glob.glob("/proc/[0-9]*/fd"):
        if fds == f"/proc/{os.getpid()}/fd":
            continue
        try:
            if any(os.readlink(os.path.join(fds, fd)) in paths for fd in os.listdir(fds)):
                with open(os.path.join(os.path.dirname(fds), "comm")) as f:
                    names.add(f.read().strip())
        except OSError:
            pass  # gone, or not ours to look at
    return sorted(names)


def camera_devices():
    """/dev/videoN of Plus One's v4l2loopback devices (data/plus-one-camera-options.conf):
    the one for programs that read cameras themselves, and the one for PipeWire."""
    found = []
    for entry in sorted(glob.glob("/sys/class/video4linux/video*")):
        try:
            with open(os.path.join(entry, "name")) as f:
                if f.read().strip().startswith(CAMERA_LABEL):
                    found.append("/dev/" + os.path.basename(entry))
        except OSError:
            pass
    return found


class Device:
    """What camera and microphone share: on and off with the switch, the
    tablet asked only while a program uses the device, and a state for the
    window: "off", "setup" (missing on this PC), "webrtc" (WebRTC missing on
    this PC), "ready", "in use", or why
    the tablet did not give it ("away", "tablet-off", "tablet-permission"...)."""

    def __init__(self, app, kind):
        self.app = app
        self.kind = kind
        self.state = "off"
        self.feed = None
        self.users = []  # the programs using it, by name, for the window
        self._in_use = False
        self._timer = None
        self.gated = False  # seat.py: the desktop locked, or another user's in front

    def _set_state(self, state):
        if state != self.state:
            self.state = state
            self.app.devices_changed()

    def _users(self, names):
        if names != self.users:
            self.users = names
            self.app.devices_changed()

    def where(self):
        """The tablet in use's addresses and app id: from the feed's thread."""
        tablet = self.app.tablets.active()
        return (self.app.tablets.targets(tablet), tablet.app_id) if tablet else None

    def _problem(self, reason):
        """The tablet could not give it: "away", or the app's reason as "tablet-<reason>"."""
        state = reason if reason == "away" else "tablet-" + reason
        GLib.idle_add(once(lambda: self.feed and self._set_state(state)))

    def _used(self, in_use):
        """A program took the device, or let it go: the tablet follows, a little later."""
        if self._timer:
            GLib.source_remove(self._timer)
        self._timer = GLib.timeout_add(int(1000 * (IN_USE_AFTER if in_use else IDLE_AFTER)),
                                       once(self._settle, in_use))

    def _settle(self, in_use):
        self._timer = None
        if in_use == self._in_use or self.state in ("off", "setup", "reload", "webrtc", "decoder"):
            return
        self._in_use = in_use
        if in_use:
            log(f"{self.kind}: a program uses it")
            if self.gated:
                self._set_state("locked")
                return
            self._set_state("in use")
            self._told()
            self.begin()
        else:
            log(f"{self.kind}: no program uses it")
            self.end()
            self._set_state("ready")

    def _told(self):
        """Which program turned the tablet's camera or microphone on, said
        each time (SEC-25): any program on this PC may, while the switch is on."""
        if self.kind not in ("camera", "microphone"):
            return
        title = ("A program uses the tablet's camera" if self.kind == "camera"
                 else "A program uses the tablet's microphone")
        self.app.notify(self.kind, title, "{programs}", programs=", ".join(self.users) or "?")

    def gate(self, closed):
        """seat.py: the desktop locked or another user's session in front,
        the tablet's feed pauses (no program gets its picture or sound
        meanwhile, SEC-22); open again, it goes on if a program still uses
        the device."""
        if closed == self.gated:
            return
        self.gated = closed
        if not self._in_use or self.state in ("off", "setup", "reload", "webrtc", "decoder"):
            return
        if closed:
            log(f"{self.kind}: paused, the desktop is locked")
            self.end()
            self._set_state("locked")
        else:
            log(f"{self.kind}: on again")
            self._set_state("in use")
            self._told()
            self.begin()

    def begin(self):
        raise NotImplementedError

    def end(self):
        raise NotImplementedError

    def stop_feed(self):
        if self.feed:
            self.feed.stop()
            self.feed = None

    def _ready(self):
        GLib.idle_add(once(lambda: self.feed and self._set_state("in use")))

    def renew(self):
        """What a session asks for changed (the tablet in use, the noise
        reduction): one in progress is made again with it."""
        if self.feed:
            self.end()
            self.begin()


class Camera(Device):
    """The webcam: Plus One keeps writing to v4l2loopback's device while the
    switch is on (programs list it only then), black while no program uses
    it, the tablet's camera while one does.

    The writer only ever sees frames of one format, so the device is never
    asked to change (v4l2loopback refuses while a program reads: "busy"):
    the tablet's picture is decoded to it, black is one frame made once,
    written again by a timer when no picture comes: 30 times a second while
    a program has the device open, once a second otherwise (a full-rate
    black generator cost 6-12% of a core, idle)."""

    def __init__(self, app):
        super().__init__(app, "camera")
        self.writers, self.opened = [], []
        self.frames = []
        self._black_timer = None  # (Device's _timer is the in-use one)
        self._fast = None  # the timer's pace: a program reading, or not
        self._live_at = 0.0  # monotonic time of the tablet's last frame
        self.restart_when_idle = False  # Devices.restart_camera

    def enable(self):
        if self.writers:
            return
        paths = camera_devices()
        self.decoder = h264_decoder()
        self.rtc = webrtc()
        if self.rtc is None:
            self._set_state("webrtc")
            return
        if not paths:
            # Loaded already (another program's settings, before Plus One's
            # options were there): reloading it is what is missing.
            self._set_state("reload" if os.path.isdir("/sys/module/v4l2loopback") else "setup")
            return
        if self.decoder is None:
            self._set_state("decoder")
            return
        width, height, _bitrate = self.size = QUALITIES.get(self.app.settings.camera_quality, QUALITIES["720p"])
        self.out = (f"video/x-raw,format=YUY2,width={width},height={height},framerate={FPS}/1,"
                    f"pixel-aspect-ratio=1/1,interlace-mode=progressive,colorimetry=bt601,chroma-site=jpeg")
        # Black in YUY2: luma 16, chroma 128 (limited range, as bt601 says).
        self._black_frame = bytes((16, 128)) * (width * height)
        try:
            for path in paths:
                writer = Gst.parse_launch(f"appsrc name=frames is-live=true do-timestamp=true format=time "
                                          f"caps={self.out} ! v4l2sink device={path} sync=false")
                self.writers.append(writer)  # from now on disable() takes it down
                bus = writer.get_bus()
                bus.add_signal_watch()
                bus.connect("message::error", self._error)
                # Watched before the writer opens it, which it keeps open: what is
                # open once it plays is Plus One's, anything more another program's.
                opened = Opened(path, self._opened)
                self.opened.append(opened)
                if writer.set_state(Gst.State.PLAYING) == Gst.StateChangeReturn.FAILURE:
                    raise RuntimeError(f"cannot write to {path}")
                writer.get_state(2 * Gst.SECOND)
                opened.started()
            self.frames = [writer.get_by_name("frames") for writer in self.writers]
            self._black()  # at once: a device with nothing written is not listed
            self._pace(False)
        except (GLib.Error, OSError, RuntimeError) as e:
            log("camera: not on:", getattr(e, "message", None) or str(e))
            self.disable()
            self._set_state("setup")
            return
        self._set_state("ready")
        log("camera: on, as", ", ".join(paths))
        self._opened(None)  # a program that had it open already

    def _write(self, buffer):
        for frames in self.frames:
            frames.emit("push-buffer", buffer)

    def _black(self):
        """Black, when the tablet's picture is not coming (its timer)."""
        if self.frames and time.monotonic() - self._live_at > LIVE_GAP:
            self._write(Gst.Buffer.new_wrapped(self._black_frame))
        return True

    def _pace(self, fast):
        if fast == self._fast:
            return
        self._fast = fast
        if self._black_timer:
            GLib.source_remove(self._black_timer)
        self._black_timer = GLib.timeout_add(1000 // FPS if fast else 1000, self._black)

    def disable(self):
        self.stop_feed()
        if self._black_timer:
            GLib.source_remove(self._black_timer)
        self._black_timer = self._fast = None
        for opened in self.opened:
            opened.close()
        for writer in self.writers:
            writer.get_bus().remove_signal_watch()
            writer.set_state(Gst.State.NULL)
        self.writers, self.opened, self.frames = [], [], []
        self._in_use = False
        self.restart_when_idle = False
        self._set_state("off")

    def _opened(self, _count):
        reading = any(opened.count > opened.baseline for opened in self.opened)
        if self.frames:
            self._pace(reading)
        self._users(holder_names([opened.path for opened in self.opened]) if reading else [])
        self._used(reading)

    def begin(self):
        settings = self.app.settings
        width, height, bitrate = QUALITIES.get(settings.camera_quality, QUALITIES["720p"])
        request = {"type": "camera", "facing": settings.camera_facing, "width": width, "height": height,
                   "fps": FPS, "bitrate": bitrate}
        self.feed = Feed(request, self.where, self._receiver, self._ready, self._problem)
        self.feed.start()

    def end(self):
        self.stop_feed()
        self._live_at = 0.0  # black at the next tick, not half a second later
        if self.restart_when_idle:
            GLib.idle_add(once(self.app.devices.restart_camera))  # its new size, now

    def _receiver(self):
        """The tablet's video, decoded at the device's size and format, into the writers."""
        return self.rtc.Receiver("video", self._on_pad)

    def _on_pad(self, receiver, pad):
        # Nothing before a key frame, a new one asked for (PLI) after a loss,
        # and no frame the decoder knows to be broken. videorate only drops
        # (the tablet sending more than 30): holding each frame until the
        # next, to repeat it should none come, was a frame (33 ms) late.
        decoder = f"{self.decoder} output-corrupt=false" if self.decoder == "avdec_h264" else self.decoder
        chain = self.rtc.link(receiver.pipeline, pad, f"rtph264depay wait-for-keyframe=true request-keyframe=true "
                                                      f"! h264parse ! {decoder} ! videoconvert "
                                                      f"! videoscale add-borders=true ! videorate drop-only=true "
                                                      f"! {self.out} ! appsink name=decoded emit-signals=true "
                                                      f"sync=false max-buffers=2 drop=true")
        chain.get_by_name("decoded").connect("new-sample", self._decoded_frame)

    def _decoded_frame(self, sink):
        # The writer's own time on it (do-timestamp), not the session's.
        buffer = sink.emit("pull-sample").get_buffer().copy()
        buffer.pts = buffer.dts = Gst.CLOCK_TIME_NONE
        self._live_at = time.monotonic()
        if self.feed:
            self._write(buffer)
        return Gst.FlowReturn.OK

    def _error(self, bus, message):
        error, _debug = message.parse_error()
        log("camera:", error.message)


class AudioDevice(Device):
    """What microphone and speakers share: a virtual PipeWire node (a null
    sink, NAME) made while the switch is on, and the programs that use it,
    known from its links (pw-link) and named by pw-dump. Subclasses say
    what the node is (CLASS, CHANNELS, DESCRIPTION), which links are a
    program using it (ours()) and who they are (users_of())."""

    NAME = CLASS = DESCRIPTION = None
    CHANNELS = ("1", "mono")
    NEEDS = "webrtc"  # what else it needs: WebRTC for GStreamer, or python3-cryptography
    # Programs' streams that pause and play again while linked to it (a
    # paused video): watched (pactl subscribe), so a paused one stops using it.
    WATCH_PAUSES = False

    def __init__(self, app, kind):
        super().__init__(app, kind)
        self.module = None
        self.links = set()
        self.watch = None
        self.pauses = None  # pactl subscribe, when WATCH_PAUSES
        self._asked = 0  # the last look at who uses it (pw-dump), its number

    def enable(self):
        if self.module:
            return
        if self.NEEDS == "webrtc":
            self.rtc = webrtc()
            if self.rtc is None:
                self._set_state("webrtc")
                return
        elif importlib.util.find_spec("cryptography") is None:
            self._set_state("setup")
            return
        if not all(shutil.which(tool) for tool in ("pactl", "pw-link", "pw-dump")):
            self._set_state("setup")
            return
        unload_stale(self.NAME)
        try:
            # No rate of its own: it follows the graph's (Bluetooth headphones
            # run theirs at 44.1 kHz, and a 48 kHz source went silent in it).
            out = subprocess.run(["pactl", "load-module", "module-null-sink", f"sink_name={self.NAME}",
                                  f"media.class={self.CLASS}", f"channels={self.CHANNELS[0]}",
                                  f"channel_map={self.CHANNELS[1]}",
                                  f"sink_properties=\"device.description='{self.DESCRIPTION}'\""],
                                 capture_output=True, text=True, timeout=5, check=True).stdout
            self.module = out.strip()
        except (OSError, subprocess.SubprocessError) as e:
            log(f"{self.kind}: no PipeWire node:", repr(e))
            self._set_state("setup")
            return
        self._watch_links()
        self._set_state("ready")
        log(f"{self.kind}: on, as", self.NAME)

    def disable(self):
        self.end()
        if self.watch:
            self.watch.force_exit()
            self.watch = None
        if self.pauses:
            self.pauses.force_exit()
            self.pauses = None
        if self.module:
            try:
                subprocess.run(["pactl", "unload-module", self.module], capture_output=True, timeout=5)
            except (OSError, subprocess.SubprocessError) as e:
                log(f"{self.kind}: its node not removed:", repr(e))
            self.module = None
        self.links.clear()
        self._in_use = False
        self._set_state("off")

    def ours(self, link):
        """Whether link (output port, input port) is a program using it."""
        raise NotImplementedError

    def users_of(self, objects):
        """The programs using it, by name, from pw-dump's objects."""
        raise NotImplementedError

    # -- who uses it -------------------------------------------------------------
    def _watch_links(self):
        """pw-link tells each link made or removed."""
        self._parser = LinkParser()
        self.watch = Gio.Subprocess.new(["pw-link", "--monitor", "--links", "--output"],
                                        Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_SILENCE)
        self._lines = Gio.DataInputStream.new(self.watch.get_stdout_pipe())
        self._lines.read_line_async(GLib.PRIORITY_DEFAULT, None, self._line, self.watch)
        if self.WATCH_PAUSES:
            # Events only (nothing polled): a stream made, changed (paused,
            # played again) or gone; who uses it is looked at again then.
            try:
                self.pauses = Gio.Subprocess.new(["pactl", "subscribe"],
                                                 Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_SILENCE)
            except GLib.Error as e:
                log(f"{self.kind}: pauses not watched:", e.message)
                return
            events = Gio.DataInputStream.new(self.pauses.get_stdout_pipe())
            events.read_line_async(GLib.PRIORITY_DEFAULT, None, self._event, self.pauses)

    def _event(self, stream, result, pauses):
        try:
            line, _length = stream.read_line_finish_utf8(result)
        except GLib.Error:
            line = None
        if line is None or pauses is not self.pauses:
            return  # switched off, or PipeWire went (the links' watch handles that)
        if "sink-input" in line and self.links:
            self._who_records()
            # A stream played again is told before PipeWire runs its node
            # (still "idle" then): looked at once more a moment later, or a
            # video played again after a pause went unheard.
            GLib.timeout_add(AGAIN_AFTER_EVENT, once(lambda: self.links and self._who_records()))
        stream.read_line_async(GLib.PRIORITY_DEFAULT, None, self._event, pauses)

    def _line(self, stream, result, watch):
        try:
            line, _length = stream.read_line_finish_utf8(result)
        except GLib.Error:
            line = None
        if watch is not self.watch:
            return  # switched off
        if line is None:
            # pw-link ended by itself: PipeWire was restarted, and the node
            # went with it. Made again once PipeWire is back.
            log(f"{self.kind}: PipeWire went away")
            self.module = None  # gone; its number may be another module's by now
            self.disable()
            GLib.timeout_add_seconds(RETRY, once(self._come_back))
            return
        changed = self._parser(line)
        if changed:
            sign, link = changed
            if self.ours(link):
                (self.links.add if sign in "=+" else self.links.discard)(link)
                self._who_records()
        stream.read_line_async(GLib.PRIORITY_DEFAULT, None, self._line, watch)

    def _who_records(self):
        """Whether a program uses it, from what pw-dump says of the streams
        linked to it (users_of())."""
        if not self.links:
            self._asked += 1  # an answer still coming is out of date
            self._used(False)
            return
        self._asked += 1
        dump = Gio.Subprocess.new(["pw-dump"], Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_SILENCE)
        dump.communicate_utf8_async(None, None, self._dumped, (self.watch, self._asked))

    def _dumped(self, dump, result, asked):
        try:
            _ok, out, _err = dump.communicate_utf8_finish(result)
            names = self.users_of(pw_objects(out))
        except (GLib.Error, ValueError, TypeError, AttributeError) as e:
            log(f"{self.kind}: who uses it, not known:", repr(e))
            names = ["?"] * len(self.links)  # as if each were a program's
        if asked == (self.watch, self._asked):
            self._users(sorted(set(n for n in names if n != "?")))
            self._used(bool(names))

    def _come_back(self):
        self.app.devices.apply()
        if self.state == "setup":
            GLib.timeout_add_seconds(RETRY, once(self._come_back))  # PipeWire not back yet


class Microphone(AudioDevice):
    """The microphone: a virtual PipeWire source (a null node that programs
    see as a microphone only), which a stream of Plus One's is linked into
    while a program records. About 30 ms on the PC (a pipe source was 170
    and grew with each burst of Wi-Fi); never more than MIC_BEHIND waits."""

    NAME, CLASS, DESCRIPTION = MIC_NAME, "Audio/Source/Virtual", "Tablet microphone (Plus One)"

    def __init__(self, app):
        super().__init__(app, "microphone")
        self._streams = itertools.count(1)

    def ours(self, link):
        return link[0].startswith(MIC_NAME + ":")  # a program recording: a link from its ports

    def users_of(self, objects):
        # A level meter's ("Peak detect": GNOME Settings' Sound panel,
        # pavucontrol) does not count, or an open Sound panel would keep the
        # tablet's microphone on, only to show a level.
        return recorders(objects)

    # -- the tablet's sound into the source ---------------------------------------
    def begin(self):
        self.chain, self.noise = voice(self.app.settings.microphone_noise)
        log(f"microphone: noise reduction {self.noise}")
        # The tablet's libwebrtc takes the noise out only for "light": twice
        # (it and DeepFilterNet) took the voice's edges too.
        request = {"type": "microphone", "noise_reduction": self.noise == "light"}
        self.feed = Feed(request, self.where, self._receiver, self._ready, self._problem)
        self.feed.start()

    def end(self):
        self.stop_feed()

    def _receiver(self):
        """The tablet's sound, decoded into a stream of Plus One's, linked into
        the source by hand (nothing else plays it). webrtcbin's jitter buffer
        puts the packets back in order and evens out the Wi-Fi's timing; one
        still missing the decoder conceals."""
        return self.rtc.Receiver("audio", self._on_pad)

    def _on_pad(self, receiver, pad):
        # A name of its own: linked by name, and the last session's stream
        # may still be there for a moment (or a second one of this session's).
        name = f"{MIC_FEED}-{next(self._streams)}"
        # Decoded, the noise taken out if neural, the level set (voice()).
        self.rtc.link(receiver.pipeline, pad,
                      f"rtpopusdepay ! opusdec plc=true use-inband-fec=true ! audioconvert ! audioresample "
                      f"! audio/x-raw,format=F32LE,rate=48000,channels=1 ! {self.chain} "
                      f"! audioconvert dithering=none ! audio/x-raw,format=S16LE,rate=48000,channels=1 "
                      f"! queue max-size-time={MIC_BEHIND} max-size-buffers=0 max-size-bytes=0 leaky=downstream "
                      # As it comes, not by the pipeline's clock (not PipeWire's: the sink
                      # came after webrtcbin started). PipeWire is asked to read 10 ms at
                      # a time: each 20 ms packet lasts it two reads. At its usual 21.3 ms
                      # a packet was always 64 samples short, and the source went silent.
                      f"! pipewiresink sync=false stream-properties=\"props,node.name={name},"
                      f"node.autoconnect=false,media.type=Audio,node.latency=480/48000\"")
        # Its ports show once sound flows: waited for on another thread. Not
        # linked, the program would hear nothing: the session is made again.
        threading.Thread(target=lambda: link_feed(name) or receiver.fail("could not link Plus One's stream "
                                                                         "into the source"), daemon=True).start()


def link_feed(name, wait=5):
    """Links Plus One's stream (node name) into the microphone's source, once
    PipeWire shows its ports: True if done."""
    deadline = time.monotonic() + wait
    while time.monotonic() < deadline:
        outputs = subprocess.run(["pw-link", "--output"], capture_output=True, text=True).stdout.split()
        inputs = subprocess.run(["pw-link", "--input"], capture_output=True, text=True).stdout.split()
        feed = [p for p in outputs if p.startswith(name + ":output")]
        source = [p for p in inputs if p.startswith(MIC_NAME + ":")]
        if feed and source:
            return subprocess.run(["pw-link", feed[0], source[0]], capture_output=True).returncode == 0
        time.sleep(0.05)
    return False


def recorders(objects):
    """The programs whose streams record from the microphone's source, in
    pw-dump's objects (links from it, to streams other than level meters'),
    by name; one entry per stream."""
    nodes = {o.get("id"): (o.get("info") or {}).get("props") or {}
             for o in objects if o.get("type") == "PipeWire:Interface:Node"}
    source = {id for id, props in nodes.items() if props.get("node.name") == MIC_NAME}
    found = []
    for o in objects:
        info = o.get("info") or {}
        if o.get("type") == "PipeWire:Interface:Link" and info.get("output-node-id") in source:
            props = nodes.get(info.get("input-node-id"), {})
            if props.get("stream.monitor") not in (True, "true", "1"):
                found.append(str(props.get("application.name") or props.get("node.name") or "?"))
    return found


def make_default_sink(name):
    """name the output programs play to (what GNOME's Sound settings set)."""
    try:
        subprocess.run(["pactl", "set-default-sink", name], capture_output=True, timeout=5)
    except (OSError, subprocess.SubprocessError) as e:
        log("speakers: not made the output:", repr(e))


class BluetoothAudio:
    """The tablet's own Bluetooth audio, off while the tablet is this PC's
    speakers (l15): else Android sends the tablet's sound to the PC over
    A2DP, the PC's default output (the tablet) plays it, and round. Off as
    Bluetooth goes: BlueZ disconnects the tablet's A2DP source, and Android
    plays on its own speaker again (PipeWire's card profile "off" was not
    enough: the A2DP link stayed, Android kept sending on it, and the PC
    dropped it: nothing heard at all). Every CHECK while the speakers are
    on; connected again when they go, and at the next start after a crash
    (STATE)."""

    CHECK = 5  # seconds
    A2DP_SOURCE = "0000110a-0000-1000-8000-00805f9b34fb"  # the tablet's role: it sends
    STATE = os.path.join(GLib.get_user_state_dir(), "plus-one", "bluetooth-audio.json")

    def __init__(self, tablet_name):
        self.tablet_name = tablet_name  # () -> the tablet in use's name (its Bluetooth one), or None
        self._timer = None

    def hold(self):
        if self._timer is None:
            self._check()
            self._timer = GLib.timeout_add_seconds(self.CHECK, self._check)

    def release(self):
        if self._timer is not None:
            GLib.source_remove(self._timer)
            self._timer = None
        self.restore()

    def _check(self):
        name = self.tablet_name()
        objects = _bluez_objects()
        for path, interfaces in objects.items():
            device = interfaces.get("org.bluez.Device1")
            if not device or not device.get("Connected") or name not in (device.get("Name"), device.get("Alias")):
                continue
            # Its A2DP up: a media transport of the device's.
            if not any(p.startswith(path + "/") and "org.bluez.MediaTransport1" in i for p, i in objects.items()):
                continue
            self._save(path)
            log(f"speakers: {name}'s own Bluetooth audio disconnected: no sound round and round")
            _bluez_call(path, "DisconnectProfile", self.A2DP_SOURCE)
        return True

    @classmethod
    def restore(cls):
        """Each A2DP disconnected connected again (also after a crash)."""
        try:
            with open(cls.STATE) as f:
                saved = json.load(f)
        except (OSError, ValueError):
            return
        for path in saved if isinstance(saved, list) else []:
            if isinstance(path, str) and path.startswith("/org/bluez/"):
                _bluez_call(path, "ConnectProfile", cls.A2DP_SOURCE)
                log("speakers: the tablet's Bluetooth audio connected again")
        try:
            os.remove(cls.STATE)
        except OSError:
            pass

    def _save(self, path):
        try:
            with open(self.STATE) as f:
                saved = json.load(f)
        except (OSError, ValueError):
            saved = []
        if not isinstance(saved, list):
            saved = []
        if path not in saved:
            saved.append(path)
            os.makedirs(os.path.dirname(self.STATE), exist_ok=True)
            with open(self.STATE, "w") as f:
                json.dump(saved, f)


def _bluez_objects():
    try:
        bus = Gio.bus_get_sync(Gio.BusType.SYSTEM, None)
        return bus.call_sync("org.bluez", "/", "org.freedesktop.DBus.ObjectManager", "GetManagedObjects", None,
                             None, Gio.DBusCallFlags.NONE, 5000, None).unpack()[0]
    except GLib.Error:
        return {}


def _bluez_call(path, method, uuid):
    """Device1.method(uuid), not waited for (connecting takes seconds)."""
    try:
        bus = Gio.bus_get_sync(Gio.BusType.SYSTEM, None)
        bus.call("org.bluez", path, "org.bluez.Device1", method, GLib.Variant("(s)", (uuid,)), None,
                 Gio.DBusCallFlags.NONE, 20000, None, lambda b, r: _bluez_done(b, r, method), None)
    except GLib.Error as e:
        log(f"speakers: Bluetooth {method}: {e.message}")


def _bluez_done(bus, result, method):
    try:
        bus.call_finish(result)
    except GLib.Error as e:
        log(f"speakers: Bluetooth {method}: {e.message}")


class Speakers(AudioDevice):
    """The tablet's speakers: a virtual PipeWire sink ("Tablet speakers"),
    which Plus One captures and sends to the tablet as it is (pcm.PcmSender)
    while a program plays into it, and nothing otherwise."""

    NAME, CLASS, DESCRIPTION = SPEAKERS_NAME, "Audio/Sink", "Tablet speakers (Plus One)"
    CHANNELS = ("2", "front-left,front-right")
    NEEDS = "cryptography"
    WATCH_PAUSES = True

    def __init__(self, app):
        super().__init__(app, "speakers")
        self.bluetooth = BluetoothAudio(lambda: (self.app.tablets.active() or types.SimpleNamespace(name=None)).name)

    def enable(self):
        super().enable()
        if self.module:
            self.bluetooth.hold()

    def disable(self):
        super().disable()
        self.bluetooth.release()

    def ours(self, link):
        return link[1].startswith(SPEAKERS_NAME + ":playback_")  # a program playing: a link into it

    def users_of(self, objects):
        return players(objects)

    def begin(self):
        self.feed = Feed({"type": "speakers"}, self.where, self._sender, self._ready, self._problem)
        self.feed.start()

    def end(self):
        self.stop_feed()

    def _sender(self):
        from .pcm import PcmSender
        # What plays into the sink, from its monitor, at the graph's own
        # pace (PcmSender cuts it in 5 ms packets whatever comes). Not 5 ms
        # forced on the whole graph: Chrome's playback, writing in larger
        # pieces, stopped itself after about 10 s at that (Firefox did not),
        # and every program's sound woke the CPU four times as often.
        return PcmSender(f"pipewiresrc target-object={SPEAKERS_NAME} do-timestamp=true "
                         f"stream-properties=\"props,stream.capture.sink=true,"
                         f"node.name=plusone-speakers-feed\"")


def pw_objects(text):
    """pw-dump's objects. It prints one JSON array, and while the graph
    changes (a stream starting) more arrays after it, of what changed: each
    object as last told, those removed (info null) left out."""
    decoder, at, objects = json.JSONDecoder(), 0, {}
    while True:
        while at < len(text) and text[at].isspace():
            at += 1
        if at >= len(text):
            break
        document, at = decoder.raw_decode(text, at)
        for o in document if isinstance(document, list) else ():
            if isinstance(o, dict):
                objects[o.get("id")] = o
    return [o for o in objects.values() if o.get("info") is not None]


def players(objects):
    """The programs whose streams play into the speakers' sink, in pw-dump's
    objects (links to it), by name; one entry per stream. A stream linked
    but paused (a video paused, its tab still open: node state "idle") is
    no program playing: the tablet need not stay awake for it. The sink
    drives itself (node.driver), so a stream playing runs whether the
    tablet takes its sound yet or not."""
    nodes = {o.get("id"): o.get("info") or {} for o in objects if o.get("type") == "PipeWire:Interface:Node"}
    sink = {id for id, info in nodes.items() if (info.get("props") or {}).get("node.name") == SPEAKERS_NAME}
    found = []
    for o in objects:
        info = o.get("info") or {}
        if o.get("type") == "PipeWire:Interface:Link" and info.get("input-node-id") in sink:
            node = nodes.get(info.get("output-node-id"), {})
            if node.get("state", "running") != "running":
                continue
            props = node.get("props") or {}
            found.append(str(props.get("application.name") or props.get("node.name") or "?"))
    return found


class LinkParser:
    """Links from pw-link --monitor --links --output, line by line: ("+",
    (output port, input port)) for one made, "-" removed, "=" there already.
    Those there already come as a port's line, then "=   |-> input" lines."""

    def __init__(self):
        self.port = None

    def __call__(self, line):
        sign, _space, rest = line.rstrip().partition(" ")
        rest = rest.strip()
        if sign not in ("=", "+", "-"):
            return None
        if rest.startswith("|->"):
            return (sign, (self.port, rest[3:].strip())) if self.port else None
        if " -> " in rest:
            source, _arrow, target = rest.partition(" -> ")
            return sign, (source.strip(), target.strip())
        self.port = rest
        return None


def unload_stale(name=MIC_NAME):
    """A node left by a Plus One that did not end cleanly."""
    try:
        modules = subprocess.run(["pactl", "list", "modules", "short"], capture_output=True, text=True,
                                 timeout=5).stdout
    except (OSError, subprocess.SubprocessError):
        return
    for line in modules.splitlines():
        if f"sink_name={name}" in line or f"source_name={name}" in line:
            try:
                subprocess.run(["pactl", "unload-module", line.split()[0]], capture_output=True, timeout=5)
            except (OSError, subprocess.SubprocessError):
                pass


class Devices:
    """Camera, microphone and speakers, as the settings and the tablet in use say."""

    def __init__(self, app):
        self.app = app
        self.camera = Camera(app)
        self.microphone = Microphone(app)
        self.speakers = Speakers(app)
        BluetoothAudio.restore()  # turned off by a Plus One that did not live to turn it back
        self.tablet = None  # the fingerprint of the tablet in use, as last applied

    def apply(self):
        settings = self.app.settings
        active = self.app.tablets.active()
        fingerprint = active.fingerprint if active is not None else None
        changed, self.tablet = fingerprint != self.tablet, fingerprint
        # The speakers only while the tablet's app is on the network: away, the
        # output goes, and the PC's sound back to its own (a camera or
        # microphone a program picked stays, and says the tablet is away).
        here = active is not None and bool(self.app.tablets.addresses(active))
        for device, on in ((self.camera, settings.camera), (self.microphone, settings.microphone),
                           (self.speakers, settings.speakers and here)):
            if on and active is not None:
                device.enable()
                if changed:
                    # Another tablet in use (or this one forgotten): not one
                    # more moment from the one before.
                    device.renew()
            else:
                device.disable()

    def gate(self, closed):
        """seat.py's say for the camera and the microphone (the speakers play
        on: they take nothing in)."""
        self.camera.gate(closed)
        self.microphone.gate(closed)

    def restart_camera(self):
        """Its quality changed: the device's format with it, which means a
        new device: not while a program uses it (its picture would stop, and
        it would have to find the camera again), but once it lets go."""
        if self.camera._in_use:
            log("camera: the new size once no program uses it")
            self.camera.restart_when_idle = True
            return
        self.camera.restart_when_idle = False
        self.camera.disable()
        self.apply()

    def shutdown(self):
        self.camera.disable()
        self.microphone.disable()
        self.speakers.disable()
