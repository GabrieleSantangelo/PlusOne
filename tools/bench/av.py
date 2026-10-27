"""Python's side of tools/bench/av.sh: a test pattern and a test tone to the
fake tablet at once, the way Plus One sends the desktop and the speakers
(appdisplay.AppDisplay; devices.Feed with a pcm.PcmSender): the same
threads. av.py HOST PORT SECONDS BITRATE SIZE"""
import sys
import threading

import gi

gi.require_version("Gst", "1.0")
from gi.repository import GLib, Gst  # noqa: E402

from plusone import appdisplay, link, media, pcm  # noqa: E402


def main():
    host, port, seconds, bitrate, size = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
    Gst.init(None)
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
