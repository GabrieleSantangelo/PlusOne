"""Plus One's window (see app.py): a sidebar of what it does, each page with
its button and its settings; pairing and the log in dialogs of their own."""
import os
import threading

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Adw, Gdk, Gio, GLib, Gtk, Pango  # noqa: E402

from . import __version__, link, log, once, pen  # noqa: E402
from .app import APP_ID, IN_FLATPAK, set_shell_extension, shell_extension_state  # noqa: E402
from .devices import neural_available  # noqa: E402
from .engine import State, parse_size  # noqa: E402
from .i18n import _  # noqa: E402

TABLET_ICON = "tablet-symbolic"  # the system's (Adwaita, Yaru): drawn in the theme's colours
WEBSITE = "https://github.com/GabrieleSantangelo/PlusOne"
QUALITY_LIST = ["720p", "1080p"]
# The second screen's picture, by what it is for: (label, kbit/s).
QUALITY_BITRATES = [("Balanced", 20000), ("Sharper text", 40000), ("For weak Wi-Fi", 10000)]
FACING_LIST = ["front", "back"]
NOISE_LIST = ["off", "light", "neural"]  # devices.NOISE


def device_status(kind, state, users=()):
    """What the window says under the camera's, the microphone's or the speakers' switch."""
    if kind == "speakers":
        return {
            "setup": _("Needs PipeWire's pactl and pw-link, and python3-cryptography"),
            "off": _("As a sound output for this PC's programs"),
            "ready": _("On: choose “Tablet speakers” as the output in the Sound settings"),
            "in use": _("Playing {programs}").format(programs=", ".join(users)) if users else _("Playing"),
            "away": _("A program plays on the speakers, but the tablet is not reachable"),
            "tablet-off": _("Not allowed on the tablet: Plus One → For the PC"),
            "tablet-unpaired": _("The tablet no longer knows this PC: pair it again"),
            "tablet-version": _("Plus One on the tablet and on this PC are different versions: update them"),
        }.get(state, "")
    if state == "setup":
        return (_("Needs the v4l2loopback module: see the README") if kind == "camera"
                else _("Needs PipeWire's pactl and pw-link"))
    if state == "webrtc":
        return _("Needs WebRTC for GStreamer: gstreamer1.0-nice and gir1.2-gst-plugins-bad-1.0")
    if state == "reload":
        return _("The webcam module is loaded with other settings: sudo modprobe -r v4l2loopback && "
                 "sudo modprobe v4l2loopback")
    if state == "decoder":
        return _("Needs an H.264 decoder: gstreamer1.0-libav")
    camera = kind == "camera"
    return {
        "off": _("As a webcam for this PC's programs") if camera else _("As a microphone for this PC's programs"),
        "ready": _("On: the tablet turns its camera on while a program uses it") if camera
        else _("On: the tablet turns its microphone on while a program uses it"),
        # Which program: any on this PC may use it while the switch is on.
        "in use": _("In use by {programs}").format(programs=", ".join(users)) if users else _("In use"),
        "away": _("A program uses the camera, but the tablet is not reachable") if camera
        else _("A program uses the microphone, but the tablet is not reachable"),
        "tablet-off": _("Not allowed on the tablet: Plus One → For the PC"),
        "tablet-permission": _("On the tablet, allow Plus One to use the camera") if camera
        else _("On the tablet, allow Plus One to use the microphone"),
        "tablet-background": _("On the tablet, let Plus One display over other apps"),
        "tablet-unavailable": _("On the tablet, another app is using the camera") if camera
        else _("On the tablet, another app is using the microphone"),
        "tablet-unpaired": _("The tablet no longer knows this PC: pair it again"),
        "tablet-version": _("Plus One on the tablet and on this PC are different versions: update them"),
    }.get(state, "")
CSS = b".pair-code { font-size: 300%; font-weight: 800; letter-spacing: 0.1em; }"
_resources_added = False


def _add_resources(display):
    """Plus One's icons and style, once."""
    global _resources_added
    if not _resources_added:
        Gtk.IconTheme.get_for_display(display).add_search_path(os.path.join(os.path.dirname(__file__), "icons"))
        style = Gtk.CssProvider()
        style.load_from_data(CSS, len(CSS))
        Gtk.StyleContext.add_provider_for_display(display, style, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)
        _resources_added = True


def _pill(label, *styles, width=-1):
    return Gtk.Button(label=label, halign=Gtk.Align.CENTER, css_classes=["pill", *styles], width_request=width)


def _spinner(text):
    box = Gtk.Box(spacing=12, halign=Gtk.Align.CENTER)
    box.append(Gtk.Spinner(spinning=True))
    box.append(Gtk.Label(label=text, wrap=True))
    return box


