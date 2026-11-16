"""The H.264 encoders the desktop is encoded with (appdisplay.py), and the
keyframes they give on request.
"""
import gi

gi.require_version("Gst", "1.0")
from gi.repository import Gst  # noqa: E402

# A keyframe of a page of text is 80-140 KB, sent as 60-100 packets back to
# back. Keep them smaller and rarer: the app asks for one when it lost data.
KEYFRAME_SECONDS = 10      # periodic keyframe interval
VBV_FRAMES = 4             # a keyframe may be this many average frames

# name -> (GStreamer element, settings, raw format it takes), in preference order.

ENCODERS = {
    # Intel's low-power encoder (VDENC), the frame turned into NV12 on the
    # GPU too: first, for the battery. Measured at 1920x1200@60: 3.8 ms to the
    # encoder's output and 3.5% of a core, against NVENC's 4.7 ms, 7% and the
    # NVIDIA GPU woken (about 8 W more).
    "va-lp": ("vah264lpenc", "rate-control=cbr bitrate={bitrate} key-int-max={gop} b-frames=0 "
              "ref-frames=1 target-usage=7 aud=false", "NV12"),
    "nvenc": ("nvh264enc", "preset=low-latency-hq zerolatency=true bframes=0 rc-mode=cbr "
              "bitrate={bitrate} gop-size={gop} aud=false", "BGRA"),
    "va": ("vah264enc", "rate-control=cbr bitrate={bitrate} key-int-max={gop} b-frames=0 "
           "target-usage=7", "NV12"),
    "vaapi": ("vaapih264enc", "rate-control=cbr bitrate={bitrate} keyframe-period={gop} "
              "max-bframes=0 tune=low-power", "NV12"),
    "x264": ("x264enc", "tune=zerolatency speed-preset=ultrafast bitrate={bitrate} "
             "key-int-max={gop} bframes=0", "I420"),
}
# The same, for H.265 (HEVC): about a third fewer bytes at the same picture,
# and so a keyframe a third smaller to get through after a stall. Taken when
# the tablet's decoder takes it too (AppDisplay's offer, DisplayHandler's
# answer); PLUSONE_CODEC=h264 never.
ENCODERS_HEVC = {
    "va-lp": ("vah265lpenc", "rate-control=cbr bitrate={bitrate} key-int-max={gop} b-frames=0 "
              "ref-frames=1 target-usage=7 aud=false", "NV12"),
    "nvenc": ("nvh265enc", "preset=low-latency-hq zerolatency=true bframes=0 rc-mode=cbr "
              "bitrate={bitrate} gop-size={gop} aud=false", "BGRA"),
    "vaapi": ("vaapih265enc", "rate-control=cbr bitrate={bitrate} keyframe-period={gop} "
              "max-bframes=0 tune=low-power", "NV12"),
    "x264": ("x265enc", "tune=zerolatency speed-preset=ultrafast bitrate={bitrate} "
             "key-int-max={gop} bframes=0", "I420"),
}
CODEC_ENCODERS = {"h264": ENCODERS, "hevc": ENCODERS_HEVC}
# Encoders that start a new sequence, a keyframe, at each change of the
# bitrate (measured, GStreamer 1.24: vah264lpenc and vaapih264enc; vah264enc
# is vah264lpenc's own code; vah265lpenc is the same code). NVENC and x264
# change it in place.
RESTARTS = {"vah264lpenc", "vah264enc", "vaapih264enc", "vah265lpenc", "vaapih265enc"}
# Mutter hands out BGRA as readily as BGRx; asking for it makes the colour
# conversion a no-op for NVENC (it cost ~2.3 ms of CPU per 1920x1200 frame).
CAPTURE_FORMAT = "BGRA"
# How each encoder gets its raw format: VA's on the GPU (vapostproc; on the
# CPU it was +30% of a core), the others' by videoconvert.
VA_CONVERT = "vapostproc ! video/x-raw(memory:VAMemory),format=NV12"
CONVERT = {"va-lp": VA_CONVERT, "va": VA_CONVERT}


