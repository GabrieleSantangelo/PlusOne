"""Python's side of tools/bench/av.sh: a test pattern and a test tone to the
fake tablet at once, the way Plus One sends the desktop and the speakers
(appdisplay.AppDisplay; devices.Feed with a pcm.PcmSender): the same
threads. av.py HOST PORT SECONDS BITRATE SIZE [camera] [input]: the camera's
frames written as devices.Camera writes them; the fake tablet's fingers and
pen taken by appdisplay.Input, given to nothing (not the desktop)."""
import sys
import threading

import gi

gi.require_version("Gst", "1.0")
from gi.repository import GLib, Gst  # noqa: E402

from plusone import appdisplay, link, media, pcm  # noqa: E402


class Nothing:
    """Where the fingers and the pen go: nowhere (not the desktop)."""

    def __getattr__(self, name):
        return lambda *args, **kwargs: None


def camera_load(width, height, fps):
    """The camera's frames, as devices.Camera._decoded_frame writes them: from
    an appsink, copied, pushed into an appsrc (here to nowhere)."""
    caps = f"video/x-raw,format=YUY2,width={width},height={height},framerate={fps}/1"
    decoded = Gst.parse_launch(f"videotestsrc is-live=true pattern=ball ! {caps} ! "
                               f"appsink name=decoded emit-signals=true sync=false max-buffers=2 drop=true")
    writer = Gst.parse_launch(f"appsrc name=frames is-live=true do-timestamp=true format=time caps={caps} ! "
                              f"fakesink sync=false")
    frames = writer.get_by_name("frames")

    def frame(sink):
        buffer = sink.emit("pull-sample").get_buffer().copy()
        buffer.pts = buffer.dts = Gst.CLOCK_TIME_NONE
        frames.emit("push-buffer", buffer)
        return Gst.FlowReturn.OK

    decoded.get_by_name("decoded").connect("new-sample", frame)
    writer.set_state(Gst.State.PLAYING)
    decoded.set_state(Gst.State.PLAYING)
    return decoded, writer


def main():
    host, port, seconds, bitrate, size = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
    extra = sys.argv[6:]
    Gst.init(None)
    if "camera" in extra:
        camera = camera_load(1920, 1080, 30)  # noqa: F841 - running while main() does
    app_id = next(iter(link.Paired().all()))[:16]
    targets = [(host, port)]
    stopped = threading.Event()

    def speakers():  # as devices.Feed's session
        tablet = link.connect(targets, app_id)
        sender = pcm.PcmSender("audiotestsrc is-live=true wave=sine freq=440 volume=0.3 samplesperbuffer=240")
        sender.peer = tablet.peer
        reply = tablet.request({"type": "speakers", "sdp": sender.offer()})
        sender.answer(str(reply.get("sdp", "")))
        tablet.hold(until=stopped.is_set, tick=1.0)
        try:
            tablet.send({"type": "stop"})
        except OSError:
            pass
        sender.close()
        tablet.close()

    threading.Thread(target=speakers, daemon=True).start()
    width, height = map(int, size.split("x"))
    session = appdisplay.AppDisplay(targets, app_id, (width, height, 60), bitrate, media.pick_encoder("auto"),
                                    touch=False, test=True)
    session.start()
    if "input" in extra:
        def take_input():
            if session.streamer is None:
                return True
            session.streamer.input = appdisplay.Input(Nothing(), (width, height), lambda: Nothing())
            return False
        GLib.timeout_add(100, take_input)
    loop = GLib.MainLoop()

    def end():
        stopped.set()
        session.stop()
        GLib.timeout_add(1500, loop.quit)
        return False

    GLib.timeout_add_seconds(seconds, end)
    loop.run()


if __name__ == "__main__":
    main()