# The window's sections, all listed in its sidebar: (key, icon, title).
SECTIONS = [
    ("screen", "video-display-symbolic", "Second screen"),
    ("signing", "document-edit-symbolic", "Signing"),
    ("camera", "camera-web-symbolic", "Camera"),
    ("microphone", "audio-input-microphone-symbolic", "Microphone"),
    ("speakers", "audio-speakers-symbolic", "Speakers"),
    ("tablets", TABLET_ICON, "Tablets"),
    ("general", "preferences-system-symbolic", "General"),
]
DEVICES = ("camera", "microphone", "speakers")


class Window(Adw.ApplicationWindow):
    """A sidebar of what Plus One does, each page with its button and every
    setting it has in view: no menu, no Advanced, no preferences elsewhere.
    Narrow, the sidebar and the page take turns."""

    def __init__(self, app):
        super().__init__(application=app, title="Plus One", default_width=900, default_height=640,
                         width_request=360, height_request=400)
        self.app = app
        self.controller = app.controller
        self.settings = app.settings
        _add_resources(self.get_display())
        for name, callback in (("preferences", lambda *a: self.show_section("general")),
                               ("log", lambda *a: LogDialog(self.app).present(self)),
                               ("about", lambda *a: self._about())):
            action = Gio.SimpleAction.new(name, None)
            action.connect("activate", callback)
            self.add_action(action)
        # The tablet in use (the Tablets page's rows), locked while connected.
        self.use_action = Gio.SimpleAction.new_stateful("use-tablet", GLib.VariantType("s"), GLib.Variant("s", ""))
        self.use_action.connect("change-state", self._on_use)
        self.add_action(self.use_action)
        self._build()
        narrow = Adw.Breakpoint.new(Adw.BreakpointCondition.parse("max-width: 640sp"))
        narrow.add_setter(self.split, "collapsed", True)
        self.add_breakpoint(narrow)
        app.listeners.append(self._refresh)
        self.connect("close-request", self._on_close)

    def _build(self):
        self.device_rows = {}  # camera, microphone, speakers: their switches
        self.stack = Gtk.Stack(transition_type=Gtk.StackTransitionType.CROSSFADE)
        self.stack.add_named(self._empty_page(), "empty")
        self.split = Adw.NavigationSplitView(min_sidebar_width=220, max_sidebar_width=260)
        self.split.set_sidebar(Adw.NavigationPage(title="Plus One", child=self._sidebar()))
        self.pages = {}
        for key, _icon, title in SECTIONS:
            content = getattr(self, f"_{key}_page")()
            toolbar = Adw.ToolbarView(content=content)
            toolbar.add_top_bar(Adw.HeaderBar())
            self.pages[key] = Adw.NavigationPage(title=_(title), tag=key, child=toolbar)
        self.stack.add_named(self.split, "tablet")
        self.toasts = Adw.ToastOverlay(child=self.stack)
        self.set_content(self.toasts)
        self._tablets_shown = None
        self.show_section("screen", reveal=False)
        self._refresh()

    def _empty_page(self):
        empty = Adw.StatusPage(icon_name=TABLET_ICON, title=_("No tablet"),
                               description=_("Install Plus One on the tablet too, then pair it to use it as a second "
                                             "screen, to sign PDFs and as a webcam."))
        pair = _pill(_("Pair a tablet"), "suggested-action")
        pair.connect("clicked", lambda b: self.pair())
        get = Gtk.LinkButton(uri=WEBSITE + "/releases/latest", label=_("Get the tablet app"))
        first = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12, halign=Gtk.Align.CENTER)
        first.append(pair)
        first.append(get)
        empty.set_child(first)
        toolbar = Adw.ToolbarView(content=empty)
        toolbar.add_top_bar(Adw.HeaderBar())
        return toolbar

    # -- the sidebar -----------------------------------------------------------------
    def _sidebar(self):
        # The tablet in use and where it is, on top: a click shows the tablets.
        card = Gtk.Box(spacing=12, margin_start=6, margin_end=6)
        card.append(Gtk.Image(icon_name=TABLET_ICON, pixel_size=32))
        names = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, valign=Gtk.Align.CENTER)
        self.name_label = Gtk.Label(xalign=0, css_classes=["heading"], wrap=True, lines=2,
                                    ellipsize=Pango.EllipsizeMode.END, wrap_mode=Pango.WrapMode.WORD_CHAR)
        self.where_label = Gtk.Label(xalign=0, css_classes=["caption", "dim-label"], ellipsize=Pango.EllipsizeMode.END)
        names.append(self.name_label)
        names.append(self.where_label)
        card.append(names)
        self.name_button = Gtk.Button(child=card, css_classes=["flat"], margin_start=6, margin_end=6,
                                      margin_top=6, margin_bottom=6, tooltip_text=_("Choose the tablet"))
        self.name_button.connect("clicked", lambda b: self.show_section("tablets"))
        self.sidebar_list = Gtk.ListBox(css_classes=["navigation-sidebar"])
        self.sidebar_rows, self.sidebar_marks = {}, {}
        for key, icon, title in SECTIONS:
            box = Gtk.Box(spacing=12)
            box.append(Gtk.Image(icon_name=icon))
            box.append(Gtk.Label(label=_(title), xalign=0, hexpand=True))
            # On (dim) or at work (green): the second screen, camera, microphone, speakers.
            mark = Gtk.Image(icon_name="media-record-symbolic", pixel_size=8, visible=False)
            box.append(mark)
            row = Gtk.ListBoxRow(child=box)
            row.key = key
            self.sidebar_list.append(row)
            self.sidebar_rows[key], self.sidebar_marks[key] = row, mark
        self.sidebar_list.connect("row-activated", lambda _l, row: self.show_section(row.key))
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        box.append(self.name_button)
        box.append(Gtk.ScrolledWindow(child=self.sidebar_list, vexpand=True,
                                      hscrollbar_policy=Gtk.PolicyType.NEVER))
        toolbar = Adw.ToolbarView(content=box)
        toolbar.add_top_bar(Adw.HeaderBar())
        return toolbar

    def show_section(self, key, reveal=True):
        if self.split.get_content() is not self.pages[key]:
            self.split.set_content(self.pages[key])
        self.sidebar_list.select_row(self.sidebar_rows[key])
        if reveal:
            self.split.set_show_content(True)

    # -- the pages ---------------------------------------------------------------------
    def _screen_page(self):
        s = self.settings
        page = Adw.PreferencesPage()
        hero = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12, margin_top=12, margin_bottom=12)
        self.screen_button = _pill("", width=260)
        self.screen_button.connect("clicked", self._toggle)
        # Something happens while it turns: looking for the tablet, connecting.
        self.spinner = Gtk.Spinner(spinning=True, visible=False)
        self.status_label = Gtk.Label(wrap=True, justify=Gtk.Justification.CENTER, css_classes=["dim-label"])
        status = Gtk.Box(spacing=8, halign=Gtk.Align.CENTER)
        status.append(self.spinner)
        status.append(self.status_label)
        hero.append(self.screen_button)
        hero.append(status)
        group = Adw.PreferencesGroup()
        group.add(hero)
        page.add(group)

        group = Adw.PreferencesGroup(title=_("Picture"))
        # Said as what it is for, not in Mbit/s (QUALITY_BITRATES).
        self.quality_row = Adw.ComboRow(title=_("Quality"), model=Gtk.StringList.new(
            [_(label) for label, _kbit in QUALITY_BITRATES]),
            selected=min(range(len(QUALITY_BITRATES)), key=lambda i: abs(QUALITY_BITRATES[i][1] - s.bitrate)),
            subtitle=_("Sharper text takes more of the Wi-Fi"))
        # Entry 0 is "Automatic"; the rest are the tablet's sizes: _fill_modes().
        self.mode_row = Adw.ComboRow(title=_("Resolution"), subtitle_lines=2)
        for row in (self.quality_row, self.mode_row):
            group.add(row)
        page.add(group)

        group = Adw.PreferencesGroup(title=_("Touch and pen"))
        # Its subtitle says how to get the pen's pressure.
        self.touch_row = Adw.SwitchRow(title=_("Touch and S Pen"), active=s.touch, subtitle_selectable=True,
                                       subtitle={
            "set up": _("Without Plus One's pen helper (the .deb installs it) the S Pen works as a mouse, "
                        "without pressure"),
        }.get(pen.uhid_missing(), _("Fingers and the pen drive the desktop; the pen with pressure and tilt")))
        group.add(self.touch_row)
        page.add(group)

        self._fill_modes()
        # Read when a connection starts: locked while one is up.
        self.session_rows = [self.quality_row, self.mode_row, self.touch_row]
        for row, signal in ((self.quality_row, "notify::selected"), (self.mode_row, "notify::selected"),
                            (self.touch_row, "notify::active")):
            row.connect(signal, self._apply)
        return page

    def _signing_page(self):
        page = Adw.PreferencesPage()
        hero = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12, margin_top=12, margin_bottom=12)
        sign = _pill(_("Sign a PDF…"), "suggested-action", width=260)
        sign.connect("clicked", lambda b: self.app.choose_and_sign(self))
        hero.append(sign)
        hero.append(Gtk.Label(label=_("Or right-click a PDF in Files: Sign on Tablet"), wrap=True,
                              justify=Gtk.Justification.CENTER, css_classes=["dim-label"]))
        group = Adw.PreferencesGroup()
        group.add(hero)
        page.add(group)
        group = Adw.PreferencesGroup()
        signatures = Adw.ActionRow(title=_("My signatures"), activatable=True,
                                   subtitle=_("Placed with one tap when signing, here and on the tablet"))
        signatures.add_suffix(Gtk.Image(icon_name="go-next-symbolic"))
        signatures.connect("activated", lambda r: self._show_signatures())
        # Files and Open With are outside a Flatpak's reach.
        sign_row = Adw.SwitchRow(title=_("Sign on Tablet"), active=self.settings.sign_on_tablet,
                                 visible=not IN_FLATPAK, subtitle=_("In Files and Open With, for PDFs"))
        sign_row.connect("notify::active", lambda row, _p: self.app.set_sign_on_tablet(row.get_active()))
        group.add(signatures)
        group.add(sign_row)
        page.add(group)
        return page

    def _device_row(self, kind, title):
        row = Adw.SwitchRow(title=title, subtitle_lines=3, use_markup=False)
        row.handler = row.connect("notify::active", lambda r, _p: self.app.set_device(kind, r.get_active()))
        self.device_rows[kind] = row
        return row

    def _camera_page(self):
        s = self.settings
        page = Adw.PreferencesPage()
        group = Adw.PreferencesGroup(description=_("The tablet's camera as a webcam that every program on this "
                                                   "PC sees; the tablet turns it on only while one uses it."))
        group.add(self._device_row("camera", _("Tablet camera")))
        self.facing_row = Adw.ComboRow(title=_("Camera"), model=Gtk.StringList.new([_("Front"), _("Back")]),
                                       selected=FACING_LIST.index(s.camera_facing)
                                       if s.camera_facing in FACING_LIST else 0)
        self.quality_row_camera = Adw.ComboRow(title=_("Picture"), model=Gtk.StringList.new(
            ["720p (1280 × 720)", "1080p (1920 × 1080)"]),
            selected=QUALITY_LIST.index(s.camera_quality) if s.camera_quality in QUALITY_LIST else 0,
            subtitle=_("1080p takes about twice the bandwidth. A change waits until no program uses the camera"))
        for row in (self.facing_row, self.quality_row_camera):
            row.connect("notify::selected", self._camera_options)
            group.add(row)
        page.add(group)
        return page

    def _microphone_page(self):
        s = self.settings
        page = Adw.PreferencesPage()
        group = Adw.PreferencesGroup(description=_("The tablet's microphone for this PC's programs; the tablet "
                                                   "turns it on only while one records."))
        group.add(self._device_row("microphone", _("Tablet microphone")))
        neural = neural_available()
        self.noise_row = Adw.ComboRow(
            title=_("Noise reduction"),
            model=Gtk.StringList.new([_("Off"), _("Light, on the tablet"), _("Neural, on this PC")]),
            selected=NOISE_LIST.index(s.microphone_noise) if s.microphone_noise in NOISE_LIST else 2,
            subtitle=_("Neural takes out more of the room's noise, with about half a processor core")
            if neural else _("Neural needs DeepFilterNet's plugin in ~/.ladspa: light until then"))
        self.noise_row.connect("notify::selected", lambda row, _p: self.app.set_microphone_noise(
            NOISE_LIST[row.get_selected()]))
        group.add(self.noise_row)
        page.add(group)
        return page

    def _speakers_page(self):
        page = Adw.PreferencesPage()
        group = Adw.PreferencesGroup(description=_("The tablet's speakers as a sound output of this PC, there "
                                                   "while the tablet is: the sound as it is, uncompressed."))
        group.add(self._device_row("speakers", _("Tablet speakers")))
        page.add(group)
        return page

    def _tablets_page(self):
        page = Adw.PreferencesPage()
        self.tablets_group = Adw.PreferencesGroup(description=_("The one in use gets the desktop and the PDFs. "
                                                                "Forget one you no longer use."))
        page.add(self.tablets_group)
        group = Adw.PreferencesGroup()
        pair = Adw.ActionRow(title=_("Pair another tablet…"), activatable=True)
        pair.add_prefix(Gtk.Image(icon_name="list-add-symbolic"))
        pair.connect("activated", lambda r: self.pair())
        get = Adw.ActionRow(title=_("Get the tablet app"), activatable=True,
                            subtitle=_("Plus One's app, for Android 13 or later"))
        get.add_suffix(Gtk.Image(icon_name="go-next-symbolic"))
        get.connect("activated", lambda r: Gtk.UriLauncher.new(WEBSITE + "/releases/latest").launch(self, None, None))
        group.add(pair)
        group.add(get)
        page.add(group)
        group = Adw.PreferencesGroup()
        updates = Adw.SwitchRow(title=_("Keep the tablet's app up to date"), active=self.settings.update_tablet,
                                subtitle=_("With this PC's version, while nothing is in use"))
        updates.connect("notify::active", lambda row, _p: self._set_update_tablet(row.get_active()))
        group.add(updates)
        page.add(group)
        self.tablet_rows = []
        return page

    def _set_update_tablet(self, on):
        self.settings.update_tablet = on
        self.app.save_settings()
        if on:
            self.app.tablet_updates.check()

    def _general_page(self):
        s = self.settings
        page = Adw.PreferencesPage()
        group = Adw.PreferencesGroup()
        self.background_row = background = Adw.SwitchRow(
            title=_("Ready in the background"), active=s.background,
            subtitle=_("Starts at login and stays ready after you close this window"))
        background.handler = background.connect("notify::active",
                                                lambda row, _p: self.app.set_background(row.get_active()))
        # Shown once the shell says the extension is installed (never in a Flatpak).
        self.extension_row = Adw.SwitchRow(title=_("Quick Settings toggle"), visible=False,
                                           subtitle=_("Adds a “Tablet” button to GNOME's Quick Settings"))
        self._extension_handler = self.extension_row.connect(
            "notify::active", lambda row, _p: set_shell_extension(row.get_active()))
        shell_extension_state(self._show_extension_state)
        group.add(background)
        group.add(self.extension_row)
        page.add(group)
        group = Adw.PreferencesGroup()
        for title, subtitle, action in ((_("Log"), _("What Plus One did, to send along with a problem"), "win.log"),
                                        (_("About Plus One"), None, "win.about")):
            row = Adw.ActionRow(title=title, subtitle=subtitle or "", activatable=True)
            row.add_suffix(Gtk.Image(icon_name="go-next-symbolic"))
            row.connect("activated", lambda r, action=action: self.activate_action(action, None))
            group.add(row)
        page.add(group)
        return page

    # -- state -----------------------------------------------------------------------
    def _refresh(self):
        # Changed elsewhere too: Quick Settings' toggle, the first desktop
        # through the app (its sizes).
        with self.background_row.handler_block(self.background_row.handler):
            self.background_row.set_active(self.settings.background)
        if self.settings.app_screen != self._modes_for:
            self._fill_modes()
        tablet = self.app.tablets.active()
        self.stack.set_visible_child_name("tablet" if tablet else "empty")
        if tablet is None:
            return
        state = self.controller.state
        self.name_label.set_label(tablet.name)
        here = bool(self.app.tablets.addresses(tablet))
        self.where_label.set_label(_("Second screen") if state == State.STREAMING
                                   else _("Ready") if here else _("Not found"))
        self._refresh_tablets(tablet)
        for kind in DEVICES:
            row = self.device_rows[kind]
            with row.handler_block(row.handler):
                row.set_active(getattr(self.settings, kind))
            device = getattr(self.app.devices, kind)
            row.set_subtitle(device_status(kind, device.state, device.users))
            self._mark(kind, getattr(self.settings, kind), device.state == "in use")
        self._mark("screen", self.controller.running, state == State.STREAMING)
        text = self._status(tablet)
        self.status_label.set_label(text)
        self.status_label.set_css_classes(["error"] if state == State.ERROR else ["dim-label"])
        self.spinner.set_visible(state == State.CONNECTING)
        if not self.controller.running:
            self.screen_button.set_label(_("Use as second screen"))
            self.screen_button.set_css_classes(["pill", "suggested-action"])
        else:
            # Nothing is lost either way: no red.
            self.screen_button.set_label(_("Disconnect") if state == State.STREAMING else _("Cancel"))
            self.screen_button.set_css_classes(["pill"])
        in_session = state in (State.CONNECTING, State.STREAMING)
        for row in self.session_rows:
            row.set_sensitive(not in_session)

    def _mark(self, key, on, working):
        mark = self.sidebar_marks[key]
        mark.set_visible(on)
        mark.set_css_classes(["success"] if working else ["dim-label"])

    def _status(self, tablet):
        title, detail = self.app.status_text()
        state = self.controller.state
        if state == State.IDLE and not self.controller.detail[0]:
            # Nothing happening: where the tablet is.
            return _("Ready") if self.app.tablets.addresses(tablet) else _("Not found: open Plus One on the tablet")
        mode = getattr(self.controller, "mode", None)
        by = self.app.connected_by()
        if state == State.STREAMING and mode:
            text = _("Second screen · {width} × {height} at {fps} Hz").format(width=mode[0], height=mode[1],
                                                                              fps=mode[2])
            return _("{detail} · {path}").format(detail=text, path=_(by)) if by else text
        return detail or title

    def _refresh_tablets(self, active):
        running = self.controller.running
        shown = ([(t.fingerprint, t.name) for t in self.app.tablets.all()], active.fingerprint, running)
        self.use_action.set_state(GLib.Variant("s", active.fingerprint))
        # Another tablet while one is connected: not until it is done.
        self.use_action.set_enabled(not running)
        if shown == self._tablets_shown:
            return
        self._tablets_shown = shown
        for row in self.tablet_rows:
            self.tablets_group.remove(row)
        self.tablet_rows = []
        tablets, active_fp, running = shown
        first = None
        for fingerprint, name in tablets:
            row = Adw.ActionRow(title=name, use_markup=False, activatable=not running)
            check = Gtk.CheckButton(active=fingerprint == active_fp, sensitive=not running, valign=Gtk.Align.CENTER,
                                    css_classes=["selection-mode"])
            if first is None:
                first = check
            else:
                check.set_group(first)
            # Not from inside the signal: the rows are made again for it.
            check.connect("toggled", lambda c, fp=fingerprint: c.get_active()
                          and GLib.idle_add(once(self.app.use_tablet, fp)))
            row.add_prefix(check)
            row.set_activatable_widget(check)
            forget = Gtk.Button(label=_("Forget"), valign=Gtk.Align.CENTER, css_classes=["flat"])
            forget.connect("clicked", lambda b, fp=fingerprint, name=name: self._ask_forget(fp, name))
            row.add_suffix(forget)
            self.tablets_group.add(row)
            self.tablet_rows.append(row)
        self.tablets_group.set_title(_("Paired tablets"))

    def _ask_forget(self, fingerprint, name):
        dialog = Adw.AlertDialog(heading=_("Forget {tablet}?").format(tablet=name),
                                 body=_("To use it again, pair it again: both screens show a code."))
        dialog.add_response("cancel", _("Cancel"))
        dialog.add_response("forget", _("Forget"))
        dialog.set_response_appearance("forget", Adw.ResponseAppearance.DESTRUCTIVE)

        def answered(_d, response):
            if response == "forget":
                self.app.forget_tablet(fingerprint)
        dialog.connect("response", answered)
        dialog.present(self)
        return dialog

    def _on_use(self, action, value):
        action.set_state(value)
        self.app.use_tablet(value.get_string())

    def _toggle(self, _button):
        if self.controller.running:
            self.controller.stop()
        else:
            self.controller.start()

    # -- settings ------------------------------------------------------------------------
    def _app_sizes(self):
        """The tablet's sizes through the app, once it said its screen."""
        from .appdisplay import sizes
        screen = parse_size(self.settings.app_screen)
        return sizes(screen) if screen else []

    def _fill_modes(self):
        """Resolution's choices: the tablet's sizes, at its proportions."""
        from .appdisplay import scale_for
        s = self.settings
        self._modes_for = s.app_screen
        choices = self._app_sizes()
        labels = [_("Automatic")]
        for w, h in choices:
            labels.append(f"{w} × {h}" if scale_for((w, h)) == 1 else
                          _("{width} × {height}, sharp: as {half}").format(
                              width=w, height=h, half=f"{w // 2} × {h // 2}"))
        current = parse_size(s.app_size)
        selected = 1 + choices.index(current) if current in choices else 0
        subtitle = (_("Above 1920 × 1200 the picture comes a little later")
                    if choices else _("The tablet's sizes appear after it showed the desktop once"))
        self._filling = True
        self.mode_row.set_model(Gtk.StringList.new(labels))
        self.mode_row.set_selected(selected)
        self.mode_row.set_subtitle(subtitle)
        self._filling = False

    def _apply(self, *_args):
        if getattr(self, "_filling", False):
            return  # the choices being replaced, not chosen
        s = self.settings
        selected = self.mode_row.get_selected()
        choices = self._app_sizes()
        s.app_size = "auto" if selected == 0 or selected > len(choices) else "%dx%d" % choices[selected - 1]
        s.bitrate = QUALITY_BITRATES[self.quality_row.get_selected()][1]
        s.touch = self.touch_row.get_active()
        self.app.save_settings()

    def _camera_options(self, *_args):
        self.app.set_camera_options(FACING_LIST[self.facing_row.get_selected()],
                                    QUALITY_LIST[self.quality_row_camera.get_selected()])

    def _show_extension_state(self, enabled):
        if enabled is None:
            return  # not installed, or not GNOME Shell
        with self.extension_row.handler_block(self._extension_handler):
            self.extension_row.set_active(enabled)
        self.extension_row.set_visible(True)

    # -- dialogs ---------------------------------------------------------------------
    def pair(self):
        PairDialog(self.app, on_paired=self._paired).present(self)

    def _paired(self, name):
        # The name is the tablet's own: shown as text, not markup.
        self.toasts.add_toast(Adw.Toast(title=_("Paired with {name}").format(name=GLib.markup_escape_text(name))))
        if not self.app.settings.toggle_offered:
            shell_extension_state(self._offer_toggle)

    def _offer_toggle(self, enabled):
        """Once, after the first pairing: the Quick Settings toggle, if the
        shell has it and it is off. Turned on only if the user says so."""
        if enabled is not False or self.app.settings.toggle_offered:
            return
        self.app.settings.toggle_offered = True
        self.app.save_settings()
        toast = Adw.Toast(title=_("Add a “Tablet” button to Quick Settings?"), button_label=_("Add"), timeout=0)
        toast.connect("button-clicked", lambda t: set_shell_extension(True))
        self.toasts.add_toast(toast)

    def _show_signatures(self):
        from .ink import load_pen
        from .signature_pad import SignaturesWindow
        from .signing import PEN
        color, width, _smoothing = load_pen(PEN)
        SignaturesWindow(self, color, width).present()

    def _on_close(self, _window):
        self.app.listeners.remove(self._refresh)
        self.app.window_closed()
        return False

    def _about(self):
        Adw.AboutDialog(application_name="Plus One", application_icon=APP_ID, version=__version__,
                        comments=_("Your Galaxy Tab as a second monitor, and to sign PDFs with its S Pen"),
                        website=WEBSITE, issue_url=WEBSITE + "/issues",
                        license_type=Gtk.License.MIT_X11).present(self)


