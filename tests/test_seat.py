"""The desktop locked, or another user's session in front (SEC-22): the
tablet's input reaches nothing and lets go at once, its camera and
microphone pause, and all comes back when it opens again.

seat.py runs against a fake logind on a private D-Bus daemon (no services,
no settings): Plus One as a D-Bus service has no session of its own, so the
fake answers GetSessionByPID with an error and the user's Display instead.
"""
import os
import shutil
import subprocess
import sys
import textwrap
import time
import types

import pytest

from gi.repository import Gio, GLib

from plusone import appdisplay, devices, seat

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

FAKE = textwrap.dedent("""
    from gi.repository import Gio, GLib
    SESSION = "/org/freedesktop/login1/session/_2"
    xml = '''<node>
      <interface name="org.freedesktop.login1.Manager">
        <method name="GetSessionByPID"><arg type="u" direction="in"/><arg type="o" direction="out"/></method>
        <method name="GetUser"><arg type="u" direction="in"/><arg type="o" direction="out"/></method>
      </interface>
      <interface name="org.freedesktop.login1.User"><property name="Display" type="(so)" access="read"/></interface>
      <interface name="org.freedesktop.login1.Session">
        <property name="Active" type="b" access="read"/><property name="LockedHint" type="b" access="read"/>
      </interface></node>'''
    node = Gio.DBusNodeInfo.new_for_xml(xml)
    state = {"Active": True, "LockedHint": False}
    def manager(conn, sender, path, interface, method, args, invocation):
        if method == "GetSessionByPID":
            invocation.return_dbus_error("org.freedesktop.login1.NoSessionForPID", "a service")
        else:
            invocation.return_value(GLib.Variant("(o)", ("/org/freedesktop/login1/user/_1000",)))
    conn = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    conn.register_object("/org/freedesktop/login1", node.interfaces[0], manager, None, None)
    conn.register_object("/org/freedesktop/login1/user/_1000", node.interfaces[1], None,
                         lambda *a: GLib.Variant("(so)", ("2", SESSION)), None)
    conn.register_object(SESSION, node.interfaces[2], None, lambda c, s, p, i, name: GLib.Variant("b", state[name]),
                         None)
    def command(channel, condition):
        status, line, _length, _end = channel.read_line()
        if status != GLib.IOStatus.NORMAL or not line.strip():
            loop.quit()
            return False
        name, value = line.split()
        state[name] = value == "1"
        conn.emit_signal(None, SESSION, "org.freedesktop.DBus.Properties", "PropertiesChanged",
                         GLib.Variant("(sa{sv}as)", ("org.freedesktop.login1.Session",
                                                    {name: GLib.Variant("b", state[name])}, [])))
        return True
    def acquired(conn, name):
        print("ready", flush=True)
    Gio.bus_own_name_on_connection(conn, "org.freedesktop.login1", Gio.BusNameOwnerFlags.NONE, acquired, None)
    GLib.io_add_watch(GLib.IOChannel.unix_new(0), GLib.PRIORITY_DEFAULT, GLib.IOCondition.IN | GLib.IOCondition.HUP,
                      command)
    loop = GLib.MainLoop()
    loop.run()
""")


@pytest.fixture
def logind():
    if not shutil.which("dbus-daemon"):
        pytest.skip("dbus-daemon not installed")
    daemon = subprocess.Popen(["dbus-daemon", "--session", "--nofork", "--print-address"],
                              stdout=subprocess.PIPE, text=True)
    address = daemon.stdout.readline().strip()
    env = dict(os.environ, DBUS_SESSION_BUS_ADDRESS=address, PYTHONPATH=ROOT)
    fake = subprocess.Popen([sys.executable, "-c", FAKE], env=env, stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, text=True)
    assert fake.stdout.readline().strip() == "ready"
    bus = Gio.DBusConnection.new_for_address_sync(
        address, Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
        None, None)

    def say(name, value):
        fake.stdin.write(f"{name} {int(value)}\n")
        fake.stdin.flush()

    yield bus, say
    fake.stdin.close()
    fake.wait(timeout=5)
    daemon.terminate()
    daemon.wait()


def until(condition, seconds=5):
    deadline = time.monotonic() + seconds
    while not condition():
        assert time.monotonic() < deadline, "never"
        GLib.MainContext.default().iteration(False)
        time.sleep(0.005)


def test_the_session_locked_inactive_and_open_again_is_told(logind):
    bus, say = logind
    told = []
    watched = seat.Seat(lambda open, active: told.append((open, active)), bus=bus)
    assert watched.path == "/org/freedesktop/login1/session/_2"  # the user's Display: a service has none
    assert told == [(True, True)]
    say("LockedHint", True)
    until(lambda: told[-1] == (False, True))
    say("LockedHint", False)
    until(lambda: told[-1] == (True, True))
    say("Active", False)
    until(lambda: told[-1] == (False, False))


def test_no_session_known_is_no_input(monkeypatch):
    told = []

    def fail(*args):
        raise GLib.Error("no logind")

    monkeypatch.setattr(seat.Seat, "_session", fail)
    seat.Seat(lambda open, active: told.append((open, active)), bus=object())
    assert told == [(False, False)]


class FakeInput:
    def __init__(self):
        self.packets, self.closed = [], 0

    def busy(self):
        return True

    def check(self):
        pass

    def packet(self, body):
        self.packets.append(body)
        return True

    def close(self):
        self.closed += 1


def listen(streamer, bodies):
    """AppStreamer._listen over these bodies from the app, then stopped."""
    left = list(bodies)

    def receive(timeout):
        if not left:
            streamer._running = False
            return None
        return left.pop(0)

    streamer.channel = types.SimpleNamespace(receive=receive)
    streamer._running = True
    appdisplay.AppStreamer._listen(streamer)


def test_locked_the_tablet_s_input_is_dropped_and_what_touches_let_go():
    touch = bytes([appdisplay.INPUT, 0, 0])
    streamer = types.SimpleNamespace(input=FakeInput(), gated=False, _let_go=False)
    listen(streamer, [touch])
    assert streamer.input.packets == [touch] and streamer.input.closed == 0
    streamer.gated = True
    listen(streamer, [touch, None, touch])
    assert streamer.input.packets == [touch]  # nothing more: dropped, not queued
    assert streamer.input.closed == 1  # let go once, at once
    streamer.gated = False
    listen(streamer, [touch])
    assert streamer.input.packets == [touch, touch]


class FakeDevice(devices.Device):
    def __init__(self):
        self.notes = []
        app = types.SimpleNamespace(devices_changed=lambda: None,
                                    notify=lambda key, title, body, **args: self.notes.append((title, args)))
        super().__init__(app, "camera")
        self.state, self.calls = "ready", []

    def begin(self):
        self.calls.append("begin")

    def end(self):
        self.calls.append("end")


def test_locked_the_camera_pauses_and_goes_on_after():
    camera = FakeDevice()
    camera.users = ["Meet"]
    camera._settle(True)
    assert camera.state == "in use" and camera.calls == ["begin"]
    assert camera.notes == [("A program uses the tablet's camera", {"programs": "Meet"})]  # said each time (SEC-25)
    camera.gate(True)
    assert camera.state == "locked" and camera.calls == ["begin", "end"]
    camera.gate(False)
    assert camera.state == "in use" and camera.calls == ["begin", "end", "begin"]


def test_a_program_that_takes_the_camera_while_locked_waits():
    camera = FakeDevice()
    camera.gate(True)
    camera._settle(True)
    assert camera.state == "locked" and camera.calls == []
    camera.gate(False)
    assert camera.state == "in use" and camera.calls == ["begin"]
