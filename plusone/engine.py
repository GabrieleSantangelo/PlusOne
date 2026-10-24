"""Connection lifecycle shared by the command line and the GUI.

Runs on the GLib main loop of whoever owns it (GLib.MainLoop for the CLI,
the GTK application for the GUI). Blocking work (adb, the RTSP session)
happens on helper threads and reports back through GLib.idle_add.
"""
import socket
import threading
from dataclasses import dataclass

from gi.repository import Gio, GLib

from . import log, media, once, pen, tablet
from .display import VirtualMonitor, virtual_monitor_edid
from .p2p import P2PLink
from .rtsp import MODES, WfdSession
from .uibc import UibcServer

RTSP_PORT = 7236
UIBC_PORT = 50000
# Searching for the tablet keeps the Wi-Fi radio busy, so it is time-boxed:
CONNECT_WINDOW = 120    # after the user asks to connect
RECONNECT_WINDOW = 90   # after the tablet closes Second Screen, to catch a reopen
# Only the user can help then: the tablet is stuck in a half-made group.
NOT_ANSWERING = "The tablet is not answering: close and reopen Second Screen"


@dataclass
class Settings:
    auto_mode: bool = True  # take the resolution from the tablet's EDID
    width: int = 1920       # used when auto_mode is off
    height: int = 1200
    fps: int = 60
    bitrate: int = 20000  # kbit/s
    encoder: str = "auto"
    use_adb: bool = False
    touch: bool = True
    test_pattern: bool = False
    peer: str | None = None
    reconnect: bool = True  # look for the tablet again for a while after it disconnects
    # The app (not the engine): keep running without a window and start at
    # login, so the Quick Settings toggle connects at once.
    background: bool = True
    language: str = "auto"
    # "Sign on Tablet" in Files and Open With.
    sign_on_tablet: bool = True


class State:
    IDLE = "idle"
    WAITING = "waiting"        # looking for the tablet
    CONNECTING = "connecting"  # Wi-Fi Direct / RTSP in progress
    STREAMING = "streaming"
    ERROR = "error"


# Programs known to fight over the Wi-Fi Direct device, by D-Bus name: unlike
# the process list, the session bus is visible from inside a Flatpak sandbox.
CONFLICTING_APPS = {"org.gnome.NetworkDisplays": "GNOME Network Displays"}


def conflicting_apps():
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    running = []
    for name, label in CONFLICTING_APPS.items():
        owned = bus.call_sync("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
                              "NameHasOwner", GLib.Variant("(s)", (name,)), None,
                              Gio.DBusCallFlags.NONE, -1, None).unpack()[0]
        if owned:
            running.append(label)
    return running


