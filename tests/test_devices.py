"""The tablet's camera and microphone as the PC's: who uses them, and the tablet asked only then."""
import collections
import threading
import time
import types

import gi

gi.require_version("Gst", "1.0")
from gi.repository import GLib  # noqa: E402

from plusone import devices, link  # noqa: E402


def spin(until, timeout=5):
    end = time.monotonic() + timeout
    while time.monotonic() < end and not until():
        GLib.MainContext.default().iteration(False)
        time.sleep(0.005)
    return until()


def test_links_from_pw_link():
    parse = devices.LinkParser()
    assert parse("= plusone_mic:capture_MONO") is None
    # One there already, under its port; then one made and removed.
    assert parse("=   |-> pw-record:input_MONO") == ("=", ("plusone_mic:capture_MONO", "pw-record:input_MONO"))
    assert parse("+ plusone_mic:capture_MONO -> Chrome:input_MONO") == \
        ("+", ("plusone_mic:capture_MONO", "Chrome:input_MONO"))
    assert parse("- plusone_mic:capture_MONO -> Chrome:input_MONO")[0] == "-"
    assert parse("garbage") is None


def test_two_streams_of_one_program_are_two_links():
    # Chrome's new stream linked before its old one goes: the same ports' names.
    links = collections.Counter()
    chrome = ("Google Chrome:output_FL", devices.SPEAKERS_NAME + ":playback_FL")
    devices.counted(links, "=", chrome)
    devices.counted(links, "+", chrome)
    devices.counted(links, "-", chrome)
    assert links  # the new one's, still playing
    devices.counted(links, "-", chrome)
    devices.counted(links, "-", chrome)  # one never seen made: none, not less
    assert not links
    devices.counted(links, "+", chrome)
    assert links


def test_level_meters_do_not_record():
    def node(id, **props):
        return {"id": id, "type": "PipeWire:Interface:Node", "info": {"props": props}}

    def link(out, into):
        return {"type": "PipeWire:Interface:Link", "info": {"output-node-id": out, "input-node-id": into}}
    objects = [node(1, **{"node.name": devices.MIC_NAME}), node(2, **{"node.name": "plusone-mic-feed-1"}),
               node(3, **{"node.name": "GNOME Settings", "media.name": "Peak detect", "stream.monitor": True}),
               node(4, **{"node.name": "Google Chrome input"}), link(2, 1), link(1, 3)]
    assert devices.recorders(objects) == []  # Plus One's own stream in, a level meter out
    chrome = node(5, **{"node.name": "Chrome input", "application.name": "Google Chrome"})
    assert devices.recorders(objects + [link(1, 4)]) == ["Google Chrome input"]
    assert devices.recorders(objects + [chrome, link(1, 5)]) == ["Google Chrome"]


def test_opening_a_device_is_counted(tmp_path):
    path = tmp_path / "video9"
    path.write_bytes(b"")
    counts = []
    opened = devices.Opened(str(path), counts.append)
    try:
        first = open(path)
        assert spin(lambda: counts and counts[-1] == 1)
        second = open(path, "rb")
        assert spin(lambda: counts[-1] == 2)
        first.close()
        assert spin(lambda: counts[-1] == 1)
        second.close()
        assert spin(lambda: counts[-1] == 0)
    finally:
        opened.close()


def test_a_program_with_it_open_before_is_counted(tmp_path, monkeypatch):
    path = tmp_path / "video9"
    path.write_bytes(b"")
    before = open(path)  # a program's, before Plus One watched
    monkeypatch.setattr(devices, "holders", lambda p: 1 if p == str(path) else 0)
    counts = []
    opened = devices.Opened(str(path), counts.append)
    try:
        ours = open(path)  # Plus One's writer
        assert spin(lambda: counts and counts[-1] == 1)
        opened.started()
        assert (opened.baseline, opened.count) == (1, 2)
        before.close()
        assert spin(lambda: counts[-1] == 1)
        # Its closing seen; the next program's opening shows.
        after = open(path)
        assert spin(lambda: counts[-1] == 2)
        after.close()
        ours.close()
        assert spin(lambda: counts[-1] == 1)  # never below Plus One's own
    finally:
        opened.close()


def test_holders_are_other_processes(tmp_path):
    path = tmp_path / "video9"
    path.write_bytes(b"")
    with open(path):
        assert devices.holders(str(path)) == 0  # this process's own: not counted


