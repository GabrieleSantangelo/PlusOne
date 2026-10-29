"""The speakers' sound as it is: 5 ms packets of 16-bit stereo PCM, sealed."""
import json
import socket
import struct
import threading
import time
import types

import pytest

crypto = pytest.importorskip("cryptography.hazmat.primitives.ciphers.aead")

import gi  # noqa: E402

gi.require_version("Gst", "1.0")
from gi.repository import Gst  # noqa: E402

from plusone import pcm  # noqa: E402

Gst.init(None)


def test_the_sound_goes_in_whole_5_ms_packets_as_it_is():
    app = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    app.bind(("127.0.0.1", 0))
    app.settimeout(3)
    # 2400 frames of a known pattern: exactly ten packets.
    sender = pcm.PcmSender("audiotestsrc wave=ticks num-buffers=10 samplesperbuffer=240 is-live=true")
    offer = json.loads(sender.offer())
    assert offer["rate"] == 48000 and offer["channels"] == 2 and offer["frames"] == 240
    sender.peer = "127.0.0.1"
    sender.answer(json.dumps({"port": app.getsockname()[1]}))
    try:
        packets = [app.recv(2048) for _ in range(10)]
    finally:
        sender.close()
        app.close()
    aead = crypto.AESGCM(bytes.fromhex(offer["key"]))
    numbers = []
    for packet in packets:
        direction, _count = struct.unpack_from("!IQ", packet)
        body = aead.decrypt(packet[:12], packet[12:], None)
        kind, number = pcm.HEADER.unpack_from(body)
        assert direction == 1 and kind == pcm.PCM and len(body) == pcm.HEADER.size + pcm.BYTES
        numbers.append(number)
    assert numbers == list(range(10)) and sender.losses()[0] == 10


def test_an_answer_without_the_app_s_address_or_a_port_is_refused():
    sender = pcm.PcmSender("audiotestsrc num-buffers=1")
    try:
        with pytest.raises(ValueError):
            sender.answer(json.dumps({"port": 4000}))  # no address from the paired link
        sender.peer = "127.0.0.1"
        with pytest.raises(ValueError):
            sender.answer(json.dumps({"port": 70000}))
    finally:
        sender.close()


