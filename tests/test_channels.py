"""plusone/link/channels.py: a payload and a long message over a feature's
control channel, in pieces (as the app's ChannelLink.kt reads and writes
them)."""
import json
import socket
import threading

import pytest

pytest.importorskip("plusone_link")
import plusone_link  # noqa: E402

from plusone.link import channels  # noqa: E402
from plusone.link.channels import CHUNK, ChannelLink  # noqa: E402


@pytest.fixture
def ends():
    """(our ChannelLink, the app's end of its control channel)."""
    ours, theirs = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    control = plusone_link.Channel(ours.detach(), "tablet", "sign", "control", plusone_link.CONTROL, 1)
    link = ChannelLink(None, "tablet", "sign", control, "Tab")
    yield link, theirs
    control.close()
    theirs.close()


def read_all(sock, out):
    """What the app gets, message by message, until the end."""
    while True:
        data = sock.recv(1 << 20)
        if not data:
            return
        out.append(data)


def test_a_payload_follows_its_header_in_pieces(ends):
    link, theirs = ends
    got = []
    reader = threading.Thread(target=read_all, args=(theirs, got))
    reader.start()
    pdf = bytes(range(256)) * 1000  # 256 000 bytes: four pieces
    link.send({"type": "sign", "job": "j"}, pdf)
    link.control.sock.shutdown(socket.SHUT_WR)
    reader.join(5)
    header = json.loads(got[0])
    assert header == {"type": "sign", "job": "j", "size": len(pdf)}
    assert all(len(piece) <= CHUNK for piece in got[1:])
    assert b"".join(got[1:]) == pdf


def test_a_long_message_is_put_together(ends):
    link, theirs = ends
    marks = {"type": "signed", "job": "j", "marks": {"pages": [{"page": 0, "marks": ["x" * 1000] * 300}]}}
    data = json.dumps(marks).encode()
    assert len(data) > CHUNK

    def app():
        theirs.send(json.dumps({"type": "long", "size": len(data)}).encode())
        for start in range(0, len(data), CHUNK):
            theirs.send(data[start:start + CHUNK])
    threading.Thread(target=app).start()
    assert link.readable(5)
    assert link.receive() == marks


def test_a_long_message_we_send_goes_in_pieces(ends):
    link, theirs = ends
    got = []
    reader = threading.Thread(target=read_all, args=(theirs, got))
    reader.start()
    message = {"type": "x", "text": "y" * (3 * CHUNK)}
    link.send(message)
    link.control.sock.shutdown(socket.SHUT_WR)
    reader.join(5)
    assert json.loads(got[0]) == {"type": "long", "size": len(json.dumps(message).encode())}
    assert json.loads(b"".join(got[1:])) == message


def test_a_long_message_over_its_limit_or_its_size_is_refused(ends):
    link, theirs = ends
    theirs.send(json.dumps({"type": "long", "size": 100}).encode())
    with pytest.raises(ValueError):
        link.receive(limit=99)
    theirs.send(json.dumps({"type": "long", "size": 4}).encode())
    theirs.send(b'{"a": 1}')
    with pytest.raises(ValueError):
        link.receive()


def test_nothing_readable_in_time(ends):
    link, _theirs = ends
    assert not link.readable(0.05)


def test_what_the_channels_send_a_tablet_is_measured(monkeypatch):
    now = [100.0]
    monkeypatch.setattr(channels.time, "monotonic", lambda: now[0])
    sent = []
    channel = type("FakeChannel", (), {"send": lambda self, body: sent.append(body), "feature": "speakers"})()
    pcm = channels.ChannelPackets(None, "tab-measured", channel)
    for _ in range(200):  # a second of 5 ms packets of PCM
        pcm.send(bytes(965))
        now[0] += 0.005
    assert len(sent) == 200
    kbps = channels.sending_kbps("tab-measured")
    assert 1600 <= kbps <= 1700, kbps  # with what the network adds to each
    assert channels.sending_kbps("another") == 0
    now[0] += 2  # silence: nothing sent
    assert channels.sending_kbps("tab-measured") == 0


def test_a_payload_the_app_does_not_read_fails_in_time(ends):
    link, _theirs = ends  # the app reads nothing more
    link.settimeout(0.3)
    outcome = []

    def send():
        try:
            link.send({"type": "sign", "job": "j"}, bytes(16 << 20))
        except OSError as e:
            outcome.append(e)
    sending = threading.Thread(target=send, daemon=True)
    sending.start()
    sending.join(5)
    assert not sending.is_alive() and isinstance(outcome[0], TimeoutError)



APP = "abcd" * 4


def link_answering(monkeypatch, devices):
    """plus-one-link as channels asks it: its devices() answer, or what it raises."""
    class FakeLink:
        def devices(self):
            if isinstance(devices, BaseException):
                raise devices
            return devices

        def __enter__(self):
            return self

        def __exit__(self, *exc):
            pass
    monkeypatch.setattr(plusone_link, "Link", FakeLink)


@pytest.mark.parametrize("features", [None, ["update"], "update", 7])
def test_features_not_an_object_are_not_known(monkeypatch, features):
    link_answering(monkeypatch, [{"id": APP + "0" * 48, "connected": True, "features": features}])
    assert channels.app_version(APP) is None
    assert channels.connected(APP)


def test_the_app_s_version_from_its_features(monkeypatch):
    link_answering(monkeypatch, [{"id": APP + "0" * 48, "connected": True, "features": {"update": 1000}}])
    assert channels.app_version(APP) == 1000


@pytest.mark.parametrize("error", [BrokenPipeError(), ValueError("garbled"), plusone_link.LinkError("gone")])
def test_plus_one_link_failing_is_no_session(monkeypatch, error):
    link_answering(monkeypatch, error)
    assert channels.app_version(APP) is None and not channels.connected(APP)