class FakeApp:
    def __init__(self):
        self.changes = 0
        tablet = types.SimpleNamespace(app_id="a1b2" + "0" * 12)
        self.tablets = types.SimpleNamespace(active=lambda: tablet, addresses=lambda t: [("10.0.0.2", 47236)],
                                             targets=lambda t: [("10.0.0.2", 47236)])

    def devices_changed(self):
        self.changes += 1

    def notify(self, key, title, body, button=None, **args):
        self.notes = getattr(self, "notes", []) + [title]


class Probe(devices.Device):
    def __init__(self, app):
        super().__init__(app, "camera")
        self.calls = []

    def begin(self):
        self.calls.append("begin")

    def end(self):
        self.calls.append("end")


def test_the_tablet_only_while_a_program_uses_it(monkeypatch):
    monkeypatch.setattr(devices, "IN_USE_AFTER", 0.01)
    monkeypatch.setattr(devices, "IDLE_AFTER", 0.05)
    device = Probe(FakeApp())
    device.state = "ready"
    device._used(True)
    assert spin(lambda: device.calls == ["begin"]) and device.state == "in use"
    # A program that opens it to look at it and closes it at once: nothing.
    device._used(False)
    device._used(True)
    spin(lambda: False, timeout=0.15)
    assert device.calls == ["begin"]
    device._used(False)
    assert spin(lambda: device.calls == ["begin", "end"]) and device.state == "ready"


def test_nothing_while_switched_off(monkeypatch):
    monkeypatch.setattr(devices, "IN_USE_AFTER", 0.01)
    device = Probe(FakeApp())
    device._used(True)
    spin(lambda: False, timeout=0.1)
    assert device.calls == [] and device.state == "off"


class FakeWire:
    """The paired link as the feed uses it: what it sends, the app's answer."""

    def __init__(self, reply):
        self.sent, self.reply, self.hung_up = [], reply, threading.Event()

    def send(self, message, payload=None):
        self.sent.append(message)

    def receive(self, *args):
        if self.reply is not None:
            reply, self.reply = self.reply, None
            return reply
        raise EOFError

    def readable(self, timeout):
        return self.hung_up.wait(timeout)


class FakeSock:
    def __init__(self, wire):
        self.wire = wire

    def getpeername(self):
        return ("10.0.0.2", 47236)

    def shutdown(self, how):
        self.wire.hung_up.set()

    def close(self):
        pass


def fake_link(wire):
    """A Link over a scripted wire: what link.connect() gives."""
    return link.Link(FakeSock(wire), wire, "Tab", "a1b2" + "0" * 60, {"version": link.PROTOCOL})


class FakeReceiver:
    def __init__(self):
        self.answered, self.closed, self.broken = None, False, None

    def offer(self):
        return "v=0 offer"

    def answer(self, sdp):
        self.answered = sdp

    def failed(self):
        return self.broken

    def losses(self):
        self.received = getattr(self, "received", 0) + 50
        return self.received, 0, 0, 0

    def close(self):
        self.closed = True


def test_the_feed_offers_and_hangs_up(monkeypatch):
    wire = FakeWire({"type": "answer", "sdp": "v=0 answer"})
    opened = []
    monkeypatch.setattr(link, "connect", lambda targets, expect: opened.append((targets, expect))
                        or fake_link(wire))
    receivers, ready = [], []
    feed = devices.Feed({"type": "microphone"}, lambda: ([("10.0.0.2", 47236)], "a1b2"),
                        lambda: receivers.append(FakeReceiver()) or receivers[-1], lambda: ready.append(1),
                        lambda reason: None)
    feed.start()
    assert spin(lambda: ready)
    use = wire.sent[0]["use"]  # one use: what the tablet's Stop holds until the next
    assert len(use) == 8 and use != devices.Feed({"type": "microphone"}, None, None, None, None).request["use"]
    assert wire.sent == [{"type": "microphone", "use": use, "sdp": "v=0 offer"}] and receivers[0].answered == "v=0 answer"
    assert opened == [([("10.0.0.2", 47236)], "a1b2")]
    feed.stop()
    feed._thread.join(5)
    assert receivers[0].closed and len(receivers) == 1


