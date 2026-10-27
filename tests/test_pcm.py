"""The speakers' sound as it is: 5 ms packets of 16-bit stereo PCM, sealed."""
import json
import socket
import struct

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
