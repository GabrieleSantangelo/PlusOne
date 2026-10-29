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
import threading
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
        self.packets, self.let_goes, self.pen = [], 0, None

    def busy(self):
        return True

    def check(self):
        pass

    def packet(self, body):
        self.packets.append(body)
        return True

    def let_go(self):
        self.let_goes += 1


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
    streamer = types.SimpleNamespace(input=FakeInput(), gated=False, active=True, _let_go=False)
    listen(streamer, [touch])
    assert streamer.input.packets == [touch] and streamer.input.let_goes == 0
    streamer.gated = True
    listen(streamer, [touch, None, touch])
    assert streamer.input.packets == [touch]  # nothing more: dropped, not queued
    assert streamer.input.let_goes == 1  # let go once, at once
    streamer.gated = False
    listen(streamer, [touch])
    assert streamer.input.packets == [touch, touch]



class FakePipeline:
    """The states asked of it, in order; one thread's set_state held, if told."""

    def __init__(self):
        self.states, self.hold = [], None

    def set_state(self, state):
        if self.hold and threading.current_thread().name == self.hold[0]:
            _name, reached, go = self.hold
            reached.set()
            go.wait(5)
        self.states.append(state)


class WatchedLock:
    """A lock that says which threads asked for it."""

    def __init__(self):
        self.lock, self.asked = threading.Lock(), []

    def __enter__(self):
        self.asked.append(threading.current_thread().name)
        self.lock.acquire()

    def __exit__(self, *exc):
        self.lock.release()


def test_the_pen_stays_while_paused_or_locked_and_goes_with_another_user_in_front():
    # Made once (D-Bus, the helper's polkit, the tablet's settings written),
    # not again at the next stroke after each pause or lock; another
    # user's session in front, closed (the helper takes it away too), and
    # made again once the seat is ours.
    made, closed = [], []

    def make_pen():
        made.append(1)
        return types.SimpleNamespace(report=lambda report: None, close=lambda: closed.append(1))
    touch = appdisplay.Input(types.SimpleNamespace(), (1000, 1000), make_pen)
    streamer = types.SimpleNamespace(input=touch, gated=False, active=True, _let_go=False, shown=True, _frame=1,
                                     _state_lock=threading.Lock(), _capture=lambda: None,
                                     force_keyframe=lambda: None, on_visible=lambda shown: None)
    near = bytes([appdisplay.INPUT, 0, 1, appdisplay.pen.IN_RANGE]) + bytes(8)

    def pen_there():
        deadline = time.monotonic() + 5
        while not touch.pen and time.monotonic() < deadline:
            time.sleep(0.005)
        return touch.pen
    touch.prepare()
    assert pen_there() and made == [1]
    appdisplay.AppStreamer.visible(streamer, False)  # the app paused
    appdisplay.AppStreamer.visible(streamer, True)
    listen(streamer, [near])
    appdisplay.AppStreamer.gate(streamer, True)  # locked
    listen(streamer, [None])
    appdisplay.AppStreamer.gate(streamer, False)
    listen(streamer, [near])
    assert made == [1] and closed == [] and touch.pen
    appdisplay.AppStreamer.gate(streamer, True, active=False)  # another user's session in front
    listen(streamer, [None])
    assert closed == [1] and touch.pen is None
    appdisplay.AppStreamer.gate(streamer, False, active=True)
    assert pen_there() and made == [1, 1]


def streamer_of(pipeline, shown=True):
    streamer = appdisplay.AppStreamer.__new__(appdisplay.AppStreamer)
    streamer.pipeline, streamer.shown, streamer.input = pipeline, shown, None
    streamer.gated, streamer.active, streamer._let_go = False, True, False
    streamer._state_lock = WatchedLock()
    streamer.on_visible = streamer.force_keyframe = lambda *args: None
    return streamer


def test_the_desktop_shown_again_as_another_user_comes_in_front_is_not_captured():
    # The app shows the desktop again (listening thread) as the seat goes
    # to another user's session (main loop): visible() read the seat still
    # active, gate() paused, then visible()'s PLAYING came after it.
    pipeline = FakePipeline()
    streamer = streamer_of(pipeline, shown=False)
    reached, go, gated = threading.Event(), threading.Event(), threading.Event()
    pipeline.hold = ("listening", reached, go)
    shown = threading.Thread(target=streamer.visible, args=(True,), name="listening")
    shown.start()
    assert reached.wait(5)  # PLAYING decided, the seat active
    seat = threading.Thread(target=lambda: (streamer.gate(True, active=False), gated.set()), name="main")
    seat.start()
    deadline = time.monotonic() + 5
    while not (gated.is_set() or "main" in streamer._state_lock.asked):  # done, or waiting its turn
        assert time.monotonic() < deadline, "never"
        time.sleep(0.005)
    go.set()
    shown.join(5)
    seat.join(5)
    assert streamer.shown and not streamer.active
    assert pipeline.states[-1] == appdisplay.Gst.State.PAUSED


def test_a_stream_started_with_another_user_in_front_captures_nothing():
    pipeline = FakePipeline()
    streamer = streamer_of(pipeline)
    streamer._feedback, streamer.channel = types.SimpleNamespace(start=lambda: None), types.SimpleNamespace()
    streamer.gate(True, active=False)  # AppDisplay._stream: the seat's say, then start()
    streamer.start()
    assert pipeline.states[-1] == appdisplay.Gst.State.PAUSED
    streamer.gate(False, active=True)
    assert pipeline.states[-1] == appdisplay.Gst.State.PLAYING
    streamer.visible(False)
    streamer.gate(True, active=False)
    streamer.gate(False, active=True)  # the seat back, the app still elsewhere
    assert pipeline.states[-1] == appdisplay.Gst.State.PAUSED

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