def test_a_session_that_breaks_is_made_again_and_pings_meanwhile(monkeypatch):
    monkeypatch.setattr(devices, "RETRY", 0.05)
    monkeypatch.setattr(link.link, "KEEPALIVE", 0.1)
    wires = []

    def connect(targets, expect):
        wires.append(FakeWire({"type": "answer", "sdp": "v=0 answer"}))
        return fake_link(wires[-1])
    monkeypatch.setattr(link, "connect", connect)
    receivers = []
    feed = devices.Feed({"type": "microphone"}, lambda: ([("10.0.0.2", 47236)], "a1b2"),
                        lambda: receivers.append(FakeReceiver()) or receivers[-1], lambda: None, lambda reason: None)
    feed.start()
    assert spin(lambda: {"type": "ping"} in (wires[0].sent if wires else []))
    receivers[0].broken = "pipewiresink: no stream"
    assert spin(lambda: len(receivers) == 2)
    feed.stop()
    feed._thread.join(5)
    assert receivers[0].closed and receivers[1].closed


def test_pipewire_going_away_makes_the_source_again(monkeypatch):
    later = []
    monkeypatch.setattr(devices.GLib, "timeout_add_seconds", lambda seconds, call: later.append(call))
    app = FakeApp()
    applied = []
    app.devices = types.SimpleNamespace(apply=lambda: applied.append(1))
    mic = devices.Microphone(app)
    watch = types.SimpleNamespace(force_exit=lambda: None)
    mic.module, mic.watch, mic.state = "536870913", watch, "ready"
    unloaded = []
    monkeypatch.setattr(devices.subprocess, "run", lambda *args, **kwargs: unloaded.append(args))
    ended = types.SimpleNamespace(read_line_finish_utf8=lambda result: (None, 0))
    mic._line(ended, None, watch)
    # Not unloaded by its number: after PipeWire's restart it may be another module's.
    assert mic.state == "off" and mic.module is None and unloaded == []
    later.pop()()
    assert applied == [1]


def test_a_refusal_is_said_and_asked_again_later(monkeypatch):
    monkeypatch.setattr(devices, "REFUSED_RETRY", 0.05)
    tries, problems = [], []

    def connect(targets, expect):
        tries.append(1)
        return fake_link(FakeWire({"type": "refused", "reason": "permission"}))
    monkeypatch.setattr(link, "connect", connect)
    feed = devices.Feed({"type": "camera"}, lambda: ([("10.0.0.2", 47236)], "a1b2"),
                        FakeReceiver, lambda: None, problems.append)
    feed.start()
    assert spin(lambda: len(tries) >= 2)
    feed.stop()
    feed._thread.join(5)
    assert problems[0] == "permission"


def test_no_tablet_on_the_network(monkeypatch):
    monkeypatch.setattr(devices, "RETRY", 0.05)
    problems = []
    feed = devices.Feed({"type": "camera"}, lambda: ([], "a1b2"), FakeReceiver, None, problems.append)
    feed.start()
    assert spin(lambda: problems)
    feed.stop()
    feed._thread.join(5)
    assert problems[0] == "away"


def test_a_new_size_waits_until_no_program_uses_the_camera(monkeypatch):
    later = []
    monkeypatch.setattr(devices.GLib, "idle_add", lambda call: later.append(call))
    calls = []
    manager = devices.Devices.__new__(devices.Devices)
    manager.app = types.SimpleNamespace(devices=manager)
    camera = devices.Camera(FakeApp())
    camera.app = manager.app
    camera.disable = lambda: calls.append("disable")
    manager.camera = camera
    manager.apply = lambda: calls.append("apply")
    camera._in_use = True
    manager.restart_camera()
    assert calls == [] and camera.restart_when_idle  # the call goes on as it is
    camera._in_use = False
    camera.end()  # the program let go
    later.pop()()
    assert calls == ["disable", "apply"] and not camera.restart_when_idle


def test_the_voice_chain_by_what_is_there(monkeypatch):
    every = {devices.DEEP_FILTER: "ladspa-x-deep-filter-mono", devices.COMPRESSOR: "ladspa-x-sc4m",
             devices.LIMITER: "ladspa-x-fastlookaheadlimiter"}
    monkeypatch.setattr(devices, "ladspa", every.get)
    chain, noise = devices.voice("neural")
    assert noise == "neural" and "leaky=downstream ! ladspa-x-deep-filter-mono min-processing-threshold" in chain
    assert "ladspa-x-sc4m" in chain and "ladspa-x-fastlookaheadlimiter limit=-1" in chain
    chain, noise = devices.voice("light")
    assert noise == "light" and "deep-filter" not in chain and "sc4m" in chain
    # No DeepFilterNet: the tablet's noise reduction instead.
    monkeypatch.setattr(devices, "ladspa", {devices.COMPRESSOR: "c", devices.LIMITER: "l"}.get)
    assert devices.voice("neural")[1] == "light"
    # No swh-plugins: only a gain, no compressor that bends samples.
    monkeypatch.setattr(devices, "ladspa", lambda suffix: None)
    assert devices.voice("off") == (devices.PLAIN_LEVEL, "off")


