"""plusone_link against plus-one-link and a test tablet (link/core's
examples/tablet.rs), both real processes on loopback. Needs them built
(tools/link-docker.sh build --release --workspace --examples --bins):
PLUSONE_LINK_BIN=link/target/release; skipped without it."""
import json
import os
import socket
import ssl
import subprocess
import threading
import time

import pytest

from plusone_link import CONTROL, REALTIME, Link, LinkError

BIN = os.environ.get("PLUSONE_LINK_BIN")
pytestmark = pytest.mark.skipif(not BIN, reason="PLUSONE_LINK_BIN: the link's binaries")


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
    pc = tmp_path / "pc"
    _pc_cert, _pc_key, pc_fp = identity(pc)
    tablet_cert, tablet_key, tablet_fp = identity(tmp_path / "tablet")
    (pc / "paired.json").write_text(json.dumps({tablet_fp: {"name": "Test tablet"}}))
    tablet = subprocess.Popen([os.path.join(BIN, "examples", "tablet"), "--cert", tablet_cert, "--key", tablet_key,
                               "--trust", pc_fp], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    port = int(tablet.stdout.readline().split()[1])
    path = str(tmp_path / "run" / "link")
    daemon = subprocess.Popen([os.path.join(BIN, "plus-one-link"), "--socket", path, "--config", str(pc),
                               "--name", "test-pc", "--state", str(tmp_path / "state.json")],
                              stderr=subprocess.DEVNULL)
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
    assert set(device["features"]) == {"echo", "sink", "callback"}


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
    channel = link.open(tablet_id, "echo", "x", REALTIME)
    tablet.kill()
    # Its path lost: the session waits for another (15 s), then is over.
    assert gone.wait(25), events
    kinds = [e["event"] for e in events]
    assert kinds[0] == "up" and kinds[-1] == "gone" and "detached" in kinds and events[0]["device"] == tablet_id
    assert channel.recv(timeout=1) == b""
    watcher.close()


def test_not_there():
    with pytest.raises(LinkError, match="not there"):
        Link("/nonexistent/plus-one/link")
