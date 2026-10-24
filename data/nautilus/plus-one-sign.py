"""Nautilus: "Sign on Tablet" for PDF files, while there is a tablet to sign
on. Plus One does the work and knows whether there is one (on the network or
connected); this only asks it over D-Bus (starting it if needed)."""
import json
import os

from gi.repository import Gio, GLib, GObject, Nautilus

BUS_NAME = "io.github.gabrielesantangelo.PlusOne"
OBJECT_PATH = "/io/github/gabrielesantangelo/PlusOne"
INTERFACE = "io.github.gabrielesantangelo.PlusOne1"
SETTINGS = os.path.join(GLib.get_user_config_dir(), "plus-one", "settings.json")


def _label():
    for var in ("LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"):
        value = os.environ.get(var)
        if value:
            return "Firma sul tablet" if value.startswith("it") else "Sign on Tablet"
    return "Sign on Tablet"


def _switched_on():
    """Plus One's "Sign on Tablet" switch, read from its settings."""
    try:
        with open(SETTINGS) as f:
            return json.load(f).get("sign_on_tablet", True) is not False
    except (OSError, ValueError, AttributeError):
        return True


class PlusOneSign(GObject.GObject, Nautilus.MenuProvider):
    def __init__(self):
        super().__init__()
        self._proxy = None
        # Follows Plus One's properties without starting it.
        Gio.DBusProxy.new_for_bus(Gio.BusType.SESSION, Gio.DBusProxyFlags.DO_NOT_AUTO_START, None,
                                  BUS_NAME, OBJECT_PATH, INTERFACE, None, self._got_proxy)

    def _got_proxy(self, _source, result):
        try:
            self._proxy = Gio.DBusProxy.new_for_bus_finish(result)
        except GLib.Error:
            pass

    def _offered(self):
        if self._proxy is not None and self._proxy.get_name_owner():
            available = self._proxy.get_cached_property("SignAvailable")
            if available is not None:
                return available.unpack()
        # Plus One is not running (or predates the property): it cannot tell
        # whether a tablet is around, so only the switch counts.
        return _switched_on()

    def get_file_items(self, files):
        paths = [f.get_location().get_path() for f in files
                 if f.get_mime_type() == "application/pdf" and f.get_location().get_path()]
        if not paths or len(paths) != len(files) or not self._offered():
            return []
        item = Nautilus.MenuItem(name="PlusOne::Sign", label=_label())
        item.connect("activate", self._sign, paths)
        return [item]

    def _sign(self, _item, paths):
        try:
            bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        except GLib.Error:
            return
        for path in paths:
            bus.call(BUS_NAME, OBJECT_PATH, INTERFACE, "SignDocument",
                     GLib.Variant("(s)", (path,)), None, Gio.DBusCallFlags.NONE, -1, None, None, None)