def test_silence_stops_going_after_a_second_and_sound_goes_again():
    # A paused player that keeps its stream: a second of zeros goes (the
    # app plays them out), then nothing until there is sound again.
    sender = pcm.PcmSender("audiotestsrc num-buffers=1")
    sent = []
    sender.channel.send = sent.append
    try:
        def push(data):
            buffer = Gst.Buffer.new_wrapped(data)
            sample = Gst.Sample.new(buffer, None, None, None)
            sender._send(type("Sink", (), {"emit": lambda self, signal: sample})())

        for _ in range(pcm.QUIET_AFTER + 50):
            push(pcm.SILENCE)
        assert len(sent) == pcm.QUIET_AFTER and sender.captured == pcm.QUIET_AFTER + 50
        push(b"\x01\x00" * (pcm.BYTES // 2))
        assert len(sent) == pcm.QUIET_AFTER + 1
        _kind, number = pcm.HEADER.unpack_from(sent[-1])
        assert number == pcm.QUIET_AFTER and sender.losses()[0] == pcm.QUIET_AFTER + 51
    finally:
        sender.close()


def test_the_app_s_reports_of_each_version_are_read():
    report = struct.pack("!BIIIIII", pcm.STATS, 1920, 2, 3, 4, 7000, 1)
    assert pcm.app_report(report) == (1920, 2, 3, 4, 7000, 1)  # 0.9: the gaps heard too
    assert pcm.app_report(report[:21]) == (1920, 2, 3, 4, 7000, 0)
    assert pcm.app_report(report[:13]) == (1920, 2, 3, 0, 0, 0)
    assert pcm.app_report(report[:17]) is None
    assert pcm.app_report(b"\x07" + report[1:]) is None and pcm.app_report(None) is None


def opus_sender(codecs, kbps):
    """A sender over the link (its channel faked), its app playing codecs, the link's budget kbps."""
    sender = pcm.PcmSender("audiotestsrc wave=sine is-live=true samplesperbuffer=240")
    sent, watched = [], []
    budget = types.SimpleNamespace(kbps=lambda: kbps, watch=watched.append)
    sender.channel = types.SimpleNamespace(send=sent.append, budget=budget, receive=lambda timeout: time.sleep(0.05),
                                           close=lambda: None)
    sender._over_link = True
    sender.answer(json.dumps({"codecs": codecs}))
    return sender, sent, watched


def until(condition, seconds=5):
    deadline = time.monotonic() + seconds
    while not condition():
        assert time.monotonic() < deadline, "never"
        time.sleep(0.01)


def test_under_a_low_budget_the_sound_goes_in_opus_and_as_it_is_again_after():
    sender, sent, watched = opus_sender(["pcm", "opus"], 5000)
    try:
        assert sender.codec == "opus" and watched == [sender.follow]
        until(lambda: len(sent) >= 40)
        assert {body[0] for body in sent} == {pcm.OPUS}
        assert all(len(body) < 200 for body in sent)  # 160 kbit/s: about 100 bytes each, not 965
        numbers = [pcm.HEADER.unpack_from(body)[1] for body in sent]
        assert numbers == list(range(numbers[0], numbers[0] + len(numbers)))
        # The budget up: PCM again only once it held PCM_AFTER.
        sender.follow(15000)
        time.sleep(0.2)
        assert sender.codec == "opus"
        sender._pcm_at = time.monotonic()  # (as if it held that long already)
        until(lambda: sent[-1][0] == pcm.PCM)
        first = next(i for i, body in enumerate(sent) if body[0] == pcm.PCM)
        assert all(body[0] == pcm.PCM and len(body) == pcm.HEADER.size + pcm.BYTES for body in sent[first:])
        numbers = [pcm.HEADER.unpack_from(body)[1] for body in sent]
        assert numbers == list(range(numbers[0], numbers[0] + len(numbers)))  # one count, whatever the codec
        # Down again: Opus at once.
        sender.follow(4000)
        assert sender.codec == "opus"
    finally:
        sender.close()


def test_an_app_that_plays_no_opus_gets_the_sound_as_it_is():
    sender, sent, watched = opus_sender(None, 2000)
    try:
        assert watched == [] and sender.codec == "pcm"
        until(lambda: len(sent) >= 10)
        assert {body[0] for body in sent} == {pcm.PCM}
    finally:
        sender.close()


@pytest.mark.parametrize("asked, told", [(900, None), (None, 900)])
def test_on_bluetooth_s_budget_the_sound_goes_in_opus(asked, told):
    # plus-one-link's budget on a Bluetooth path: what its stream drains,
    # 0.9 Mbit/s until measured, asked as the speakers start (devices) or
    # told after (an event). It said none (null) before, and the speakers
    # sent PCM's 1.7 Mbit/s on a link of about 1.
    channels = pytest.importorskip("plusone.link.channels")
    device = "6a2665bc" * 8
    watches = []

    class Link:
        def devices(self):
            return [{"id": device, "rate_kbps": asked,
                     "paths": [{"id": 1, "kind": "bt", "transport": "stream", "state": "active",
                                "budget_bps": asked and asked * 1000, "budget_from": "prior"}]}]

        def watch(self, on_event):
            watches.append(on_event)
            ended = threading.Event()
            return types.SimpleNamespace(thread=types.SimpleNamespace(join=ended.wait), close=ended.set)

    budget = channels.Budget(Link(), device)
    sent = []
    sender = pcm.PcmSender("audiotestsrc wave=sine is-live=true samplesperbuffer=240")
    sender.channel = types.SimpleNamespace(send=sent.append, budget=budget, receive=lambda timeout: time.sleep(0.05),
                                           close=budget.close)
    sender._over_link = True
    try:
        sender.answer(json.dumps({"codecs": ["pcm", "opus"]}))
        if told:
            assert sender.codec == "pcm"  # none yet: PCM
            watches[0]({"event": "budget", "device": device, "kbps": told})
        assert sender.codec == "opus"
        until(lambda: len(sent) >= 20)
        assert {body[0] for body in sent[-10:]} == {pcm.OPUS}
    finally:
        sender.close()
