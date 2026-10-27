"""The link to the tablet's app: pairing code, framing, marks."""
import hashlib
import json
import secrets
import shutil
import socket
import ssl
import struct
import threading

import pytest

from plusone import link, sign_jobs, updates


def test_the_pairing_code_is_the_same_on_both_sides():
    a, b = b"pc certificate", b"tablet certificate"
    ours, theirs = b"n" * 16, b"m" * 16
    code = link.trust.pairing_code(a, b, ours, theirs)
    assert code == link.trust.pairing_code(b, a, ours, theirs)
    assert len(code) == 6 and code.isdigit()
    assert code != link.trust.pairing_code(a, b + b"!", ours, theirs)  # a swapped certificate shows
    assert code != link.trust.pairing_code(a, b, ours, b"x" * 16)     # and so does the app's number


def test_messages_with_and_without_payload():
    left, right = socket.socketpair()
    with left, right:
        link.Wire(left).send({"type": "sign", "job": "x"}, b"%PDF-1.7 ...")
        raw = right.recv(4096)
        assert raw.endswith(b"%PDF-1.7 ...") and b'"size": 12' in raw
        link.Wire(left).send({"type": "ping"})
        assert link.Wire(right).receive() == {"type": "ping"}


def test_what_the_app_may_not_send():
    for bad in ({"type": "signed", "size": 1 << 30}, ["not", "an", "object"]):
        left, right = socket.socketpair()
        with left, right:
            data = json.dumps(bad).encode()
            left.sendall(struct.pack(">I", len(data)) + data)
            with pytest.raises(ValueError):
                link.Wire(right).receive()


def test_a_closed_connection_is_an_eof():
    left, right = socket.socketpair()
    left.close()
    with right, pytest.raises(EOFError):
        link.Wire(right).receive()


def test_marks_from_the_app():
    marks = sign_jobs.to_marks({"pages": [
        {"page": 0, "marks": [{"type": "stroke", "width": 1.6, "color": [0.1, 0.2, 0.3],
                               "points": [1, 2, 0.4, 3, 4, 0.5, 5, 6, 0.6]}]},
        {"page": 2, "marks": []}]})
    assert list(marks) == [0]
    stroke = marks[0][0]
    assert stroke.points == [(1, 2, 0.4), (3, 4, 0.5), (5, 6, 0.6)] and stroke.color == (0.1, 0.2, 0.3)


needs_openssl = pytest.mark.skipif(not shutil.which("openssl"), reason="no openssl")