class PairDialog(Adw.Dialog):
    """Pick a tablet with the app on the network, compare the code, done."""

    def __init__(self, app, on_paired=None):
        super().__init__(title=_("Pair a tablet"), content_width=420, content_height=480)
        self.app = app
        self.on_paired = on_paired or (lambda name: None)
        self.answer = None      # (threading.Event, [bool]) while the user compares the code
        self.closed = False
        self.attempt = 0        # each tablet chosen is one; an older one's answers are not this one's
        self.nav = Adw.NavigationView()
        self.set_child(self.nav)

        page = Adw.PreferencesPage()
        self.found = Adw.PreferencesGroup(title=_("With Plus One on your Wi-Fi"),
                                          description=_("Not there? Open Plus One on the tablet, on the same "
                                                        "Wi-Fi as this PC."))
        page.add(self.found)
        self.rows = []
        self.searching = Adw.ActionRow(title=_("Looking for tablets…"))
        self.searching.add_prefix(Gtk.Spinner(spinning=True))
        # Nothing after a while: what to do, not a spinner forever.
        GLib.timeout_add_seconds(10, once(lambda: self.closed or self.searching.set_title(
            _("No tablet found yet. Open Plus One on the tablet, on the same Wi-Fi as this PC."))))
        self.nav.add(self._page(page, _("Pair a tablet")))
        self._listing = None
        self._list()
        app.listeners.append(self._list)
        self.connect("closed", self._on_closed)

    @staticmethod
    def _page(content, title):
        toolbar = Adw.ToolbarView(content=content)
        toolbar.add_top_bar(Adw.HeaderBar())
        return Adw.NavigationPage(child=toolbar, title=title)

    def _list(self):
        apps = [(app_id, name) for app_id, _a, _p, name in self.app.discovery.apps() if app_id]
        if apps == self._listing:
            return
        self._listing = apps
        for row in self.rows + [self.searching]:
            if row.get_parent() is not None:
                self.found.remove(row)
        self.rows = []
        paired = {t.app_id for t in self.app.tablets.all()}
        for app_id, name in apps:
            row = Adw.ActionRow(title=name, use_markup=False, activatable=True,
                                subtitle=_("Paired") if app_id in paired else "")
            row.add_prefix(Gtk.Image(icon_name=TABLET_ICON))
            row.add_suffix(Gtk.Image(icon_name="go-next-symbolic"))
            row.connect("activated", lambda r, app_id=app_id, name=name: self._pair(app_id, name))
            self.found.add(row)
            self.rows.append(row)
        self.found.add(self.searching)  # last: more may come

    def _pair(self, app_id, name):
        """The code page; pairing runs in a thread, asking self._ask."""
        self._answered(False)  # one still comparing (gone back from): not paired
        self.attempt += 1
        attempt = self.attempt
        self.stage = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=18, valign=Gtk.Align.CENTER,
                             margin_start=24, margin_end=24)
        self.stage.append(_spinner(_("Connecting to {name}…").format(name=name)))
        self.nav.push(self._page(self.stage, name))
        targets = self.app.discovery.addresses(app_id)

        def work():
            try:
                fingerprint, paired_name = link.pair(targets, lambda n, code: self._ask(attempt, n, code))
            except link.PairingRefused:
                GLib.idle_add(once(self._failed, attempt, _("Not paired: the code was not confirmed on both screens")))
            except Exception as e:  # noqa: BLE001 - shown to the user
                log("pairing:", repr(e))
                GLib.idle_add(once(self._failed, attempt, _("{name} did not answer").format(name=name)))
            else:
                GLib.idle_add(once(self._done, attempt, fingerprint, paired_name))
        threading.Thread(target=work, daemon=True).start()

    def _current(self, attempt):
        return not self.closed and attempt == self.attempt

    def _ask(self, attempt, name, code):
        """From the pairing thread: blocks until the user compares the code;
        no at once when the dialog closed or another tablet was chosen."""
        if not self._current(attempt):
            return False
        answered, answer = self.answer = (threading.Event(), [])
        GLib.idle_add(once(self._show_code, attempt, code))
        if not answered.wait(link.PAIRING_TIMEOUT):
            log("pairing: the code was not confirmed in time")
        return bool(answer and answer[0])

    def _answered(self, yes):
        if self.answer and not self.answer[0].is_set():
            log("pairing:", "confirmed on the PC" if yes else "refused on the PC")
            self.answer[1].append(yes)
            self.answer[0].set()

    def _show_code(self, attempt, code):
        if not self._current(attempt):
            return
        self._clear()
        self.stage.append(Gtk.Label(label=f"{code[:3]} {code[3:]}", selectable=True,
                                    css_classes=["title-1", "numeric", "pair-code"]))
        self.stage.append(Gtk.Label(label=_("Pair only if the tablet shows this same code. Confirm there too."),
                                    wrap=True, justify=Gtk.Justification.CENTER))
        buttons = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12, halign=Gtk.Align.CENTER)
        same = _pill(_("Same code"), "suggested-action", width=220)
        same.connect("clicked", lambda b: self._confirmed())
        cancel = _pill(_("Cancel"), width=220)
        cancel.connect("clicked", lambda b: (self._answered(False), self.close()))
        buttons.append(same)
        buttons.append(cancel)
        self.stage.append(buttons)

    def _confirmed(self):
        self._answered(True)
        self._clear()
        self.stage.append(_spinner(_("Confirm on the tablet…")))

    def _done(self, attempt, fingerprint, name):
        if self.closed:
            return
        if attempt != self.attempt:  # paired all the same: in use only if it is the one shown
            return
        if not self.app.controller.running:  # else the connected one stays in use (Tablets later)
            self.app.use_tablet(fingerprint)
        self.on_paired(name)
        self.close()

    def _failed(self, attempt, text):
        if not self._current(attempt):
            return
        self._clear()
        self.stage.append(Gtk.Label(label=text, wrap=True, justify=Gtk.Justification.CENTER))
        again = _pill(_("Try again"))
        again.connect("clicked", lambda b: self.nav.pop())
        self.stage.append(again)

    def _clear(self):
        while (child := self.stage.get_first_child()) is not None:
            self.stage.remove(child)

    def _on_closed(self, _dialog):
        self.closed = True
        self._answered(False)  # closed while comparing: not paired
        self.app.listeners.remove(self._list)


