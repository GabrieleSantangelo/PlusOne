"""plus-one: the window and background service (no arguments), or the desktop
on the tablet in use from the command line (--cli)."""
import argparse
import signal
import sys

import gi

gi.require_version("Gst", "1.0")
from gi.repository import GLib, Gst  # noqa: E402

try:  # GLib >= 2.80 moved it; the old name warns in newer PyGObject
    gi.require_version("GLibUnix", "2.0")
    from gi.repository import GLibUnix  # noqa: E402
    signal_add = GLibUnix.signal_add
except (ValueError, ImportError, AttributeError):
    signal_add = GLib.unix_signal_add

from . import media  # noqa: E402
from .engine import Controller, Settings, State, parse_size  # noqa: E402


def size_argument(text):
    """--size: "auto", or WxH as the settings take it."""
    if text == "auto":
        return None
    size = parse_size(text)
    if size is None:
        raise argparse.ArgumentTypeError(f"not a size: {text!r}")
    return size


def main():
    if sys.argv[1:] in ([], ["--gui"], ["--gapplication-service"]) or sys.argv[1:2] == ["--sign"]:
        # The application: window, background service, D-Bus interface,
        # and `--sign [FILE...]` to sign PDFs on the tablet.
        from .app import main as app_main
        return app_main(sys.argv[:1] if sys.argv[1:] == ["--gui"] else sys.argv)

    ap = argparse.ArgumentParser(prog="plus-one", description=__doc__)
    ap.add_argument("--cli", action="store_true", help="the desktop on the tablet in use, without the window")
    ap.add_argument("--size", type=size_argument, default=None,
                    help="WIDTHxHEIGHT at the tablet's proportions (default: about 1920x1200)")
    ap.add_argument("--bitrate", type=int, default=20000, help="kbit/s at 1920x1200, the most")
    ap.add_argument("--encoder", default="auto", choices=["auto", *media.ENCODERS])
    ap.add_argument("--test", action="store_true", help="a test pattern, no virtual monitor")
    ap.add_argument("--no-input", action="store_true", help="no touch nor pen")
    args = ap.parse_args()

    Gst.init(None)
    loop = GLib.MainLoop()
    settings = Settings(bitrate=args.bitrate, encoder=args.encoder, touch=not args.no_input,
                        test_pattern=args.test, app_size="%dx%d" % args.size if args.size else "auto")
    # The tablet in use is the window's: the only one that gets the desktop.
    from .app import load_settings
    settings.tablet = load_settings().tablet

    def on_state(state, template, args):
        if state == State.IDLE or (state == State.ERROR and not controller.running):
            loop.quit()

    # Where the tablet's app is: announced (mDNS), else where it was last.
    from .link import Discovery, Nearby, Tablets
    tablets = Tablets(settings)

    def go():
        if not controller.running and controller.state == State.IDLE and not started:
            started.append(True)
            controller.start()
        return False

    def found():
        tablet = tablets.active()
        if tablet and tablets.discovery.addresses(tablet.app_id):
            GLib.idle_add(go)

    started = []
    tablets.discovery = Discovery([Nearby(on_change=found)])
    tablets.discovery.start()
    controller = Controller(settings, on_state, tablets)

    def shutdown(*_):
        controller.stop()
        GLib.timeout_add_seconds(2, loop.quit)
        return False

    signal_add(GLib.PRIORITY_DEFAULT, signal.SIGINT, shutdown)
    signal_add(GLib.PRIORITY_DEFAULT, signal.SIGTERM, shutdown)
    GLib.timeout_add_seconds(3, go)  # not announced by then: where it was last
    loop.run()


if __name__ == "__main__":
    main()
