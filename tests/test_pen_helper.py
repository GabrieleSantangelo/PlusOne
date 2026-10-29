"""The pen helper (data/pen-helper, root on the system bus): a pen only for
the session in front of the PC, and taken away as soon as another user's
comes in front, or it ends. A program of the first user's could go on
sending reports, and the other user's compositor would take them as clicks.

The helper is loaded from its script; the system bus is a fake (polkit, the
bus itself and logind answer as told, their signals sent by hand), and
/dev/uhid a file: no bus, logind, polkit or uhid device of the PC's."""
import importlib.machinery
import importlib.util
import os
import socket
import struct

import pytest

from gi.repository import GLib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPT = os.path.join(ROOT, "data", "pen-helper", "plus-one-pen-helper")
LOGIN1 = "org.freedesktop.login1"
OURS, THEIRS = "/org/freedesktop/login1/session/_2", "/org/freedesktop/login1/session/_3"
REPORT = bytes.fromhex("14114a39277dfc013319")


@pytest.fixture
def helper(tmp_path, monkeypatch):
    loader = importlib.machinery.SourceFileLoader("plus_one_pen_helper", SCRIPT)
    module = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
    loader.exec_module(module)
    monkeypatch.setattr(module, "UHID", str(tmp_path / "uhid"))
    monkeypatch.setattr(module, "arm_idle", lambda: None)
    (tmp_path / "uhid").touch()
    yield module
    for pen in list(module.pens.values()):
        pen.close()


class FakeSystemBus:
    """What the helper asks of the system bus: polkit says yes, the caller's
    process is in session (None: in none, a D-Bus service of its user's,
    whose graphical session is OURS), the seat is in front, its signals
    are sent by hand."""

    def __init__(self, session=OURS):
        self.session, self.front = session, OURS
        self.seats = {OURS: "seat0", THEIRS: "seat0"}
        self.subscriptions, self.ids = {}, 0

    def call_sync(self, name, path, interface, method, args, reply, flags, timeout, cancellable):
        if method == "CheckAuthorization":
            return GLib.Variant("((bba{ss}))", ((True, False, {}),))
        if method == "GetConnectionUnixProcessID":
            return GLib.Variant("(u)", (4242,))
        if method == "GetConnectionUnixUser":
            return GLib.Variant("(u)", (1000,))
        if method == "GetSessionByPID":
            if self.session is None:
                raise GLib.Error("org.freedesktop.login1.NoSessionForPID: a service")
            return GLib.Variant("(o)", (self.session,))
        if method == "GetUser":
            return GLib.Variant("(o)", ("/org/freedesktop/login1/user/_1000",))
        if method == "Get" and path == "/org/freedesktop/login1/user/_1000":
            return GLib.Variant("(v)", (GLib.Variant("(so)", ("2", OURS)),))
        if method == "GetAll" and path in self.seats:
            seat = self.seats[path]
            return GLib.Variant("(a{sv})", ({
                "Active": GLib.Variant("b", path == self.front or not seat),  # logind: no seat, always "active"
                "Seat": GLib.Variant("(so)", (seat, f"/org/freedesktop/login1/seat/{seat}" if seat else "/"))},))
        raise GLib.Error(f"{interface}.{method} on {path}: not faked")

    def signal_subscribe(self, sender, interface, member, path, arg0, flags, callback, *data):
        self.ids += 1
        self.subscriptions[self.ids] = (sender, interface, member, path, arg0, callback, data)
        return self.ids

    def signal_unsubscribe(self, id):
        del self.subscriptions[id]

    def emit(self, path, interface, member, params):
        for sender, i, m, p, arg0, callback, data in list(self.subscriptions.values()):
            if (sender in (None, LOGIN1) and i in (None, interface) and m in (None, member) and p in (None, path)
                    and arg0 in (None, params.get_child_value(0).unpack())):
                callback(self, ":1.1", path, interface, member, params, *data)

    def changed(self, path, **values):
        self.emit(path, "org.freedesktop.DBus.Properties", "PropertiesChanged",
                  GLib.Variant("(sa{sv}as)", ("org.freedesktop.login1.Session",
                                             {k: GLib.Variant("b", v) for k, v in values.items()}, [])))

    def switch(self, to):
        """The seat goes to another session, as logind tells it: each one's
        Active, then the seat's ActiveSession."""
        before, self.front = self.front, to
        self.changed(before, Active=False)
        self.changed(to, Active=True)
        self.emit("/org/freedesktop/login1/seat/seat0", "org.freedesktop.DBus.Properties", "PropertiesChanged",
                  GLib.Variant("(sa{sv}as)", ("org.freedesktop.login1.Seat", {
                      "ActiveSession": GLib.Variant("(so)", (to[-2:], to))}, [])))