class LogDialog(Adw.Dialog):
    def __init__(self, app):
        super().__init__(title=_("Log"), content_width=640, content_height=480)
        self.app = app
        self.view = Gtk.TextView(editable=False, monospace=True, cursor_visible=False,
                                 wrap_mode=Gtk.WrapMode.WORD_CHAR, top_margin=12, bottom_margin=12,
                                 left_margin=12, right_margin=12)
        buf = self.view.get_buffer()
        buf.set_text("".join(line + "\n" for line in app.log_lines))
        self.end = buf.create_mark(None, buf.get_end_iter(), False)  # follows what is added
        copy = Gtk.Button(icon_name="edit-copy-symbolic", tooltip_text=_("Copy"))
        copy.connect("clicked", lambda b: self.get_clipboard().set_content(Gdk.ContentProvider.new_for_bytes(
            "text/plain;charset=utf-8", GLib.Bytes.new(self._text().encode()))))
        header = Adw.HeaderBar()
        header.pack_start(copy)
        toolbar = Adw.ToolbarView(content=Gtk.ScrolledWindow(child=self.view, vexpand=True))
        toolbar.add_top_bar(header)
        self.set_child(toolbar)
        app.log_listeners.append(self._append)
        self.connect("closed", lambda d: app.log_listeners.remove(self._append))
        GLib.idle_add(once(self._to_end))

    def _text(self):
        buf = self.view.get_buffer()
        return buf.get_text(buf.get_start_iter(), buf.get_end_iter(), False)

    def _append(self, line):
        buf = self.view.get_buffer()
        buf.insert(buf.get_end_iter(), line + "\n")
        if buf.get_line_count() > 400:
            buf.delete(buf.get_start_iter(), buf.get_iter_at_line(100)[1])
        self._to_end()

    def _to_end(self):
        self.view.scroll_to_mark(self.end, 0, False, 0, 0)
