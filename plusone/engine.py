"""The second screen's lifecycle, shared by the command line and the GUI:
the desktop in Plus One's app on the tablet in use (appdisplay.py).

Runs on the GLib main loop of whoever owns it (GLib.MainLoop for the CLI,
the GTK application for the GUI); the session reports back on it.
"""
import importlib.util
import threading
from dataclasses import dataclass

from . import link, log, media
from .link import Tablets


@dataclass
class Settings:
    bitrate: int = 20000  # kbit/s at 1920x1200, the most: less while the network does not take it
    encoder: str = "auto"
    touch: bool = True
    test_pattern: bool = False  # the command line's --test: a test pattern, no virtual monitor
    # The app (not the engine): keep running without a window and start at
    # login, so the Quick Settings toggle connects at once.
    background: bool = True
    # "Sign on Tablet" in Files and Open With.
    sign_on_tablet: bool = True
    # The tablet's app brought to this PC's version (updates.py).
    update_tablet: bool = True
    # The paired tablet in use (its app's certificate fingerprint, link/tablets.py):
    # the only one that gets the desktop and the PDFs.
    tablet: str | None = None
    # Its camera and microphone as the PC's own (devices.py).
    camera: bool = False
    camera_facing: str = "front"   # or "back"
    camera_quality: str = "720p"   # or "1080p"
    microphone: bool = False
    # Its speakers as one of the PC's sound outputs.
    speakers: bool = False
    # Its noise taken out: "off", "light" (the tablet's libwebrtc) or "neural"
    # (DeepFilterNet's plugin, on this PC: devices.py).
    microphone_noise: str = "neural"
    # The tablet by cable when it is plugged in (docs/LINK.md): plus-one-link
    # asks it to be Plus One's accessory (its file transfer off meanwhile).
    cable: bool = True
    # The saved signatures shared with the tablet's app, both ways (the app
    # chooses too, for each PC). Off: each side keeps its own.
    share_signatures: bool = False
    # The Quick Settings toggle offered once, after the first pairing (gui.py).
    toggle_offered: bool = False
    # The desktop's size: "auto" (about 1920x1200) or "WxH" at the tablet's
    # proportions; and the tablet's screen, as its app last said.
    app_size: str = "auto"
    app_screen: str | None = None


class State:
    IDLE = "idle"
    CONNECTING = "connecting"  # the app asked to show the desktop
    STREAMING = "streaming"
    ERROR = "error"


def parse_size(text):
    """(width, height) of "WxH", or None ("auto", nothing, nonsense)."""
    try:
        width, height = (int(v) for v in str(text).split("x"))
    except ValueError:
        return None
    return (width, height) if 320 <= width <= 7680 and 240 <= height <= 4320 else None


def why_not(error, name):
    """Why the app did not show the desktop: (English template, args)."""
    if isinstance(error, link.Refused):
        if str(error) == "background":
            return "On {name}, let Plus One display over other apps, or open it", {"name": name}
        return "{name} did not let the desktop show: {reason}", {"name": name, "reason": str(error)}
    if isinstance(error, link.PairingRefused):
        return "{name} no longer knows this PC: pair it again", {"name": name}
    if isinstance(error, link.Incompatible):
        return "Plus One on {name} and on this PC are different versions: update them", {"name": name}
    if isinstance(error, OSError):
        return "{name} did not answer: is Plus One open on it?", {"name": name}
    return "{error}", {"error": str(error)}


