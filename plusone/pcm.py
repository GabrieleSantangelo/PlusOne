"""The PC's sound to the tablet's speakers, as it is: 16-bit stereo PCM at
48 kHz (no codec at all), in UDP packets of 5 ms sealed with AES-GCM under a
key the paired link hands over (link.Datagrams). The app plays it with a
plain media AudioTrack, as the tablet's own players do (PcmPlayback.kt).
libwebrtc, which played it before, is made for calls: its jitter buffer
stretched and squeezed the music to keep the delay down, and it sounded so.

Digital silence (a player paused, its stream left open: a browser's, a
chat program's) goes for a second, then nothing until there is sound again:
the app pauses meanwhile and starts again with its little wait.

Shaped for devices.Feed as an rtc.Receiver is: offer() and answer() carry
the session's key and ports (JSON) where WebRTC's carry SDP; failed(),
losses() (here: the sound captured) and close(). Over the link
(plus-one-link, docs/LINK.md) the packets go on its "pcm" channel instead
(use_link), and the offer carries no key nor port.

PCM is 1.7 Mbit/s on the network: half of a weak phone's hotspot. Over the
link, while its budget is under OPUS_UNDER (its congestion controller's,
told as it moves), the same 5 ms packets go in Opus instead (OPUS_KBPS, its
low-delay mode), to an app that says it plays it; PCM again once the
budget held over PCM_OVER for PCM_AFTER (architecture: PCM where the
bandwidth abounds, the cable, Wi-Fi Direct). The encoder runs only then (a
valve before it): nothing spent on it otherwise.
"""
import json
import os
import struct
import threading
import time

from gi.repository import GLib, Gst

from . import log
import plusone_link
from .link import Datagrams

PCM = 7                  # the packets' kind (after appdisplay's)
OPUS = 8                 # the same 5 ms, in Opus
OPUS_KBPS = 160
OPUS_UNDER = 3000        # kbit/s of the link's budget: PCM (1.5 Mbit/s) would be half of it and more. (8000: a
                         # still screen leaves the rate at its start, 10 Mbit/s, and each cut a Wi-Fi's stall
                         # made took the sound to Opus and back, heard as it going up and down.)
PCM_OVER = 5000          # kbit/s...
# The sound on a Bluetooth reserve when there is one (PLUSONE_AUDIO_BT=0: never), looked for each so often (s).
BLUETOOTH_AUDIO = os.environ.get("PLUSONE_AUDIO_BT", "1").strip() != "0"
BLUETOOTH_EVERY = 2.0
PCM_AFTER = 10.0         # ...held this long (s): PCM again
STATS = 5                # the app's report (appdisplay.STATS's number, its own layout)
RATE, CHANNELS = 48000, 2
FRAMES = 240             # per packet: 5 ms
BYTES = FRAMES * CHANNELS * 2
HEADER = struct.Struct("!BI")  # kind, packet number
SILENCE = bytes(BYTES)
QUIET_AFTER = 200        # packets of silence in a row (a second) after which none go
STATS_EVERY = 30         # the app's reports (one a second) logged: one in so many, and each with a loss


def app_report(body):
    """(held, missed, late, left out, behind, gaps) from an app's STATS, in
    frames and packets; what it does not say, 0. None if it is not one."""
    if not body or body[0] != STATS:
        return None
    if len(body) == 25:
        return struct.unpack_from("!IIIIII", body, 1)
    if len(body) == 21:
        return struct.unpack_from("!IIIII", body, 1) + (0,)
    if len(body) == 13:
        return struct.unpack_from("!III", body, 1) + (0, 0, 0)
    return None