class Caller:
    """The D-Bus invocation of Open: the error, or the socket of the pen."""

    def __init__(self):
        self.error = self.sock = None

    def return_dbus_error(self, name, message):
        self.error = name

    def return_value_with_unix_fd_list(self, value, fds):
        self.sock = socket.socket(fileno=fds.get(value.unpack()[0]))
        self.sock.settimeout(1)


def open_pen(helper, bus):
    caller = Caller()
    helper.method_call(bus, ":1.42", helper.PATH, helper.NAME, "Open", None, caller, bus)
    return caller


def sent(helper, caller):
    """A report from the caller, as the helper's loop takes it: whether it reached uhid."""
    pen, = helper.pens.values()
    before = os.path.getsize(helper.UHID)
    caller.sock.send(REPORT)
    pen._report(pen.ours.fileno(), GLib.IOCondition.IN)
    with open(helper.UHID, "rb") as uhid:
        return uhid.read()[before:] == struct.pack("<IH", helper.UHID_INPUT2, len(REPORT)) + REPORT


def test_a_pen_goes_when_another_user_s_session_comes_in_front(helper):
    bus = FakeSystemBus()
    caller = open_pen(helper, bus)
    assert caller.error is None and sent(helper, caller)
    bus.changed(OURS, LockedHint=True)  # locked, still in front: the app's own gate (seat.py)
    bus.changed(THEIRS, Active=False)   # another session's news
    assert len(helper.pens) == 1 and sent(helper, caller)
    bus.switch(THEIRS)
    assert helper.pens == {}  # at once...
    assert caller.sock.recv(64) == b""  # ...its socket closed: nothing reaches theirs
    with open(helper.UHID, "rb") as uhid:
        assert uhid.read().endswith(struct.pack("<I", helper.UHID_DESTROY))
    assert bus.subscriptions == {}  # and nothing left watched
    bus.switch(OURS)
    assert helper.pens == {}  # back: a new Open, as the app does at its next report


def test_no_pen_for_a_session_not_in_front(helper):
    bus = FakeSystemBus()
    bus.front = THEIRS  # polkit said yes (a rule of the admin's, say), but another user's is in front
    assert open_pen(helper, bus).error == "org.freedesktop.DBus.Error.AccessDenied"
    bus.front, bus.seats[OURS] = OURS, ""  # no seat (SSH): "active" for logind, in front of nothing
    assert open_pen(helper, bus).error == "org.freedesktop.DBus.Error.AccessDenied"
    assert helper.pens == {} and bus.subscriptions == {}


def test_plus_one_as_a_service_follows_its_user_s_graphical_session(helper):
    # A D-Bus service of its user's (Plus One) is in no session: its user's
    # graphical one, as polkit judged it.
    bus = FakeSystemBus(session=None)
    caller = open_pen(helper, bus)
    assert caller.error is None and sent(helper, caller)
    bus.switch(THEIRS)
    assert helper.pens == {} and caller.sock.recv(64) == b""


def test_a_session_that_ends_takes_its_pen(helper):
    bus = FakeSystemBus()
    caller = open_pen(helper, bus)
    open_pen(helper, bus)
    assert caller.error is None and len(helper.pens) == 2
    bus.emit("/org/freedesktop/login1", "org.freedesktop.login1.Manager", "SessionRemoved",
             GLib.Variant("(so)", ("3", THEIRS)))
    assert len(helper.pens) == 2
    bus.emit("/org/freedesktop/login1", "org.freedesktop.login1.Manager", "SessionRemoved",
             GLib.Variant("(so)", ("2", OURS)))
    assert helper.pens == {} and bus.subscriptions == {}


def test_a_pen_closed_by_its_caller_leaves_nothing_watched(helper):
    bus = FakeSystemBus()
    caller = open_pen(helper, bus)
    pen, = helper.pens.values()
    caller.sock.close()
    pen._report(pen.ours.fileno(), GLib.IOCondition.HUP)
    assert helper.pens == {} and bus.subscriptions == {}


def test_on_the_session_bus_as_before(helper):
    # --session: the user's own rights, nothing asked of polkit or logind.
    caller = open_pen(helper, None)
    assert caller.error is None and sent(helper, caller)