class Controller:
    def __init__(self, settings, on_state=None, tablets=None):
        self.settings = settings
        self.on_state = on_state or (lambda state, template, args: None)
        self.tablets = tablets or Tablets(settings)
        self.state = State.IDLE
        self.detail = ("", {})
        self.encoder = None
        self.app_display = None  # appdisplay.AppDisplay, while the app shows the desktop
        self.gated = (False, True)  # seat.py's (closed, active), for the desktop's streamer
        self.mode = None  # (width, height, fps) of the current session
        self._running = False
        # Each start() a new one: an older session's callbacks change nothing.
        self._generation = 0
        self._lock = threading.Lock()

    @property
    def running(self):
        return self._running

    # -- state ---------------------------------------------------------------
    def _set_state(self, state, template="", **args):
        """template is an English message (see i18n) formatted with args."""
        self.state, self.detail = state, (template, args)
        log(f"[{state}] {template.format(**args)}")
        self.on_state(state, template, args)

    # -- public --------------------------------------------------------------
    def start(self):
        if self._running:
            return
        tablet = self.tablets.active()
        if tablet is None:
            self._set_state(State.ERROR, "Pair a tablet first")
            return
        targets = self.tablets.targets(tablet)
        if not targets:
            self._set_state(State.ERROR, "{name} is not on the network: open Plus One on it", name=tablet.name)
            return
        if importlib.util.find_spec("cryptography") is None:  # from the sources, without it
            self._set_state(State.ERROR, "Needs python3-cryptography")
            return
        self._start_app(tablet, targets)

    def stop(self, reason="", **args):
        """reason: English template shown with the idle state (see i18n), formatted with args."""
        if not self._running:
            return
        with self._lock:
            self._running = False
            self._generation += 1
        self._end_app()
        self._set_state(State.IDLE, reason, **args)

    # -- the desktop in Plus One's app (appdisplay.py) -----------------------------
    def _start_app(self, tablet, targets):
        from .appdisplay import AppDisplay
        s = self.settings
        try:
            self.encoder = media.pick_encoder(s.encoder)
        except (RuntimeError, KeyError) as e:
            self._set_state(State.ERROR, "{error}", error=str(e))
            return
        threading.Thread(target=media.warm_up, args=(self.encoder,), daemon=True).start()
        with self._lock:
            self._running = True
            self._generation += 1
        size = parse_size(s.app_size) or (None, None)  # None: chosen once the tablet says its screen
        mode = (*size, 60)
        self._set_state(State.CONNECTING, "Opening the desktop on {name}…", name=tablet.name)
        generation = self._generation

        def started(mode, screen=None):
            if generation == self._generation:
                self.mode = mode
                if screen:
                    self.tablets.screen_seen(screen)  # the window offers its sizes
                self._set_state(State.STREAMING, "{name} · {width}×{height} @ {fps} Hz",
                                name=tablet.name, width=mode[0], height=mode[1], fps=mode[2])

        def paused(paused):
            if generation != self._generation or self.state != State.STREAMING:
                return
            if paused:
                self._set_state(State.STREAMING, "{name}: paused while Plus One is not on its screen",
                                name=tablet.name)
            else:
                started(self.mode)

        def ended(error, was_started):
            if generation != self._generation:
                return
            self.app_display = None
            self.mode = None
            if was_started:
                log("app display: ended:", repr(error) if error else "")
                if error is None or isinstance(error, EOFError):
                    self.stop("The tablet closed the desktop")  # Back or Stop there
                elif isinstance(error, OSError):
                    self.stop("The tablet is no longer reachable")  # off, or off the network
                else:
                    self.stop("The desktop stopped: {error}", error=str(error))
            else:
                log("app display: not shown:", repr(error))
                with self._lock:
                    self._running = False
                    self._generation += 1
                template, args = why_not(error, tablet.name)
                self._set_state(State.ERROR, template, **args)

        self.app_display = AppDisplay(targets, tablet.app_id, mode, s.bitrate, self.encoder, touch=s.touch,
                                      test=s.test_pattern, on_started=started, on_ended=ended, on_paused=paused)
        self.app_display.gate(*self.gated)
        self.app_display.start()

    def gate(self, closed, active=True):
        """On the main loop: the desktop may not be touched now, or may
        again (seat.py)."""
        self.gated = (closed, active)
        if self.app_display is not None:
            self.app_display.gate(closed, active)

    def _end_app(self):
        if self.app_display is not None:
            self.app_display.stop()
            self.app_display = None
            self.mode = None
