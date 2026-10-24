"""UIBC: generic touch events and the HIDC S Pen as sent by Second Screen."""
import socket
import struct

import pytest

from conftest import wait_for
from plusone.uibc import TOUCH_DOWN, TOUCH_MOVE, TOUCH_UP, UibcServer

# Descriptor and reports captured from a Galaxy Tab S10 FE+ S Pen.
PEN_DESCRIPTOR = bytes.fromhex(
    "050d0902a10185140920a10009420944093c0945093215002501750195058102950381030501"
    "093075109501a4550d65133500463a2027ff7f000081020931462c1827ff7f00008102b4050d"
    "093026001081027508093d15a6255a8102093e8102c0c0")
PEN_REPORT = bytes.fromhex("14114a39277dfc013319")


class Recorder:
    def __init__(self):
        self.calls = []

    def __getattr__(self, name):
        return lambda *args: self.calls.append((name, *args))


def packet(category, body, timestamp=False):
    head = struct.pack(">BBH", 0x10 if timestamp else 0, category, 4 + len(body) + (2 if timestamp else 0))
    return head + (b"\x12\x34" if timestamp else b"") + body


def touch(ie_id, *points):
    desc = bytes([len(points)]) + b"".join(struct.pack(">BHH", *p) for p in points)
    return struct.pack(">BH", ie_id, len(desc)) + desc


def hidc(usage, data, hid_type=6):
    return bytes([1, hid_type, usage]) + struct.pack(">H", len(data)) + data


@pytest.fixture
def server():
    injector, pens = Recorder(), []

    def factory(descriptor):
        pen = Recorder()
        pen.descriptor = descriptor
        pens.append(pen)
        return pen

    srv = UibcServer(injector, port=0, pen_factory=factory)
    sink = socket.create_connection(srv.sock.getsockname())
    yield srv, sink, injector, pens
    sink.close()
    srv.close()


def test_touch_down_move_up(server):
    srv, sink, injector, _ = server
    sink.sendall(packet(0, touch(TOUCH_DOWN, (0, 100, 200))))
    sink.sendall(packet(0, touch(TOUCH_MOVE, (0, 150, 250)), timestamp=True))
    sink.sendall(packet(0, touch(TOUCH_UP, (0, 150, 250))))
    assert wait_for(lambda: len(injector.calls) == 3)
    assert injector.calls == [("touch_down", 0, 100.0, 200.0),
                              ("touch_motion", 0, 150.0, 250.0),
                              ("touch_up", 0)]


def test_multitouch_and_release_on_disconnect(server):
    srv, sink, injector, _ = server
    sink.sendall(packet(0, touch(TOUCH_DOWN, (0, 10, 10), (1, 20, 20))))
    assert wait_for(lambda: len(injector.calls) == 2)
    sink.close()
    # Fingers still down when the sink goes away must not stay stuck.
    assert wait_for(lambda: {c[:2] for c in injector.calls[2:]} == {("touch_up", 0), ("touch_up", 1)})


def test_pen_descriptor_then_reports(server):
    srv, sink, _, pens = server
    sink.sendall(packet(1, hidc(1, PEN_DESCRIPTOR)))
    sink.sendall(packet(1, hidc(0, PEN_REPORT)))
    assert wait_for(lambda: pens and pens[0].calls)
    assert pens[0].descriptor == PEN_DESCRIPTOR
    assert pens[0].calls == [("report", PEN_REPORT)]


def test_pen_report_without_descriptor_is_dropped(server):
    srv, sink, injector, pens = server
    sink.sendall(packet(1, hidc(0, PEN_REPORT)))
    sink.sendall(packet(0, touch(TOUCH_DOWN, (0, 1, 1))))
    assert wait_for(lambda: injector.calls)
    assert pens == []


def test_close_frees_the_port():
    # A thread blocked in accept() used to keep the port listening after
    # close(), so Stop then Start in the same window lost touch input.
    srv = UibcServer(Recorder(), port=0)
    port = srv.sock.getsockname()[1]
    srv.close()
    again = UibcServer(Recorder(), port=port)
    again.close()


def test_close_ends_open_connections():
    # A sink connection from the previous session must not outlive it.
    srv = UibcServer(Recorder(), port=0)
    sink = socket.create_connection(srv.sock.getsockname())
    assert wait_for(lambda: srv.conns)
    srv.close()
    sink.settimeout(2)
    assert sink.recv(1) == b""  # EOF, not a hang
    sink.close()
    assert wait_for(lambda: not srv.conns)