@needs_openssl
def test_identity_is_made_once(tmp_path):
    made = []
    threads = [threading.Thread(target=lambda: made.append(link.identity(str(tmp_path))))
               for _ in range(3)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert len({der for _cert, _key, der in made}) == 1  # one key and certificate, at once or not
    assert oct((tmp_path / "key.pem").stat().st_mode)[-3:] == "600"


def test_paired_tablets_are_remembered(tmp_path):
    paired = link.Paired(str(tmp_path))
    paired.add("ab" * 32, "Galaxy Tab")
    assert link.Paired(str(tmp_path)).all() == {"ab" * 32: "Galaxy Tab"}


def test_text_from_the_app():
    marks = sign_jobs.to_marks({"pages": [{"page": 1, "marks": [
        {"type": "text", "x": 100, "y": 700, "text": "24/10/2026", "size": 11, "color": [0, 0, 0]}]}]})
    text = marks[1][0]
    assert (text.x, text.y, text.text, text.size) == (100.0, 700.0, "24/10/2026", 11.0)


class FakeApp:
    """The tablet's app as far as the protocol goes (LinkServer.kt):
    each connection runs one of the scripts given, in order."""

    def __init__(self, tmp_path, scripts, version=link.PROTOCOL, paired=True, at=None):
        self.cert, self.key, self.der = link.identity(str(tmp_path / "app"))
        self.scripts, self.version, self.paired, self.at = list(scripts), version, paired, at
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(4)
        self.target = self.listener.getsockname()
        self.codes, self.errors = [], []
        threading.Thread(target=self._serve, daemon=True).start()

    def _serve(self):
        for script in self.scripts:
            raw, _ = self.listener.accept()
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(self.cert, self.key)
            context.load_verify_locations(link.identity()[0])  # the PC's self-signed certificate
            context.verify_mode = ssl.CERT_REQUIRED
            try:
                with context.wrap_socket(raw, server_side=True) as sock:
                    wire = link.Wire(sock)
                    hello, _ = receive(wire)
                    wire.send({"type": "hello", "name": "Tab", "version": self.version, "paired": self.paired,
                               "at": self.at or sock.getsockname()[0]})
                    if not (self.paired and hello["paired"]) and self.version == link.PROTOCOL:
                        if hello.get("pair"):  # only when the user pairs, from the window
                            self._pair(wire, sock.getpeercert(binary_form=True), script)
                    else:
                        script(wire)
            except Exception as e:  # noqa: BLE001 - shown by the test
                self.errors.append(e)

    def _pair(self, wire, pc, script):
        nonce = secrets.token_bytes(16)
        commit = getattr(script, "commit", None) or link.trust.commitment(nonce, self.der, pc)
        wire.send({"type": "pair-commit", "commit": commit})
        theirs, _ = receive(wire)
        wire.send({"type": "pair-nonce", "nonce": nonce.hex()})
        self.codes.append(link.trust.pairing_code(pc, self.der, bytes.fromhex(theirs["nonce"]), nonce))
        answer, _ = receive(wire)
        wire.send({"type": "pair-ok"})
        if answer["type"] == "pair-ok":
            self.paired = True  # as the app remembers the PC
            script(wire)


def receive(wire):
    """What the app reads: the PC's messages, payloads included."""
    (length,) = struct.unpack(">I", wire._exactly(4))
    header = json.loads(wire._exactly(length))
    return header, wire._exactly(header["size"]) if header.get("size") else None


@pytest.fixture
def pc(tmp_path, monkeypatch):
    if not shutil.which("openssl"):
        pytest.skip("no openssl")
    monkeypatch.setattr(link.trust, "CONFIG", str(tmp_path / "pc"))
    monkeypatch.setattr(sign_jobs, "RESUME_EVERY", 0.01)
    pdf = tmp_path / "doc.pdf"
    pdf.write_bytes(b"%PDF-1.7")
    return str(pdf), link.Paired(str(tmp_path / "pc"))


def signs(marks, seen=None):
    def script(wire):
        message, payload = receive(wire)
        if seen is not None:
            seen.append((message, payload))
        wire.send({"type": "opened", "job": message["job"]})
        wire.send({"type": "signed", "job": message["job"], "marks": marks, "sha256": sha256(payload)})
        assert receive(wire)[0] == {"type": "got", "job": message["job"]}
    return script


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def test_marks_for_another_pdf_are_not_taken(tmp_path, pc):
    path, paired = pc

    def another(wire):
        message, _payload = receive(wire)
        wire.send({"type": "signed", "job": message["job"], "marks": {"pages": []}, "sha256": sha256(b"other")})
        assert receive(wire)[0] == {"type": "got", "job": message["job"]}

    app = FakeApp(tmp_path, [another])
    paired.add(link.fingerprint(app.der), "Tab")
    with pytest.raises(sign_jobs.Mismatch):
        sign_jobs.sign(app.target, path, paired=paired)


def test_pairing_shows_the_same_code_on_both_sides(tmp_path, pc):
    path, paired = pc
    marks = {"pages": []}
    app = FakeApp(tmp_path, [lambda wire: None, signs(marks)], paired=False)
    asked = []
    fp, name = link.pair([app.target], lambda name, code: asked.append(code) or True, paired=paired)
    assert asked == app.codes and len(asked[0]) == 6 and name == "Tab"
    assert fp == link.fingerprint(app.der) and paired.all() == {fp: "Tab"}
    assert sign_jobs.sign(app.target, path, paired=paired) == marks


def test_a_number_the_app_did_not_commit_to_is_refused(tmp_path, pc):
    path, paired = pc
    script = signs({"pages": []})
    script.commit = hashlib.sha256(b"something else").hexdigest()
    app = FakeApp(tmp_path, [script], paired=False)
    with pytest.raises(link.PairingRefused):
        link.pair([app.target], lambda name, code: True, paired=paired)
    assert not paired.all()


def test_another_version_of_the_app(tmp_path, pc):
    path, paired = pc
    app = FakeApp(tmp_path, [lambda wire: None], version=link.PROTOCOL + 1)
    paired.add(link.fingerprint(app.der), "Tab")
    with pytest.raises(link.Incompatible):
        sign_jobs.sign(app.target, path, paired=paired)


def test_a_silent_link_is_found_dead(monkeypatch):
    monkeypatch.setattr(sign_jobs, "PING_EVERY", 0.05)
    monkeypatch.setattr(sign_jobs, "SILENT_FOR", 0.2)
    left, right = socket.socketpair()
    with left, right:
        with pytest.raises(TimeoutError):
            sign_jobs._answer(link.Wire(left), "job", "digest", None, "Tab", None)
        assert receive(link.Wire(right))[0] == {"type": "ping"}  # it asked first


def test_a_dropped_link_is_resumed(tmp_path, pc):
    # The link drops once the PDF is on the tablet; Plus One connects again
    # and gets the signature the user finished meanwhile.
    path, paired = pc
    marks = {"pages": [{"page": 0, "marks": []}]}
    ours, theirs = {"signatures": [], "removed": ["x"]}, {"signatures": [], "removed": ["y"]}
    seen, got = [], []

    def drops(wire):
        message, payload = receive(wire)
        seen.append(message)
        assert message["signatures"] == ours and payload == b"%PDF-1.7"
        assert message["sha256"] == sha256(payload)
        wire.send({"type": "opened", "job": message["job"]})

    def resumed(wire):
        message, _ = receive(wire)
        assert message == {"type": "resume", "job": seen[0]["job"]}
        wire.send({"type": "resumed", "job": message["job"]})
        wire.send({"type": "signed", "job": message["job"], "marks": marks, "signatures": theirs,
                   "sha256": seen[0]["sha256"]})
        assert receive(wire)[0] == {"type": "got", "job": message["job"]}

    app = FakeApp(tmp_path, [drops, resumed])
    paired.add(link.fingerprint(app.der), "Tab")
    result = sign_jobs.sign(app.target, path, paired=paired, locate=lambda: app.target,
                            signatures=ours, on_signatures=got.append)
    assert result == marks and got == [theirs] and not app.errors


def test_a_batch_arrives_in_order(tmp_path, pc):
    # Five PDFs selected in Files: each its own connection, but one after
    # the other in the order asked for, so the tablet's queue keeps that
    # order, whichever thread runs first.
    _path, paired = pc
    titles = []

    def queued(wire):
        message, payload = receive(wire)
        titles.append(message["title"])
        wire.send({"type": "signed", "job": message["job"], "marks": {"pages": []}, "sha256": sha256(payload)})
        assert receive(wire)[0]["type"] == "got"

    paths = []
    for i in range(3):
        (tmp_path / f"doc{i}.pdf").write_bytes(b"%PDF-1.7")
        paths.append(str(tmp_path / f"doc{i}.pdf"))
    app = FakeApp(tmp_path, [queued] * 3)
    paired.add(link.fingerprint(app.der), "Tab")
    turns = [sign_jobs.next_turn() for _ in paths]
    threads = [threading.Thread(target=sign_jobs.sign, args=(app.target, path),
                                kwargs={"paired": paired, "turn": turn})
               for path, turn in zip(paths, turns)]
    for thread in reversed(threads):
        thread.start()
    for thread in threads:
        thread.join(10)
    assert titles == ["doc0.pdf", "doc1.pdf", "doc2.pdf"] and not app.errors


def test_a_turn_goes_on_when_its_pdf_fails(tmp_path, pc):
    path, paired = pc
    first = sign_jobs.next_turn()
    with pytest.raises(OSError):
        sign_jobs.sign(("127.0.0.1", 1), str(tmp_path / "missing.pdf"), paired=paired, turn=first)
    app = FakeApp(tmp_path, [signs({"pages": []})])
    paired.add(link.fingerprint(app.der), "Tab")
    assert sign_jobs.sign(app.target, path, paired=paired) == {"pages": []}


def test_an_impostor_at_a_paired_tablets_address(tmp_path, pc):
    # Someone answers mDNS with the paired tablet's id: no pairing is
    # offered with it, and the tablet's real address is tried next.
    path, paired = pc
    real = FakeApp(tmp_path / "real", [signs({"pages": []})])
    paired.add(link.fingerprint(real.der), "Tab")
    impostor = FakeApp(tmp_path / "impostor", [lambda wire: None], paired=False)
    expect = link.fingerprint(real.der)[:16]
    result = sign_jobs.sign([impostor.target, real.target], path, paired=paired, expect=expect)
    assert result == {"pages": []}


def test_signing_never_pairs(tmp_path, pc):
    # Only the window pairs, when the user picks the tablet: a PDF to an
    # app that does not know this PC goes nowhere, and no code is shown.
    path, paired = pc
    app = FakeApp(tmp_path, [signs({"pages": []})], paired=False)
    with pytest.raises(link.PairingRefused):
        sign_jobs.sign(app.target, path, paired=paired)
    assert app.codes == [] and not paired.all()


def test_pairing_a_tablet_paired_already_asks_nothing(tmp_path, pc):
    _path, paired = pc
    app = FakeApp(tmp_path, [lambda wire: None])
    paired.add(link.fingerprint(app.der), "Tab")
    asked = []
    assert link.pair([app.target], lambda name, code: asked.append(code) or True,
                          paired=paired) == (link.fingerprint(app.der), "Tab")
    assert asked == []


def test_a_sessions_link_gives_up_on_a_tablet_gone_from_the_network(tmp_path, pc):
    # Not TCP's quarter of an hour of retries: the kernel drops the link
    # once the pings go unacknowledged for LINK_TIMEOUT.
    _path, paired = pc
    held = threading.Event()
    app = FakeApp(tmp_path, [lambda wire: held.wait(5)])
    fp = link.fingerprint(app.der)
    paired.add(fp, "Tab")
    tablet = link.connect([app.target], fp[:16], paired)
    try:
        assert tablet.sock.getsockopt(socket.IPPROTO_TCP, socket.TCP_USER_TIMEOUT) == link.LINK_TIMEOUT * 1000
    finally:
        held.set()
        tablet.close()


def test_the_app_is_handed_its_update_whatever_its_protocol(tmp_path, pc):
    # An older app speaks an older protocol: the update still goes, to a
    # tablet paired both ways, and only the update.
    _path, paired = pc
    got = []

    def takes(wire):
        message, payload = receive(wire)
        got.append((message, payload))
        wire.send({"type": "updating"})
    app = FakeApp(tmp_path, [takes, lambda wire: None], version=link.PROTOCOL - 1)
    fp = link.fingerprint(app.der)
    paired.add(fp, "Tab")
    apk = tmp_path / "app.apk"
    apk.write_bytes(b"an app")
    assert updates.update_app([app.target], fp[:16], str(apk), 800, paired) is None
    message, payload = got[0]
    assert message["type"] == "update" and message["version"] == 800 and payload == b"an app"
    assert message["sha256"] == hashlib.sha256(b"an app").hexdigest()
    with pytest.raises(link.Incompatible):  # anything else: not across versions
        sign_jobs.sign(app.target, str(apk), paired=paired)


def test_only_updating_is_an_update(tmp_path, pc):
    _path, paired = pc

    def answers(wire):
        receive(wire)
        wire.send({"type": "pong"})  # not what an app installing it says
    app = FakeApp(tmp_path, [answers])
    fp = link.fingerprint(app.der)
    paired.add(fp, "Tab")
    apk = tmp_path / "app.apk"
    apk.write_bytes(b"an app")
    assert updates.update_app([app.target], fp[:16], str(apk), 800, paired) == "pong"


def test_marks_that_make_no_sense_are_left_out():
    marks = sign_jobs.to_marks({"pages": [
        {"page": -1, "marks": [{"type": "text", "x": 1, "y": 1, "text": "x"}]},
        {"page": True, "marks": [{"type": "text", "x": 1, "y": 1, "text": "x"}]},
        {"page": 0, "marks": [
            {"type": "stroke", "points": [0, 0, 0.5, "NaN", 1, 0.5]},
            {"type": "text", "x": 1, "y": 2, "text": "ok\x00", "color": [0, 0]},
            {"type": "text", "x": 1, "y": 2, "text": "fine\x00", "size": 1e309},
            {"type": "text", "x": 1, "y": 2, "text": "kept\x00"},
        ]},
    ]})
    assert list(marks) == [0] and [m.text for m in marks[0]] == ["kept"]
