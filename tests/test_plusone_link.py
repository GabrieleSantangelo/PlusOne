"""plusone_link against plus-one-link and a test tablet (link/core's
examples/tablet.rs), both real processes on loopback. Needs them built
(tools/link-docker.sh build --release --workspace --examples --bins):
PLUSONE_LINK_BIN=link/target/release; skipped without it. The last tests
play plus-one-link's end of the socket themselves."""
import array
import json
import os
import socket
import ssl
import subprocess
import threading
import time

import pytest

import plusone_link
from plusone_link import CONTROL, HEAD, REALTIME, VIDEO, Link, LinkError

BIN = os.environ.get("PLUSONE_LINK_BIN")


def identity(directory):
    """As plusone/link/trust.py makes the PC's: (cert, key, fingerprint)."""
    os.makedirs(directory, mode=0o700, exist_ok=True)
    cert, key = os.path.join(directory, "cert.pem"), os.path.join(directory, "key.pem")
    subprocess.run(["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
                    "-nodes", "-keyout", key, "-out", cert, "-days", "2", "-subj", "/CN=test"],
                   check=True, capture_output=True, timeout=30)
    with open(cert) as f:
        der = ssl.PEM_cert_to_DER_cert(f.read())
    import hashlib
    return cert, key, hashlib.sha256(der).hexdigest()


@pytest.fixture
def world(tmp_path):
    """plus-one-link, a tablet paired with it, a Link: (link, tablet's address, tablet's id, the tablet's process)."""
    yield from _world(tmp_path)


@pytest.fixture
def quic_world(tmp_path):
    """As world, the session moving to QUIC once made (PLUSONE_LINK_QUIC=1)."""
    yield from _world(tmp_path, quic=True)


