"""Settings and status window of the Plus One application (see app.py)."""
import shutil

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Adw, GLib, Gtk  # noqa: E402

from . import i18n, once  # noqa: E402
from .app import IN_FLATPAK, set_shell_extension, shell_extension_state  # noqa: E402
from .engine import State  # noqa: E402
from .i18n import _  # noqa: E402
from .rtsp import MODES  # noqa: E402

MODE_LIST = sorted(((w, h, f) for (w, h, f) in MODES if f == 60), key=lambda m: -m[0] * m[1])
ICONS = {
    State.IDLE: "video-display-symbolic",
    State.WAITING: "network-wireless-symbolic",
    State.CONNECTING: "network-transmit-receive-symbolic",
    State.STREAMING: "emblem-ok-symbolic",
    State.ERROR: "dialog-warning-symbolic",
}


def mode_label(mode):
    w, h, _f = mode
    ratio = "16:10" if w * 10 == h * 16 else "16:9" if w * 9 == h * 16 else ""
    return f"{w} × {h} ({ratio})" if ratio else f"{w} × {h}"


class Window(Adw.ApplicationWindow):
    def __init__(self, app):
        super().__init__(application=app, title="Plus One", default_width=480, default_height=760)
        self.app = app
        self.settings = app.settings
        self.controller = app.controller

        toolbar = Adw.ToolbarView()
        toolbar.add_top_bar(Adw.HeaderBar())
        self.set_content(toolbar)
        page = Adw.PreferencesPage()
        toolbar.set_content(page)

        # A hand-made status block: Adw.StatusPage scrolls on its own and gets
        # clipped inside a PreferencesPage.
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12,
                      margin_top=12, margin_bottom=12)
        self.status_icon = Gtk.Image(pixel_size=96, css_classes=["dim-label"])
        self.status_title = Gtk.Label(css_classes=["title-1"], wrap=True,
                                      justify=Gtk.Justification.CENTER)
        self.status_detail = Gtk.Label(css_classes=["dim-label"], wrap=True,
                                       justify=Gtk.Justification.CENTER)
        self.button = Gtk.Button(halign=Gtk.Align.CENTER, css_classes=["pill"], margin_top=12)
        self.button.connect("clicked", self._toggle)
        for widget in (self.status_icon, self.status_title, self.status_detail, self.button):
            box.append(widget)
        header = Adw.PreferencesGroup()
        header.add(box)
        page.add(header)

        self.display_group = Adw.PreferencesGroup()
        # Entry 0 is "Automatic" (from the tablet's EDID); the rest follow MODE_LIST.
        self.mode_row = Adw.ComboRow()
        current = (self.settings.width, self.settings.height, self.settings.fps)
        self._mode_selected = 0 if self.settings.auto_mode or current not in MODE_LIST \
            else 1 + MODE_LIST.index(current)
        self.bitrate_row = Adw.SpinRow.new_with_range(5, 60, 1)
        self.bitrate_row.set_value(self.settings.bitrate / 1000)
        self.display_group.add(self.mode_row)
        self.display_group.add(self.bitrate_row)
        page.add(self.display_group)

        self.options_group = Adw.PreferencesGroup()
        self.touch_row = Adw.SwitchRow(active=self.settings.touch)
        # Files and Open With are outside a Flatpak's reach.
        self.sign_row = Adw.SwitchRow(active=self.settings.sign_on_tablet, visible=not IN_FLATPAK)
        self.background_row = Adw.SwitchRow(active=self.settings.background)
        # Shown once the shell says the extension is installed (never in a Flatpak).
        self.extension_row = Adw.SwitchRow(visible=False)
        self.has_adb = shutil.which("adb") is not None
        self.adb_row = Adw.SwitchRow(active=self.settings.use_adb and self.has_adb,
                                     sensitive=self.has_adb)
        self.language_row = Adw.ComboRow(model=Gtk.StringList.new(
            [i18n.LANGUAGE_NAMES[code] for code in i18n.LANGUAGES]))
        language = self.settings.language if self.settings.language in i18n.LANGUAGES else "auto"
        self.language_row.set_selected(i18n.LANGUAGES.index(language))
        self._language_handler = self.language_row.connect("notify::selected", self._on_language)
        for row in (self.touch_row, self.sign_row, self.background_row, self.extension_row,
                    self.adb_row, self.language_row):
            self.options_group.add(row)
        page.add(self.options_group)
        # Settings are read when a connection starts: lock them while one is up.
        self.session_rows = [self.mode_row, self.bitrate_row, self.touch_row]
        if self.has_adb:
            self.session_rows.append(self.adb_row)

        log_group = Adw.PreferencesGroup()
        self.log_expander = Gtk.Expander()
        self.log_view = Gtk.TextView(editable=False, monospace=True, cursor_visible=False,
                                     wrap_mode=Gtk.WrapMode.WORD_CHAR)
        self.log_expander.set_child(Gtk.ScrolledWindow(min_content_height=180, child=self.log_view))
        log_group.add(self.log_expander)
        page.add(log_group)
        buf = self.log_view.get_buffer()
        buf.set_text("".join(line + "\n" for line in app.log_lines))

        self._translate()
        for row, signal in ((self.mode_row, "notify::selected"), (self.bitrate_row, "notify::value"),
                            (self.touch_row, "notify::active"), (self.adb_row, "notify::active")):
            row.connect(signal, self._apply_settings)
        self.background_row.connect("notify::active",
                                    lambda row, _p: app.set_background(row.get_active()))
        self.sign_row.connect("notify::active",
                              lambda row, _p: app.set_sign_on_tablet(row.get_active()))
        self._extension_handler = self.extension_row.connect(
            "notify::active", lambda row, _p: set_shell_extension(row.get_active()))
        shell_extension_state(self._show_extension_state)
        app.listeners.append(self._refresh)
        app.log_listeners.append(self._append_log)
        self.connect("close-request", self._on_close)

    # -- language --------------------------------------------------------------
    def _translate(self):
        """(Re)apply every user-visible string in the current language."""
        self.display_group.set_title(_("Display"))
        self.mode_row.set_title(_("Resolution"))
        self.bitrate_row.set_title(_("Quality"))
        self.bitrate_row.set_subtitle(_("Mbit/s: higher is sharper but uses more bandwidth"))
        self.options_group.set_title(_("Options"))
        self.touch_row.set_title(_("Touch and S Pen"))
        self.sign_row.set_title(_("Sign on Tablet"))
        self.background_row.set_title(_("Ready in the background"))
        self.background_row.set_subtitle(_("Starts at login and stays ready after you close this window"))
        self.extension_row.set_title(_("Quick Settings toggle"))
        self.extension_row.set_subtitle(_("Adds a “Tablet” button to GNOME's Quick Settings"))
        self.adb_row.set_title(_("Open Second Screen from here"))
        self.adb_row.set_subtitle(_("Uses adb: needs wireless debugging on the tablet")
                                  if self.has_adb else _("adb is not available"))
        self.language_row.set_title(_("Language"))
        self.log_expander.set_label(_("Log"))
        selected = self.mode_row.get_selected() if self.mode_row.get_model() else self._mode_selected
        self.mode_row.set_model(Gtk.StringList.new(
            [_("Automatic")] + [mode_label(m) for m in MODE_LIST]))
        self.mode_row.set_selected(selected)
        # Only "System" needs translating; rebuild the model without re-firing.
        with self.language_row.handler_block(self._language_handler):
            selected = self.language_row.get_selected()
            self.language_row.set_model(Gtk.StringList.new(
                [_(i18n.LANGUAGE_NAMES[code]) for code in i18n.LANGUAGES]))
            self.language_row.set_selected(selected)
        self._refresh()

    def _on_language(self, row, _pspec):
        self.settings.language = i18n.LANGUAGES[row.get_selected()]
        i18n.set_language(self.settings.language)
        self.app.save_settings()
        # Not from inside this notify emission: _translate swaps the row's model.
        GLib.idle_add(once(self._translate))

    def _show_extension_state(self, enabled):
        if enabled is None:
            return  # not installed, or not GNOME Shell
        with self.extension_row.handler_block(self._extension_handler):
            self.extension_row.set_active(enabled)
        self.extension_row.set_visible(True)

    # -- controller ------------------------------------------------------------
    def _apply_settings(self, *_args):
        if self.mode_row.get_model() is None:  # still being built
            return
        s = self.settings
        selected = self.mode_row.get_selected()
        s.auto_mode = selected == 0
        if not s.auto_mode:
            s.width, s.height, s.fps = MODE_LIST[selected - 1]
        s.bitrate = int(self.bitrate_row.get_value() * 1000)
        s.touch = self.touch_row.get_active()
        s.use_adb = self.adb_row.get_active()
        self.app.save_settings()

    def _toggle(self, _button):
        if self.controller.running:
            self.controller.stop()
        else:
            self.controller.start()

    def _refresh(self):
        state = self.controller.state
        title, detail = self.app.status_text()
        self.status_icon.set_from_icon_name(ICONS[state])
        self.status_title.set_label(title)
        self.status_detail.set_label(detail)
        active = self.controller.running
        self.button.set_label(_("Disconnect") if active else _("Connect"))
        self.button.set_css_classes(["pill", "destructive-action" if active else "suggested-action"])
        in_session = state in (State.CONNECTING, State.STREAMING)
        for row in self.session_rows:
            row.set_sensitive(not in_session)
        if self.background_row.get_active() != self.settings.background:
            self.background_row.set_active(self.settings.background)
        self.sign_row.set_subtitle(self._sign_subtitle())

    def _sign_subtitle(self):
        text = _("In Files, for PDFs, while a tablet is on the network")
        nearby = self.app.nearby
        if not self.settings.sign_on_tablet or not nearby.known:
            return text
        names = nearby.names()
        if names:
            return text + "\n" + _("On the network now: {name}").format(name=", ".join(names))
        return text + "\n" + (_("A tablet is on the network now") if nearby.tablets
                               else _("No tablet on the network now"))

    def _append_log(self, line):
        buf = self.log_view.get_buffer()
        buf.insert(buf.get_end_iter(), line + "\n")
        if buf.get_line_count() > 400:
            buf.delete(buf.get_start_iter(), buf.get_iter_at_line(100)[1])
        self.log_view.scroll_to_iter(buf.get_end_iter(), 0, False, 0, 0)

    def _on_close(self, _window):
        self.app.listeners.remove(self._refresh)
        self.app.log_listeners.remove(self._append_log)
        self.app.window_closed()
        return False