def test_the_tablet_is_told_whether_to_take_the_noise_out(monkeypatch):
    requests = []

    class FakeFeed:
        def __init__(self, request, *args):
            requests.append(request)

        def start(self):
            pass

        def stop(self):
            pass
    monkeypatch.setattr(devices, "Feed", FakeFeed)
    app = FakeApp()
    app.settings = types.SimpleNamespace(microphone_noise="light")
    mic = devices.Microphone(app)
    for noise, told in (("light", True), ("neural", False), ("off", False)):
        monkeypatch.setattr(devices, "voice", lambda wanted, noise=noise: ("volume", noise))
        mic.begin()
        mic.end()
    assert [r["noise_reduction"] for r in requests] == [True, False, False]
    assert all(r["type"] == "microphone" for r in requests)


def test_the_voice_chain_parses():
    gi.require_version("Gst", "1.0")
    from gi.repository import Gst
    Gst.init(None)
    if not (devices.ladspa(devices.COMPRESSOR) and devices.ladspa(devices.LIMITER)):
        import pytest
        pytest.skip("no swh-plugins here")
    for noise in devices.NOISE:
        chain, _noise = devices.voice(noise)
        # As Microphone._on_pad has it: caps either side.
        Gst.parse_bin_from_description(f"audioconvert ! audio/x-raw,format=F32LE,rate=48000,channels=1 ! {chain} "
                                       "! audioconvert", True)


def test_switched_on_only_with_a_tablet(monkeypatch):
    calls = []
    app = types.SimpleNamespace(settings=types.SimpleNamespace(camera=True, microphone=False, speakers=False),
                                tablets=types.SimpleNamespace(active=lambda: None, addresses=lambda t: []))
    manager = devices.Devices.__new__(devices.Devices)
    manager.app, manager.tablet = app, None
    manager.camera = types.SimpleNamespace(enable=lambda: calls.append("camera on"),
                                           disable=lambda: calls.append("camera off"),
                                           renew=lambda: calls.append("camera renewed"))
    manager.microphone = types.SimpleNamespace(enable=lambda: calls.append("mic on"),
                                               disable=lambda: calls.append("mic off"))
    manager.speakers = types.SimpleNamespace(disable=lambda: None)
    manager.apply()
    assert calls == ["camera off", "mic off"]  # no tablet paired: neither
    one, other = types.SimpleNamespace(fingerprint="aa" * 32), types.SimpleNamespace(fingerprint="bb" * 32)
    app.tablets.active = lambda: one
    calls.clear()
    manager.apply()
    assert calls == ["camera on", "camera renewed", "mic off"]
    calls.clear()
    manager.apply()  # the same tablet: a session in progress goes on
    assert calls == ["camera on", "mic off"]
    app.tablets.active = lambda: other  # another tablet in use: not one more moment from the first
    calls.clear()
    manager.apply()
    assert calls == ["camera on", "camera renewed", "mic off"]


def test_a_dropped_session_shows_black_not_the_last_picture(monkeypatch):
    # Black is written whenever the tablet's picture has not come for a moment.
    import time as clock
    camera = devices.Camera(FakeApp())
    pushed = []
    camera.frames = [types.SimpleNamespace(emit=lambda signal, buffer: pushed.append(buffer))]
    camera._black_frame = bytes((16, 128)) * 4
    camera._live_at = clock.monotonic()  # a frame just came: no black over it
    camera._black()
    assert pushed == []
    camera._live_at = clock.monotonic() - 2 * devices.LIVE_GAP  # the session dropped
    camera._black()
    assert len(pushed) == 1 and pushed[0].get_size() == 8
    camera._live_at = clock.monotonic()
    camera.end()  # the program let go: black at the next tick
    camera._black()
    assert len(pushed) == 2