def _world(tmp_path, quic=False):
    if not BIN:
        pytest.skip("PLUSONE_LINK_BIN: the link's binaries")
    pc = tmp_path / "pc"
    _pc_cert, _pc_key, pc_fp = identity(pc)
    tablet_cert, tablet_key, tablet_fp = identity(tmp_path / "tablet")
    (pc / "paired.json").write_text(json.dumps({tablet_fp: {"name": "Test tablet"}}))
    tablet = subprocess.Popen([os.path.join(BIN, "examples", "tablet"), "--cert", tablet_cert, "--key", tablet_key,
                               "--trust", pc_fp] + (["--quic"] if quic else []),
                              stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    port = int(tablet.stdout.readline().split()[1])
    path = str(tmp_path / "run" / "link")
    environ = dict(os.environ, PLUSONE_LINK_QUIC="1" if quic else "0")
    daemon = subprocess.Popen([os.path.join(BIN, "plus-one-link"), "--socket", path, "--config", str(pc),
                               "--name", "test-pc", "--state", str(tmp_path / "state.json")],
                              stderr=subprocess.DEVNULL, env=environ)
    for _ in range(100):
        if os.path.exists(path):
            break
        time.sleep(0.02)
    link = Link(path)
    yield link, f"127.0.0.1:{port}", tablet_fp, tablet
    link.close()
    for process in (daemon, tablet):
        process.kill()
        process.wait()


def test_devices_and_connect(world):
    link, address, tablet_id, _ = world
    (device,) = link.devices()
    assert device["id"] == tablet_id and device["name"] == "Test tablet" and not device["connected"]
    assert link.connect(address) == tablet_id
    (device,) = link.devices()
    assert device["connected"] and device["name"] == "Test tablet"
    assert set(device["features"]) == {"echo", "sink", "callback", "frames"}
    queued, dropped = link.backlog(tablet_id)  # the queue and the drops in one request
    assert queued >= 0 and dropped == device["dropped"] == 0


def test_control_and_realtime_channels(world):
    link, address, tablet_id, _ = world
    link.connect(address)
    with link.open(tablet_id, "echo", "messages", CONTROL) as channel:
        for message in (b"one", b"two", bytes(150 << 10)):
            channel.send(message)
            assert channel.recv(timeout=5) == message
    with link.open(tablet_id, "echo", "video", REALTIME) as channel:
        got = 0
        for i in range(200):
            channel.send(i.to_bytes(4, "big") + bytes(1196))
            if channel.recv(timeout=0.5):
                got += 1
        assert got >= 195


def echoed_in_groups(link, tablet_id):
    """A VIDEO channel to the tablet's echo: frames of three pieces each, a
    group each (a keyframe every tenth), and a message of no group; what
    came back, and what was sent (as written, the heads not with them)."""
    sent, got = [], []
    with link.open(tablet_id, "echo", "frames", VIDEO) as channel:
        assert channel.kind == VIDEO
        for frame in range(1, 61):
            pieces = [frame.to_bytes(4, "big") + bytes([index]) + os.urandom(plusone_link.MAX_DATAGRAM - HEAD.size - 5)
                      for index in range(3)]
            for index, piece in enumerate(pieces):
                channel.send_in_group(frame, frame % 10 == 1, index == 2, piece)
            sent += pieces
            for _ in pieces:  # back before the next frame: none superseded by a key one
                body = channel.recv(timeout=0.5)
                if body:
                    got.append(body)
        channel.send(b"of no group")
        sent.append(b"of no group")
        while body := channel.recv(timeout=0.5):
            got.append(body)
    return sent, got


def test_a_video_channel_s_messages_come_as_written(world):
    # TCP: the heads taken off, each message as realtime's.
    link, address, tablet_id, _ = world
    link.connect(address)
    sent, got = echoed_in_groups(link, tablet_id)
    assert set(got) <= set(sent) and len(got) >= len(sent) * 0.95 and b"of no group" in got


def test_on_quic_a_video_channel_s_groups_come_as_written(quic_world):
    # QUIC: each frame on a stream of its own (both sides read groups), the
    # tablet's end a realtime one, what it reads the pieces as written.
    link, address, tablet_id, _ = quic_world
    link.connect(address)
    for _ in range(100):
        if link.devices()[0].get("transport") == "quic":
            break
        time.sleep(0.05)
    assert link.devices()[0]["transport"] == "quic"
    sent, got = echoed_in_groups(link, tablet_id)
    assert set(got) <= set(sent) and len(got) >= len(sent) * 0.95 and b"of no group" in got


def test_refused_and_unknown(world):
    link, address, tablet_id, _ = world
    with pytest.raises(LinkError):  # never reached: nowhere to look (not announced)
        link.open(tablet_id, "echo", "x", CONTROL)
    with pytest.raises(LinkError, match="not a paired tablet"):
        link.open("0" * 64, "echo", "x", CONTROL)
    link.connect(address)
    with pytest.raises(LinkError, match="not served"):
        link.open(tablet_id, "camera", "video", REALTIME)
    with pytest.raises(LinkError):
        link.connect("127.0.0.1:1", timeout=1)


def test_open_reconnects_where_the_tablet_was(world):
    link, address, tablet_id, _ = world
    link.connect(address)
    link.disconnect(tablet_id)
    time.sleep(0.2)
    assert not link.devices()[0]["connected"]
    with link.open(tablet_id, "echo", "x", CONTROL) as channel:
        channel.send(b"again")
        assert channel.recv(timeout=5) == b"again"


def test_serve_takes_the_tablet_s_channels(world):
    link, address, tablet_id, _ = world
    link.connect(address)
    got = []
    heard = threading.Event()

    def on_channel(channel):
        got.append((channel.device, channel.feature, channel.kind, channel.recv(timeout=5)))
        heard.set()
    server = link.serve("hello", on_channel)
    with link.open(tablet_id, "callback", "ask", CONTROL) as ask:
        ask.send(b"hello")
        assert ask.recv(timeout=5) == b"opened"
        assert heard.wait(5)
        assert got == [(tablet_id, "hello", CONTROL, b"hello")]
        server.close()
        time.sleep(0.3)
        ask.send(b"hello")
        assert b"not served" in ask.recv(timeout=5)


def test_watch_sees_the_tablet_come_and_go(world):
    link, address, tablet_id, tablet = world
    events = []
    gone = threading.Event()

    def on_event(event):
        events.append(event)
        if event["event"] == "gone":
            gone.set()
    watcher = link.watch(on_event)
    link.connect(address)
    tablet.kill()
    # Its path lost: the session waits for another (15 s), then is over.
    assert gone.wait(25), events
    kinds = [e["event"] for e in events]
    assert kinds[0] == "up" and kinds[-1] == "gone" and "detached" in kinds and events[0]["device"] == tablet_id
    # What it is on, and what carries it (TCP: plus-one-link moves to QUIC only with PLUSONE_LINK_QUIC=1).
    assert (events[0]["path"], events[0]["transport"]) == ("lan", "tcp")
    watcher.close()


def test_a_session_in_use_waits_longer(world):
    link, address, tablet_id, tablet = world
    gone = threading.Event()
    watcher = link.watch(lambda event: event["event"] == "gone" and gone.set())
    link.connect(address)
    channel = link.open(tablet_id, "echo", "x", REALTIME)
    tablet.kill()
    # A channel open: the session waits 60 s for another path, not 15;
    # once it is over, the channel ends.
    assert not gone.wait(25)
    assert gone.wait(45)
    assert channel.recv(timeout=1) == b""
    watcher.close()


def test_not_there():
    with pytest.raises(LinkError, match="not there"):
        Link("/nonexistent/plus-one/link")


def test_a_stream_made_here_is_a_path(world):
    # As a Bluetooth socket BlueZ gives Plus One: here a TCP connection to
    # the test tablet, handed over; the session goes on it, of that kind.
    link, address, tablet_id, _tablet = world
    host, port = address.rsplit(":", 1)
    sock = socket.create_connection((host, int(port)), timeout=5)
    assert link.stream("bt", sock) == tablet_id
    sock.close()  # the daemon has its own
    device = next(d for d in link.devices() if d["id"] == tablet_id)
    assert (device["path"], device["transport"]) == ("bt", "stream")
    channel = link.open(tablet_id, "echo", "x", CONTROL)
    channel.send(b"by Bluetooth")
    assert channel.recv(timeout=5) == b"by Bluetooth"


def open_fds():
    return len(os.listdir("/proc/self/fd"))


def with_fd(sock, message, fd):
    sock.sendmsg([message], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", [fd]))])


@pytest.fixture
def daemon(monkeypatch):
    """plus-one-link's end of the next connection made: what it answers is
    sent on it before the request comes."""
    ours, theirs = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    monkeypatch.setattr(plusone_link, "_connect", lambda path: ours)
    yield theirs
    ours.close()
    theirs.close()


@pytest.mark.parametrize("reply", [b"{garbled", b"[1]", b'"ok"'])
def test_a_reply_not_a_message_leaves_no_descriptor_open(daemon, reply):
    r, w = os.pipe()
    with_fd(daemon, reply, r)
    link = Link("fake")
    before = open_fds()
    with pytest.raises(ValueError if reply == b"{garbled" else LinkError):
        link.devices()
    assert open_fds() == before
    for fd in (r, w):
        os.close(fd)


@pytest.mark.parametrize("reply", [b"{garbled", b"[]", b'{"ok": false, "error": "no"}'])
def test_events_refused_or_garbled_close_their_connection(daemon, reply):
    daemon.send(reply)
    with pytest.raises((ValueError, LinkError)):
        plusone_link._Events("fake", {"op": "watch"}, lambda event, fd: None, "test-watch")
    assert plusone_link._connect("fake").fileno() == -1


@pytest.mark.parametrize("reply, backlog", [({"queued": 5, "dropped": 7}, (5, 7)),
                                            ({"queued": 5}, (5, None))])  # a plus-one-link from before
def test_the_backlog_is_the_queue_and_the_drops_in_one_request(daemon, reply, backlog):
    daemon.send(json.dumps(dict(reply, ok=True)).encode())
    assert Link("fake").backlog("tab") == backlog
    assert json.loads(daemon.recv(4096)) == {"op": "queued", "device": "tab"}


def test_a_video_channel_is_asked_for_as_video(daemon):
    ours, theirs = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    with_fd(daemon, json.dumps({"ok": True, "id": 4}).encode(), ours.fileno())
    ours.close()
    channel = Link("fake").open("tab", "display", "media", VIDEO)
    assert json.loads(daemon.recv(4096))["class"] == "video" and channel.kind == VIDEO
    channel.send(b"ping")  # of no group: the head says so
    assert theirs.recv(2048) == b"\0\0\0\0\0ping"
    channel.close()
    theirs.close()


def channel_on_a_pair(kind):
    """A channel of kind, and plus-one-link's end of it."""
    ours, theirs = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    return plusone_link.Channel(ours.detach(), "tab", "display", "media", kind, 2), theirs


def test_a_video_channel_s_messages_carry_the_head_the_link_takes_off():
    channel, link_end = channel_on_a_pair(VIDEO)
    channel.send_in_group(7, True, False, b"key 1")
    channel.send_in_group(7, True, True, b"key 2")
    channel.send_in_group(8, False, False, b"p 1")
    channel.send_in_group(8, False, True, b"p 2")
    channel.send_in_group(2 ** 32 + 9, False, True, b"on")  # a u32: round again
    channel.send(b"pong")  # of no group
    assert [link_end.recv(2048) for _ in range(6)] == [
        b"\x05\0\0\0\x07key 1", b"\x07\0\0\0\x07key 2", b"\x01\0\0\0\x08p 1", b"\x03\0\0\0\x08p 2",
        b"\x03\0\0\0\x09on", b"\0\0\0\0\0pong"]
    assert HEAD.size == 5
    channel.close()
    link_end.close()


def test_a_group_s_message_on_another_class_goes_as_it_is():
    # REALTIME where plus-one-link refused VIDEO: the caller's code the same.
    channel, link_end = channel_on_a_pair(REALTIME)
    channel.send_in_group(7, True, True, b"key")
    channel.send(b"pong")
    assert [link_end.recv(2048) for _ in range(2)] == [b"key", b"pong"]
    channel.close()
    link_end.close()


def test_a_descriptor_with_the_answer_to_a_stream_is_closed(daemon):
    r, w = os.pipe()
    with_fd(daemon, json.dumps({"ok": True, "device": "tab"}).encode(), r)
    link = Link("fake")
    given, other = socket.socketpair()
    before = open_fds()
    assert link.stream("bt", given) == "tab"
    assert open_fds() == before
    for fd in (r, w):
        os.close(fd)
    given.close()
    other.close()


def in_time(call, seconds):
    """What call raised (None: nothing), on a thread of its own: it must end within seconds."""
    outcome = []

    def run():
        try:
            call()
        except Exception as e:  # noqa: BLE001
            outcome.append(e)
        else:
            outcome.append(None)
    threading.Thread(target=run, daemon=True).start()
    deadline = time.monotonic() + seconds
    while not outcome and time.monotonic() < deadline:
        time.sleep(0.01)
    assert outcome, f"still waiting after {seconds} s"
    return outcome[0]


def test_a_request_plus_one_link_does_not_answer_fails_in_time(daemon, monkeypatch):
    monkeypatch.setattr(plusone_link, "ANSWER_WAIT", 0.2)
    link = Link("fake")
    error = in_time(link.devices, 3)
    assert isinstance(error, LinkError) and "in time" in str(error)
    # Its answer, coming late, is not taken for the next request's: that connection is over.
    assert plusone_link._connect("fake").fileno() == -1
    with pytest.raises(LinkError):
        link.devices()


def test_events_plus_one_link_does_not_answer_fail_in_time(daemon, monkeypatch):
    monkeypatch.setattr(plusone_link, "ANSWER_WAIT", 0.2)
    error = in_time(lambda: plusone_link._Events("fake", {"op": "watch"}, lambda event, fd: None, "test-watch"), 3)
    assert isinstance(error, OSError) and plusone_link._connect("fake").fileno() == -1


def test_an_event_its_handler_fails_on_does_not_end_the_events(daemon, monkeypatch):
    monkeypatch.setattr(plusone_link, "ANSWER_WAIT", 0.1)
    daemon.send(b'{"ok": true}')
    got = []

    def on_event(event, fd):
        got.append(event["n"])
        if event["n"] == 1:
            raise KeyError("a bug")
    events = plusone_link._Events("fake", {"op": "watch"}, on_event, "test-watch")
    daemon.send(b'{"n": 1}')
    time.sleep(0.3)  # longer than its first answer may take: events come whenever they come
    daemon.send(b'{"n": 2}')
    for _ in range(100):
        if len(got) == 2:
            break
        time.sleep(0.02)
    assert got == [1, 2] and events.thread.is_alive()
    events.close()