class Keyframes:
    """Keyframes on request that a decoder can start from (an IDR, its SPS
    and PPS before it). GStreamer 1.24's VA encoders answer a force-key-unit
    event with an I frame that is not an IDR, without the SPS, and only the
    first time: a decoder that had lost its place waited for the periodic
    keyframe (KEYFRAME_SECONDS), and one that had none could not start.
    Flushing them starts their GOP again, with an IDR, at the next frame. It
    is done on the streaming thread, just before a frame goes in: from
    another thread the flush could reach the frame being pushed, and the
    queue in front would stop. NVENC ignores the flush and honours the
    event; the others get the event."""

    FLUSHED = {"vah264lpenc", "vah264enc", "vah265lpenc"}

    def __init__(self, enc, on_flush=None):
        """on_flush(): on the streaming thread, after a flush threw away the
        frames the encoder held."""
        self.pad = enc.get_static_pad("sink")
        self.flush = enc.get_factory().get_name() in self.FLUSHED
        self.on_flush = on_flush or (lambda: None)
        self.wanted = False
        self.probe = self.pad.add_probe(Gst.PadProbeType.BUFFER, self._before_frame) if self.flush else None

    def request(self):
        if self.flush:
            self.wanted = True
            return
        s = Gst.Structure.new_from_string("GstForceKeyUnit, all-headers=(boolean)true")
        self.pad.send_event(Gst.Event.new_custom(Gst.EventType.CUSTOM_DOWNSTREAM, s))

    def now(self, pad):
        """A keyframe with the frame going into the encoder now: from a probe
        on its input (pad), on the streaming thread."""
        if self.flush:
            self.wanted = True
            self._before_frame(pad, None)
        else:
            self.request()

    def _before_frame(self, pad, _info):
        if self.wanted:
            self.wanted = False
            segment = pad.get_sticky_event(Gst.EventType.SEGMENT, 0)
            pad.send_event(Gst.Event.new_flush_start())
            pad.send_event(Gst.Event.new_flush_stop(False))  # the running time goes on
            if segment is not None:
                pad.send_event(segment)  # the flush took it
            self.on_flush()
        return Gst.PadProbeReturn.OK

    def remove(self):
        if self.probe is not None:
            self.pad.remove_probe(self.probe)
            self.probe = None


def cap_keyframes(enc, bitrate, fps):
    """Each encoder names its rate-control buffer differently; set the
    ones this one has (kbit or milliseconds)."""
    kbit, ms = int(bitrate / fps * VBV_FRAMES), int(1000 * VBV_FRAMES / fps)
    for prop, value in (("vbv-buffer-size", kbit), ("cpb-size", kbit),
                        ("cpb-length", ms), ("vbv-buf-capacity", ms)):
        if enc.find_property(prop):
            enc.set_property(prop, value)


def pick_encoder(preferred="auto"):
    names = list(ENCODERS) if preferred == "auto" else [preferred]
    for name in names:
        if Gst.ElementFactory.find(ENCODERS[name][0]):
            return name
    raise RuntimeError("no H.264 encoder available")


def offered_codecs(name):
    """The codecs this encoder (a key of ENCODERS) can make here, best
    first: HEVC when its element is there and PLUSONE_CODEC is not h264."""
    import os
    codecs = ["h264"]
    element = ENCODERS_HEVC.get(name, (None,))[0]
    if element and os.environ.get("PLUSONE_CODEC", "").strip().lower() != "h264" and Gst.ElementFactory.find(element):
        codecs.insert(0, "hevc")
    return codecs


def warm_up(name):
    """Load the encoder's plugin ahead of the stream (blocking): the first
    nvh264enc in a process takes 1.5 s, CUDA probing the GPUs."""
    Gst.ElementFactory.make(ENCODERS[name][0])