def test_a_program_opening_the_camera_keeps_black_coming():
    # Black's timer and the in-use one are two: a program opening the
    # device (which starts the in-use one) must not stop black.
    camera = devices.Camera(FakeApp())
    camera.frames = [types.SimpleNamespace(emit=lambda signal, buffer: None)]
    camera.opened = [types.SimpleNamespace(path="/dev/video9", count=2, baseline=1)]
    camera._black_frame = bytes((16, 128)) * 4
    try:
        camera._pace(False)
        black = camera._black_timer
        camera._opened(None)  # a program reads: black 30 times a second
        assert camera._black_timer and camera._black_timer != black and camera._timer
        assert GLib.MainContext.default().find_source_by_id(camera._black_timer)
    finally:
        for timer in (camera._black_timer, camera._timer):
            if timer:
                GLib.source_remove(timer)


def test_a_stale_microphone_is_removed(monkeypatch):
    ran = []

    def run(args, **kwargs):
        ran.append(args)
        out = "536870913\tmodule-null-sink\tsink_name=plusone_mic media.class=Audio/Source/Virtual\n7\tmodule-x\t\n"
        return types.SimpleNamespace(stdout=out if args[1] == "list" else "")
    monkeypatch.setattr(devices.subprocess, "run", run)
    devices.unload_stale()
    assert ran[-1] == ["pactl", "unload-module", "536870913"]




def test_who_has_the_webcam_open_by_name(tmp_path):
    import subprocess
    import sys
    path = tmp_path / "video9"
    path.write_bytes(b"")
    with open(path):  # this process's own: not counted
        assert devices.holder_names([str(path)]) == []
        other = subprocess.Popen([sys.executable, "-c", f"import time; f = open({str(path)!r}); time.sleep(30)"])
        try:
            assert spin(lambda: devices.holder_names([str(path)]) != [], timeout=10)
            assert devices.holder_names([str(path)])[0].startswith("python")
        finally:
            other.kill()


def test_the_programs_playing_on_the_speakers():
    objects = [
        {"id": 40, "type": "PipeWire:Interface:Node", "info": {"props": {"node.name": devices.SPEAKERS_NAME}}},
        {"id": 41, "type": "PipeWire:Interface:Node", "info": {"props": {"application.name": "Firefox"}}},
        {"id": 42, "type": "PipeWire:Interface:Node", "info": {"props": {"node.name": "plusone-speakers-feed"}}},
        {"id": 43, "type": "PipeWire:Interface:Node", "info": {"props": {"node.name": "alsa_output.pci"}}},
        {"type": "PipeWire:Interface:Link", "info": {"output-node-id": 41, "input-node-id": 40}},
        {"type": "PipeWire:Interface:Link", "info": {"output-node-id": 41, "input-node-id": 40}},
        {"type": "PipeWire:Interface:Link", "info": {"output-node-id": 40, "input-node-id": 42}},  # its monitor, ours
        {"type": "PipeWire:Interface:Link", "info": {"output-node-id": 41, "input-node-id": 43}},  # another output
    ]
    assert devices.players(objects) == ["Firefox", "Firefox"]


def test_a_program_playing_is_a_link_into_the_speakers():
    speakers = devices.Speakers.__new__(devices.Speakers)
    assert speakers.ours(("Firefox:output_FL", devices.SPEAKERS_NAME + ":playback_FL"))
    assert not speakers.ours((devices.SPEAKERS_NAME + ":monitor_FL", "plusone-speakers-feed:input_FL"))
    microphone = devices.Microphone.__new__(devices.Microphone)
    assert microphone.ours((devices.MIC_NAME + ":capture_MONO", "Meet:input_MONO"))


def test_pw_dump_s_arrays_after_the_first_are_what_changed():
    first = '[{"id": 1, "type": "PipeWire:Interface:Node", "info": {"props": {}}}, {"id": 2, "info": {}}]'
    then = '[{"id": 2, "info": null}, {"id": 3, "type": "PipeWire:Interface:Link", "info": {}}]'
    objects = devices.pw_objects(first + "\n" + then + "\n")
    assert sorted(o["id"] for o in objects) == [1, 3]
    assert devices.pw_objects("") == []


def test_a_paused_stream_is_no_program_playing():
    # A video paused in a tab still open: its stream stays linked, "idle".
    objects = [
        {"id": 40, "type": "PipeWire:Interface:Node", "info": {"props": {"node.name": devices.SPEAKERS_NAME}}},
        {"id": 41, "type": "PipeWire:Interface:Node",
         "info": {"state": "idle", "props": {"application.name": "Firefox"}}},
        {"id": 44, "type": "PipeWire:Interface:Node",
         "info": {"state": "running", "props": {"application.name": "Chrome"}}},
        {"type": "PipeWire:Interface:Link", "info": {"output-node-id": 41, "input-node-id": 40}},
        {"type": "PipeWire:Interface:Link", "info": {"output-node-id": 44, "input-node-id": 40}},
    ]
    assert devices.players(objects) == ["Chrome"]
    objects[3]["info"]["output-node-id"] = 41  # only Firefox, paused
    objects[4]["info"]["output-node-id"] = 41
    assert devices.players(objects) == []