class PcmSender:
    def __init__(self, source):
        self.channel = Datagrams()
        self.key = self.channel.key
        self.peer = None      # the app's address: set by Feed from the paired link
        self.broken = None
        self.sent = 0
        self.captured = 0     # buffers of sound from PipeWire
        self._quiet = 0       # packets of silence in a row
        self._pending = b""
        self._running = True
        # Both branches in the source's thread (nothing in them waits): the
        # sound as it is, and Opus behind a valve, shut while PCM goes.
        self.pipeline = Gst.parse_launch(
            f"{source} ! audioconvert ! audioresample "
            f"! audio/x-raw,format=S16LE,rate={RATE},channels={CHANNELS},layout=interleaved ! tee name=t "
            f"t. ! appsink name=out emit-signals=true sync=false async=false max-buffers=0 "
            f"t. ! valve name=opus-on drop=true "
            f"! opusenc frame-size={FRAMES * 1000 // RATE} bitrate={OPUS_KBPS * 1000} audio-type=restricted-lowdelay "
            f"! appsink name=opus emit-signals=true sync=false async=false max-buffers=0")
        self._sink = self.pipeline.get_by_name("out")
        self._handler = self._sink.connect("new-sample", self._send)
        self._opus_sink = self.pipeline.get_by_name("opus")
        self._opus_handler = self._opus_sink.connect("new-sample", self._send_opus)
        self._valve = self.pipeline.get_by_name("opus-on")
        self._listener = threading.Thread(target=self._listen, daemon=True)
        self._over_link = False
        self.codecs = {"pcm"}  # what the app plays (its answer)
        self.codec = "pcm"
        self._pcm_at = None   # when PCM goes again (the budget high since)
        self._bt = False      # a Bluetooth reserve is up: the sound goes on it, in Opus
        self._bluetooth = None  # link/bt.py's, the path this sender made

    def use_link(self, tablet):
        """The packets on the link's "pcm" channel (tablet: a
        channels.ChannelLink), not this session's own UDP socket; Opus or
        PCM after its budget."""
        self.channel.close()
        self.channel = tablet.packets("pcm")
        self._over_link = True

    def offer(self):
        if self._over_link:
            return json.dumps({"rate": RATE, "channels": CHANNELS, "frames": FRAMES})
        return json.dumps({"key": self.key.hex(), "port": self.channel.port, "rate": RATE,
                           "channels": CHANNELS, "frames": FRAMES})

    def answer(self, text):
        """The app's answer: its port (JSON), over the link of before; the
        codecs it plays besides PCM."""
        reply = json.loads(text) if text else {}
        if not self._over_link:
            port = int(reply["port"])
            if not self.peer or not 0 < port < 65536:
                raise ValueError("the app's answer is not one")
            self.channel.app = (self.peer, port)
        codecs = reply.get("codecs")
        self.codecs = {"pcm"} | ({c for c in codecs if isinstance(c, str)} if isinstance(codecs, list) else set())
        budget = getattr(self.channel, "budget", None)
        if budget is not None and "opus" in self.codecs:
            self.follow(budget.kbps())
            budget.watch(self.follow)
            if BLUETOOTH_AUDIO:
                threading.Thread(target=self._bring_up_bluetooth, args=(budget,), daemon=True).start()
                threading.Thread(target=self._watch_bluetooth, args=(budget,), daemon=True).start()
        self.pipeline.set_state(Gst.State.PLAYING)
        self._listener.start()

    def _bring_up_bluetooth(self, budget):
        """The Bluetooth path with the tablet, if none is up (link/bt.py: over
        the bond the two made in their Bluetooth settings; the app listens
        under Plus One's UUID): a reserve of the session, for the sound."""
        try:
            from .link.bt import Bluetooth
            device = next((d for d in budget.connection.devices() if d["id"] == budget.device), {})
            if not device.get("name") or budget.bluetooth_reserve():
                return
            bt = Bluetooth(device["name"], budget.connection)
            registered, failed = threading.Event(), []

            def register():  # BlueZ calls the profile on the main loop
                try:
                    bt.register()
                except Exception as e:  # noqa: BLE001 - said below
                    failed.append(e)
                registered.set()
                return False

            GLib.idle_add(register)
            registered.wait(10)
            if failed:
                raise failed[0]
            self._bluetooth = bt
            bt.up()
        except Exception as e:  # noqa: BLE001 - no Bluetooth: the sound stays on the Wi-Fi
            log(f"speakers: no Bluetooth path ({e})")

    def _watch_bluetooth(self, budget):
        """While the session keeps a Bluetooth path as a reserve (plus-one-link
        sends sound over it: Session::steer_audio), the sound goes in Opus:
        PCM, 1.7 Mbit/s, does not fit its 900 kbit/s, and the Wi-Fi's stalls
        are not the sound's (the buffer ran dry at each, heard as the volume
        falling and rising)."""
        while self._running:
            try:
                up = budget.bluetooth_reserve()
            except (OSError, ValueError, plusone_link.LinkError):
                up = False
            if up != self._bt:
                self._bt = up
                log(f"speakers: {'on' if up else 'off'} the Bluetooth path")
                self.follow(budget.kbps())
            time.sleep(BLUETOOTH_EVERY)

    def follow(self, kbps, now=None):
        """The link's budget (kbit/s; None: a path with no controller of its
        own, TCP's): Opus at once under OPUS_UNDER, PCM again once over
        PCM_OVER (or none) for PCM_AFTER."""
        now = time.monotonic() if now is None else now
        if self._bt:
            self._pcm_at = None
            self._use("opus")
        elif kbps and kbps < OPUS_UNDER:
            self._pcm_at = None
            self._use("opus")
        elif self.codec == "opus" and (not kbps or kbps >= PCM_OVER):
            self._pcm_at = self._pcm_at or now + PCM_AFTER
        else:
            self._pcm_at = None

    def _use(self, codec):
        if codec != self.codec:
            log(f"speakers: in {codec.upper()} now")
            self.codec = codec
            self._valve.set_property("drop", codec != "opus")

    def _send(self, appsink):
        """Streaming thread: the sound cut into 5 ms packets, out at once."""
        sample = appsink.emit("pull-sample")
        if sample is None:
            return Gst.FlowReturn.EOS
        buf = sample.get_buffer()
        self.captured += 1
        if self._pcm_at is not None and time.monotonic() >= self._pcm_at:
            self._pcm_at = None
            self._use("pcm")
        data = self._pending + buf.extract_dup(0, buf.get_size())
        whole = len(data) - len(data) % BYTES
        for at in range(0, whole, BYTES):
            packet = data[at:at + BYTES]
            if packet != SILENCE:
                self._quiet = 0
            elif self._quiet >= QUIET_AFTER:
                continue
            else:
                self._quiet += 1
            if self.codec == "pcm":
                self.channel.send(HEADER.pack(PCM, self.sent & 0xFFFFFFFF) + packet)
                self.sent += 1
        self._pending = data[whole:]
        return Gst.FlowReturn.OK

    def _send_opus(self, appsink):
        """Streaming thread: an Opus packet of 5 ms, out at once (unless the
        sound has been silence for a second: as PCM's)."""
        sample = appsink.emit("pull-sample")
        if sample is None:
            return Gst.FlowReturn.EOS
        if self.codec == "opus" and self._quiet < QUIET_AFTER:
            buf = sample.get_buffer()
            self.channel.send(HEADER.pack(OPUS, self.sent & 0xFFFFFFFF) + buf.extract_dup(0, buf.get_size()))
            self.sent += 1
        return Gst.FlowReturn.OK

    def _listen(self):
        """The app's reports, for the log: how much it holds, what it missed,
        how far behind the PC its speaker is, the gaps heard (the speaker
        ran dry); an app of 0.8: 21 bytes, of 0.7: 13."""
        reports = 0
        while self._running:
            report = app_report(self.channel.receive(1.0))
            if report is None:
                continue
            held, missed, late, dropped, behind, gaps = report
            reports += 1
            if missed or late or gaps or reports % STATS_EVERY == 1:
                log(f"speakers: the tablet holds {held / RATE * 1000:.0f} ms"
                    + (f", its speaker is {behind / RATE * 1000:.0f} ms behind" if behind else "")
                    + f"; missed {missed} packets, {late} came late"
                    + (f", {dropped} nearly silent left out" if dropped else "")
                    + (f"; {gaps} gaps heard" if gaps else ""))

    def losses(self):
        """(buffers captured, 0, 0, 0): the sound goes as long as this grows,
        silence too (not sent, but still coming from PipeWire)."""
        return self.captured, 0, 0, 0

    def fail(self, reason):
        self.broken = self.broken or reason

    def failed(self):
        bus = self.pipeline.get_bus()
        while (message := bus.pop()) is not None:
            if message.type == Gst.MessageType.ERROR and not self.broken:
                error, _debug = message.parse_error()
                self.broken = f"{message.src.get_name()}: {error.message}"
        return self.broken

    def close(self):
        if not self._running:
            return
        self._running = False
        self.pipeline.set_state(Gst.State.NULL)
        self._sink.disconnect(self._handler)
        self._opus_sink.disconnect(self._opus_handler)
        if self._listener.is_alive():
            self._listener.join(timeout=2)
        if self._bluetooth is not None:
            threading.Thread(target=self._bluetooth.close, daemon=True).start()
            self._bluetooth = None
        self.channel.close()
