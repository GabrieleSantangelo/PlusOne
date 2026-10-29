"""The streaming pipeline produces MPEG-TS in RTP (payload type 33)."""
import time

import gi
import pytest

gi.require_version("Gst", "1.0")
from gi.repository import Gst  # noqa: E402

from plusone import media  # noqa: E402

Gst.init(None)


def test_pick_encoder_finds_one():
    assert media.pick_encoder("auto") in media.ENCODERS


@pytest.mark.skipif(not Gst.ElementFactory.find("x264enc"), reason="x264enc not installed")
@pytest.mark.parametrize("encoder", ["x264", "va-lp"])
def test_keyframes_on_request_are_idrs_with_their_headers(encoder):
    """Each request: an IDR (NAL 5) with an SPS (7) before it, the next frames."""
    import re
    element = media.ENCODERS[encoder][0]
    if not Gst.ElementFactory.find(element) or not Gst.ElementFactory.make(element):
        pytest.skip(f"{element} not available")
    convert = media.CONVERT.get(encoder, f"videoconvert ! video/x-raw,format={media.ENCODERS[encoder][2]}")
    options = media.ENCODERS[encoder][1].format(bitrate=2000, gop=600)
    pipeline = Gst.parse_launch(
        f"videotestsrc is-live=true ! video/x-raw,format=BGRA,width=320,height=200,framerate=30/1 ! "
        f"queue max-size-buffers=1 leaky=downstream ! {convert} ! {element} name=enc {options} ! "
        f"h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au ! "
        f"appsink name=out emit-signals=true sync=false")
    idrs = []

    def got(sink):
        data = sink.emit("pull-sample").get_buffer().extract_dup(0, 1 << 20)
        types = [data[m.end()] & 0x1F for m in re.finditer(b"\x00\x00\x01", data)]
        if 5 in types:
            idrs.append(7 in types)
        return Gst.FlowReturn.OK

    pipeline.get_by_name("out").connect("new-sample", got)
    keyframes = media.Keyframes(pipeline.get_by_name("enc"))
    pipeline.set_state(Gst.State.PLAYING)
    try:
        for _ in range(3):
            time.sleep(0.4)
            keyframes.request()
        time.sleep(0.4)
    finally:
        pipeline.set_state(Gst.State.NULL)
        keyframes.remove()
    assert idrs[:4] == [True] * 4, idrs  # the first, and one per request


@pytest.mark.parametrize("encoder", ["x264", "va-lp"])
def test_a_keyframe_made_from_the_encoders_input_is_the_frame_going_in(encoder):
    """appdisplay makes a keyframe from a probe on the encoder's input, once
    the whole of it fits in the link: that very frame must be it (an IDR,
    its SPS before it), out before the next goes in."""
    import re
    element = media.ENCODERS[encoder][0]
    if not Gst.ElementFactory.find(element) or not Gst.ElementFactory.make(element):
        pytest.skip(f"{element} not available")
    convert = media.CONVERT.get(encoder, f"videoconvert ! video/x-raw,format={media.ENCODERS[encoder][2]}")
    options = media.ENCODERS[encoder][1].format(bitrate=2000, gop=600)
    pipeline = Gst.parse_launch(
        f"videotestsrc is-live=true pattern=ball ! video/x-raw,format=BGRA,width=320,height=200,framerate=30/1 ! "
        f"queue max-size-buffers=1 leaky=downstream ! {convert} ! {element} name=enc {options} ! "
        f"h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au ! "
        f"appsink name=out emit-signals=true sync=false")
    enc = pipeline.get_by_name("enc")
    keyframes = media.Keyframes(enc)
    went_in, idrs, behind = [0], [], []

    def frame_in(pad, _info):
        went_in[0] += 1
        if went_in[0] in (10, 20):
            keyframes.now(pad)
        return Gst.PadProbeReturn.OK

    def got(sink):
        data = sink.emit("pull-sample").get_buffer().extract_dup(0, 1 << 20)
        types = [data[m.end()] & 0x1F for m in re.finditer(b"\x00\x00\x01", data)]
        behind.append(went_in[0] - len(behind) - 1)
        if 5 in types:
            idrs.append((len(behind), 7 in types))
        return Gst.FlowReturn.OK

    probe = enc.get_static_pad("sink").add_probe(Gst.PadProbeType.BUFFER, frame_in)
    pipeline.get_by_name("out").connect("new-sample", got)
    pipeline.set_state(Gst.State.PLAYING)
    try:
        time.sleep(1.2)
    finally:
        pipeline.set_state(Gst.State.NULL)
        enc.get_static_pad("sink").remove_probe(probe)
        keyframes.remove()
    assert idrs[:3] == [(1, True), (10, True), (20, True)], idrs
    assert set(behind) == {0}  # each frame out before the next went in


def test_keyframe_cap_is_set_on_the_encoder():
    import gi
    gi.require_version("Gst", "1.0")
    from gi.repository import Gst
    from plusone import media
    Gst.init(None)
    enc = Gst.ElementFactory.make("x264enc")
    if enc is None:
        pytest.skip("x264enc not installed")
    media.cap_keyframes(enc, 20000, 60)
    assert enc.get_property("vbv-buf-capacity") == int(1000 * media.VBV_FRAMES / 60)