def test_the_tablets_own_bluetooth_audio_is_off_while_it_is_the_speakers_and_back_after(tmp_path, monkeypatch):
    tablet, earbuds = "/org/bluez/hci0/dev_54_DD", "/org/bluez/hci0/dev_88_8E"
    objects = {tablet: {"org.bluez.Device1": {"Name": "Tab", "Connected": True}},
               tablet + "/sep1/fd0": {"org.bluez.MediaTransport1": {}},
               earbuds: {"org.bluez.Device1": {"Name": "Earbuds", "Connected": True}},
               earbuds + "/sep1/fd0": {"org.bluez.MediaTransport1": {}}}
    calls = []
    monkeypatch.setattr(devices.BluetoothAudio, "STATE", str(tmp_path / "bluetooth-audio.json"))
    monkeypatch.setattr(devices, "_bluez_objects", lambda: objects)
    monkeypatch.setattr(devices, "_bluez_call", lambda path, method, uuid: calls.append((path, method, uuid)))
    audio = devices.BluetoothAudio(lambda: "Tab")
    audio._check()
    assert calls == [(tablet, "DisconnectProfile", devices.BluetoothAudio.A2DP_SOURCE)]  # the tablet's only
    del objects[tablet + "/sep1/fd0"]  # its A2DP gone
    audio._check()
    assert len(calls) == 1
    # Plus One gone without a word: the next one connects it again.
    devices.BluetoothAudio.restore()
    assert calls[-1] == (tablet, "ConnectProfile", devices.BluetoothAudio.A2DP_SOURCE)
    assert not (tmp_path / "bluetooth-audio.json").exists()


def test_a_stream_played_again_is_looked_at_again_a_moment_later():
    # pactl tells the change before PipeWire runs the node (still "idle"):
    # looked at at once, and again AGAIN_AFTER_EVENT later.
    looked = []
    pauses = object()
    lines = ["Event 'change' on sink-input #3483"]
    stream = types.SimpleNamespace(read_line_finish_utf8=lambda result: (lines.pop(0) if lines else None, 0),
                                   read_line_async=lambda *args: None)
    device = types.SimpleNamespace(pauses=pauses, links={("Chrome:output_FL", "plusone_speakers:playback_FL")},
                                   _who_records=lambda: looked.append(time.monotonic()), _event=None)
    device._look_again = lambda: devices.AudioDevice._look_again(device)
    devices.AudioDevice._event(device, stream, None, pauses)
    assert len(looked) == 1
    assert spin(lambda: len(looked) == 3)
    assert looked[1] - looked[0] >= devices.AGAIN_AFTER_EVENT / 1000 - 0.05


def test_the_tablets_speakers_stay_the_output_across_a_restart(tmp_path, monkeypatch):
    made, default = [], ["plusone_speakers"]
    monkeypatch.setattr(devices.Speakers, "WAS_OUTPUT", str(tmp_path / "speakers-were-the-output"))
    monkeypatch.setattr(devices, "default_sink", lambda: default[0])
    monkeypatch.setattr(devices, "make_default_sink", made.append)
    monkeypatch.setattr(devices.AudioDevice, "enable", lambda self: setattr(self, "module", "42"))
    monkeypatch.setattr(devices.AudioDevice, "disable", lambda self: setattr(self, "module", None))
    monkeypatch.setattr(devices.BluetoothAudio, "hold", lambda self: None)
    monkeypatch.setattr(devices.BluetoothAudio, "release", lambda self: None)
    speakers = devices.Speakers(FakeApp())
    speakers.enable()
    assert made == []  # nothing remembered: the user's choice stands
    speakers.disable()  # Plus One goes (an update), its output the tablet's
    default[0] = "bluez_output.earbuds"  # the PC's sound moved meanwhile
    speakers.enable()
    assert made == ["plusone_speakers"]
    speakers.disable()
    speakers.enable()
    assert made == ["plusone_speakers"]  # not the output when it went: left alone