class Controller:
    def __init__(self, settings, on_state=None):
        self.settings = settings
        self.on_state = on_state or (lambda state, template, args: None)
        self.state = State.IDLE
        self.detail = ("", {})
        self.link = None
        self.session = None
        self.streamer = None
        self.monitor = None
        self.uibc = None
        self.srv = None
        self.encoder = None
        self.mode = None  # (width, height, fps) of the current session
        self._running = False

    @property
    def running(self):
        return self._running

    # -- state ---------------------------------------------------------------
    def _set_state(self, state, template="", **args):
        """template is an English message (see i18n) formatted with args."""
        self.state, self.detail = state, (template, args)
        log(f"[{state}] {template.format(**args)}")
        self.on_state(state, template, args)

    def _from_thread(self, fn, *args):
        GLib.idle_add(once(fn, *args))

    # -- public --------------------------------------------------------------
    def start(self):
        if self._running:
            return
        s = self.settings
        if not s.auto_mode and (s.width, s.height, s.fps) not in MODES:
            self._set_state(State.ERROR, "Mode {mode} is not supported", mode=f"{s.width}x{s.height}@{s.fps}")
            return
        others = conflicting_apps()
        if others:
            self._set_state(State.ERROR, "Close {apps}: it also uses Wi-Fi Direct", apps=", ".join(others))
            return
        try:
            self.encoder = media.pick_encoder(s.encoder)
            self.srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            self.srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            self.srv.bind(("0.0.0.0", RTSP_PORT))
            self.srv.listen(1)
        except (OSError, RuntimeError) as e:
            self._set_state(State.ERROR, "{error}", error=str(e))
            return
        self._running = True
        if not s.test_pattern:
            self.monitor = VirtualMonitor(on_ready=self._start_stream)
        threading.Thread(target=self._accept_loop, daemon=True).start()
        if s.use_adb:
            self._set_state(State.WAITING, "Opening Second Screen on the tablet…")
            threading.Thread(target=self._adb_open, daemon=True).start()
        else:
            self._wait_for_tablet(CONNECT_WINDOW)

    def stop(self, reason=""):
        """reason: English template shown with the idle state (see i18n)."""
        if not self._running:
            return
        self._running = False
        self._end_session()
        if self.srv:
            try:
                self.srv.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            self.srv.close()
            self.srv = None
        self._set_state(State.IDLE, reason)

    # -- internals -----------------------------------------------------------
    def _adb_open(self):
        serial = tablet.device()
        if serial:
            tablet.prepare(serial, restart=False)
        else:
            log("adb: no tablet attached, open Second Screen by hand")
        self._from_thread(self._wait_for_tablet, CONNECT_WINDOW)

    def _wait_for_tablet(self, window):
        if not self._running or self.link is not None or self.session is not None:
            return
        self._set_state(State.WAITING, "Tap “Second screen” on the tablet")
        self.link = P2PLink(self.settings.peer, on_found=self._on_found, on_hint=self._on_link_hint,
                            give_up_after=window, on_give_up=self._on_give_up)
        self.link.start()

    def _on_give_up(self):
        self.stop("The tablet did not answer: open Second Screen on it and connect again")

    def _on_found(self, name):
        self._set_state(State.CONNECTING, "Connecting to {name}…", name=name)

    def _on_link_hint(self):
        self._set_state(State.CONNECTING, NOT_ANSWERING)

    def _accept_loop(self):
        while self._running:
            try:
                conn, addr = self.srv.accept()
            except OSError:
                return
            if self.link:
                self.link.connected = True
            log("sink connected from", addr[0])
            s = self.settings
            if s.touch and self.monitor:
                try:
                    self.uibc = UibcServer(self.monitor, UIBC_PORT, pen_factory=self._make_pen)
                except OSError as e:
                    log("touch disabled:", e)
            self.session = WfdSession(conn, self._on_play, self._on_idr,
                                      on_close=lambda: self._from_thread(self._on_session_closed),
                                      mode=None if s.auto_mode else (s.width, s.height, s.fps),
                                      uibc_port=UIBC_PORT if self.uibc else None)
            self.session.run()

    def _make_pen(self, descriptor):
        # Called on the UIBC thread once the tablet describes its pen.
        if pen.uhid_available():
            device = pen.UhidPen(descriptor)
            edid = virtual_monitor_edid()
            if edid:
                pen.map_to_monitor(edid)
            return device
        return pen.PointerPen(self.monitor, self.mode[:2])

    def _on_play(self, session):
        # The session settled the mode in M3/M4 (from the EDID in auto mode).
        self.mode = session.mode

        def go():
            if self.monitor:
                self.monitor.start()
            else:
                self._start_stream("test")
        self._from_thread(go)

    def _start_stream(self, source):
        width, height, fps = self.mode
        self.streamer = media.Streamer(source, self.session.sink_ip, width, height, fps,
                                       self.settings.bitrate, self.encoder)
        self.streamer.start()
        name = self.link.peer.get_name() if self.link and self.link.peer else "tablet"
        self._set_state(State.STREAMING, "{name} · {width}×{height} @ {fps} Hz",
                        name=name, width=width, height=height, fps=fps)

    def _on_idr(self):
        if self.streamer:
            self.streamer.force_keyframe()

    def _end_session(self):
        if self.session:
            self.session.stop()
            self.session = None
        if self.streamer:
            self.streamer.stop()
            self.streamer = None
        if self.uibc:
            self.uibc.close()
            self.uibc = None
        if self.monitor:
            self.monitor.stop()
        if self.link:
            self.link.stop()
            self.link = None

    def _on_session_closed(self):
        log("tablet disconnected")
        self._end_session()
        if self._running and self.settings.reconnect:
            # Reopening Second Screen soon reconnects. Give the tablet a moment
            # to tear its group down before searching again.
            self._set_state(State.WAITING, "Tap “Second screen” on the tablet")
            GLib.timeout_add_seconds(3, once(self._wait_for_tablet, RECONNECT_WINDOW))
        elif self._running:
            self.stop()
