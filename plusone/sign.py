"""Full-screen window for signing a PDF with the pen, shown on the tablet.

The S Pen draws (with pressure) and, with its side button held, erases;
fingers never draw, so a palm resting on the screen leaves no marks. Two
fingers zoom and move the page. A mouse also draws, for signing on the PC.
"""
import datetime
import faulthandler
import math
import os
import threading
import time

import cairo
import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
gi.require_version("Graphene", "1.0")
gi.require_version("Poppler", "0.18")
from gi.repository import Adw, Gdk, Gio, GLib, Graphene, Gtk, Poppler  # noqa: E402

from . import log, once  # noqa: E402
from .i18n import _  # noqa: E402
from .ink import (MAX_WIDTH, MIN_WIDTH, RawPen, View, load_pen, load_signature,  # noqa: E402
                  place, save_pen, save_signature, touches)
from .modeler import LEVELS, Params, StrokeModeler  # noqa: E402
from .pdf import INK, Stroke, Text, draw_marks, output_path, save_signed  # noqa: E402
from .pen import feed as pen_feed  # noqa: E402

SIGNATURE = os.path.join(GLib.get_user_data_dir(), "plus-one", "signature.json")
PEN = os.path.join(GLib.get_user_config_dir(), "plus-one", "pen.json")
PEN_SOURCES = (Gdk.InputSource.PEN, Gdk.InputSource.TOUCHSCREEN)
NO_PRESSURE = 0.35  # mouse, or a pen that reports none: a medium line
ERASER_RADIUS = 12  # widget pixels around the pen tip
# The S Pen's side button arrives as the stylus' second or third button.
ERASE_BUTTONS = Gdk.ModifierType.BUTTON2_MASK | Gdk.ModifierType.BUTTON3_MASK
RAW_TOLERANCE = 24  # widget pixels between where the tablet and GTK put the tip
TOOLBAR_SPACE = 84  # pixels kept clear under the page for the floating toolbar
# The S Pen reports at a steady rate, but they reach us in bursts over
# Wi-Fi: a stroke's clock counts reports rather than reading arrival times.
REPORT_INTERVAL = 1 / 285
MIN_STEP = 1.0  # widget pixels a modelled point must move to be kept
STALL = 3  # seconds without answering: GNOME is about to call the window frozen

CSS = """
.sign-canvas { background-color: mix(@window_bg_color, @window_fg_color, 0.08); }
.floating-bar {
  background-color: @popover_bg_color;
  color: @popover_fg_color;
  border-radius: 999px;
  padding: 5px;
  box-shadow: 0 0 0 1px alpha(black, 0.06), 0 2px 6px alpha(black, 0.1), 0 6px 20px alpha(black, 0.1);
}
.floating-bar button { min-width: 42px; min-height: 42px; padding: 0; border-radius: 999px; }
.floating-bar button.text-button { padding: 0 14px; }
.floating-bar separator { margin: 9px 4px; }
.floating-bar .page-number { font-feature-settings: "tnum"; margin: 0 2px; }
.hint-bar {
  background-color: @accent_bg_color;
  color: @accent_fg_color;
  border-radius: 999px;
  padding: 4px 4px 4px 18px;
  font-weight: bold;
  box-shadow: 0 2px 6px alpha(black, 0.15), 0 6px 20px alpha(black, 0.12);
}
.hint-bar button { color: inherit; min-width: 34px; min-height: 34px; padding: 0; border-radius: 999px; }
button.swatch, button.swatch:hover, button.swatch:checked, button.swatch:active {
  background: none; box-shadow: none; min-width: 38px; min-height: 38px; padding: 0;
}
"""


def load_css():
    provider = Gtk.CssProvider()
    if hasattr(provider, "load_from_string"):  # GTK 4.12
        provider.load_from_string(CSS)
    else:
        provider.load_from_data(CSS.encode(), -1)
    Gtk.StyleContext.add_provider_for_display(Gdk.Display.get_default(), provider,
                                              Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)


def tablet_monitor():
    """The tablet's virtual monitor (Mutter names them Meta-N), if any."""
    monitors = Gdk.Display.get_default().get_monitors()
    found = None
    for i in range(monitors.get_n_items()):
        monitor = monitors.get_item(i)
        if (monitor.get_connector() or "").startswith("Meta-"):
            found = monitor
    return found


def open_document(path):
    """A Poppler document, or raise GLib.Error (missing, damaged, password)."""
    return Poppler.Document.new_from_file(Gio.File.new_for_path(path).get_uri(), None)


class PageArea(Gtk.DrawingArea):
    """One page with its marks; turns pen/mouse input into page coordinates."""

    def __init__(self, window):
        # A natural size, or a window that is not maximized shrinks to nothing.
        super().__init__(hexpand=True, vexpand=True, focusable=True,
                         content_width=480, content_height=600, css_classes=["sign-canvas"])
        self.window = window
        self.view = View()
        self.fitted = False
        self._cache = None  # (page index, zoom, surface)
        self._stroke = None
        self._model = None    # the stroke's modeler, in widget pixels
        self._clock = 0.0     # time of the stroke's last sample, seconds
        self._skipped = None  # last modelled point, if too close to keep (yet)
        self._by_mouse = False  # the stroke being drawn is the mouse's
        self._eraser = None   # page point under the eraser, while erasing
        self._erased = False  # this eraser stroke removed something already
        self._logged_tool = False
        self._logged_mismatch = False
        self._logged_drag = False
        self.raw = RawPen(pen_feed.listen())
        self._raw_map = None    # tablet (0..1) to widget pixels, while drawing from it
        self._raw_tick = None
        self.raw_counts = [0, 0]  # samples drawn from the tablet, desktop events meanwhile
        self._hover = None  # pen or mouse position while something waits to be placed
        self.set_draw_func(self._draw)
        self.connect("resize", lambda *a: self.fit())

        stylus = Gtk.GestureStylus()
        stylus.connect("down", self._pen_down)
        stylus.connect("motion", self._pen_motion)
        stylus.connect("up", self._pen_up)
        stylus.connect("cancel", lambda g, seq: self._pen_cancel())
        self.add_controller(stylus)

        mouse = Gtk.GestureDrag(button=Gdk.BUTTON_PRIMARY)
        mouse.connect("drag-begin", self._mouse_begin)
        mouse.connect("drag-update", self._mouse_update)
        mouse.connect("drag-end", self._mouse_end)
        self.add_controller(mouse)

        tap = Gtk.GestureClick(touch_only=True)
        tap.connect("released", lambda g, n, x, y: self.window.place_at(*self.view.to_page(x, y)))
        self.add_controller(tap)

        zoom = Gtk.GestureZoom()
        zoom.connect("begin", self._zoom_begin)
        zoom.connect("scale-changed", self._zoom_changed)
        zoom.connect("end", lambda *a: self._sharpen())
        self.add_controller(zoom)

        scroll = Gtk.EventControllerScroll(flags=Gtk.EventControllerScrollFlags.BOTH_AXES)
        scroll.connect("scroll", self._scroll)
        self.add_controller(scroll)

        hover = Gtk.EventControllerMotion()
        hover.connect("motion", self._hover_at)
        hover.connect("leave", lambda c: self._hover_at(c, None, None))
        self.add_controller(hover)

    # -- view ------------------------------------------------------------------
    def page_size(self):
        return self.window.page().get_size()

    def fit(self):
        w, h = self.get_width(), self.get_height()
        if w and h:
            self.view.fit(*self.page_size(), w, h, margin=24, bottom=TOOLBAR_SPACE)
            self.fitted = True
            self.queue_draw()

    def _zoom_begin(self, gesture, _seq):
        ok, x, y = gesture.get_bounding_box_center()
        self._zoom_start = (self.view.zoom, self.view.to_page(x, y))

    def _zoom_changed(self, gesture, scale):
        ok, x, y = gesture.get_bounding_box_center()
        zoom, anchor = self._zoom_start
        # Pinch zooms, moving both fingers pans: the anchor follows them.
        self.view.zoom_at(zoom * scale, x, y, anchor)
        self.queue_draw()

    def _sharpen(self):
        """After a pinch, the page and its marks at exactly the new zoom."""
        self._cache = None
        self.queue_draw()

    def _scroll(self, controller, dx, dy):
        state = controller.get_current_event_state()
        if state & Gdk.ModifierType.CONTROL_MASK:
            x, y = self.get_width() / 2, self.get_height() / 2
            self.view.zoom_at(self.view.zoom * (0.9 if dy > 0 else 1.1), x, y)
        else:
            self.view.pan(-dx * 40, -dy * 40)
        self.queue_draw()
        return True

    # -- input -----------------------------------------------------------------
    def _hover_at(self, _controller, x, y):
        """Pen hovering (or mouse): shows what a tap would place, and where."""
        hover = (x, y) if x is not None and self.window.pending else None
        if hover != self._hover:
            self._hover = hover
            self.queue_draw()

    def _pressure(self, gesture):
        ok, value = gesture.get_axis(Gdk.AxisUse.PRESSURE)
        return value if ok and value > 0 else NO_PRESSURE

    # Samples go through a stroke modeler (see modeler.py), as in Notewise:
    # the ink is smoothed by the pen's stabilization level, and while drawing
    # a prediction covers the modeler's lag and some of the stream's.
    def _begin(self, x, y, pressure, t=0.0):
        px, py = self.view.to_page(x, y)
        self._by_mouse = False
        if self.window.place_at(px, py):
            return
        self._stroke = Stroke([], width=self.window.pen_width, color=self.window.pen_color)
        self._model = StrokeModeler(Params.smoothing(self.window.pen_smoothing))
        self._clock = t
        self._add(self._model.down(x, y, pressure, t))

    def _extend(self, x, y, pressure, t=None):
        """One more sample, by default one pen report after the last."""
        if self._stroke is None:
            return
        self._clock = self._clock + REPORT_INTERVAL if t is None else t
        self._add(self._model.move(x, y, pressure, self._clock))

    def _add(self, modelled):
        """Modelled widget points into the stroke, in page points; those
        that barely moved are left out."""
        points = self._stroke.points
        for x, y, pressure in modelled:
            point = (*self.view.to_page(x, y), pressure)
            if points and math.dist(point[:2], points[-1][:2]) * self.view.zoom < MIN_STEP:
                self._skipped = point
            else:
                points.append(point)
                self._skipped = None
        self.queue_draw()

    def _backlog(self, gesture):
        """Samples GTK merged into this motion event (it delivers one per
        frame); without them fast curves turn into polygons."""
        try:
            ok, coords = gesture.get_backlog()
        except (TypeError, ValueError):
            return []
        samples = []
        for coord in coords if ok else []:
            axes = coord.axes
            pressure = axes[Gdk.AxisUse.PRESSURE]
            samples.append((axes[Gdk.AxisUse.X], axes[Gdk.AxisUse.Y], pressure if pressure > 0 else NO_PRESSURE))
        return samples

    def _finish(self):
        if self._stroke is not None:
            # The ink catches up with the pen, and ends where it lifted.
            self._add(self._model.up(*self._model.raw[-1]))
            if self._skipped:
                self._stroke.points.append(self._skipped)
            self.window.add_mark(self._stroke)
            self._stroke = self._model = self._skipped = None

    def _is_eraser(self, gesture):
        """Side button held, or the eraser end of a pen that has one."""
        tool = gesture.get_device_tool()
        state = gesture.get_current_event_state()
        if not self._logged_tool:
            self._logged_tool = True
            log(f"pen: tool {tool.get_tool_type().value_nick if tool else None}, buttons {int(state):#x}")
        if tool is not None and tool.get_tool_type() == Gdk.DeviceToolType.ERASER:
            return True
        return bool(state & ERASE_BUTTONS) or self.window.erasing()

    def _erase_to(self, x, y):
        point = self.view.to_page(x, y)
        start = self._eraser or point
        # The first removal opens an undo step; the rest of the stroke joins it.
        if self.window.erase(start, point, ERASER_RADIUS / self.view.zoom, new_step=not self._erased):
            self._erased = True
        self._eraser = point
        self.queue_draw()

    # GTK says when and where the pen touches the page; while the window
    # fills the tablet, the points come from the tablet's own reports (all of
    # them, see pen.Feed), otherwise from GTK's events.
    def _tablet_mapping(self):
        """(x scale, y scale, x offset, y offset) from tablet samples to
        widget pixels, or None unless this window fills the tablet."""
        native = self.get_native()
        surface = native.get_surface() if native else None
        if surface is None or not self.window.is_fullscreen():
            return None
        monitor = self.get_display().get_monitor_at_surface(surface)
        if monitor is None or not (monitor.get_connector() or "").startswith("Meta-"):
            return None
        geometry = monitor.get_geometry()
        tx, ty = native.get_surface_transform()
        ok, origin = native.compute_point(self, Graphene.Point().init(0, 0))
        return (geometry.width, geometry.height, origin.x - tx, origin.y - ty) if ok else None

    def _from_tablet(self, sample):
        sx, sy, ox, oy = self._raw_map
        x, y, pressure = sample
        return x * sx + ox, y * sy + oy, pressure

    def _raw_begin(self, x, y):
        """Widget samples of the stroke GTK reports starting at x, y, or None."""
        self._raw_map = self._tablet_mapping()
        samples = self.raw.begin() if self._raw_map else None
        if not samples:
            return None
        samples = [self._from_tablet(s) for s in samples]
        if math.dist(samples[0][:2], (x, y)) > RAW_TOLERANCE:
            self.raw.drawing = False
            if not self._logged_mismatch:
                self._logged_mismatch = True
                log(f"pen: tablet samples land at {samples[0][:2]}, GTK at {(x, y)}: using GTK's")
            return None
        return samples

    def _raw_more(self, *_args):
        for sample in self.raw.more():
            self._extend(*self._from_tablet(sample))
            self.raw_counts[0] += 1
        return GLib.SOURCE_CONTINUE

    def _raw_stop(self):
        if self._raw_tick is not None:
            self._raw_more()
            self.remove_tick_callback(self._raw_tick)
            self._raw_tick = None
            self.raw.drawing = False

    def _pen_down(self, gesture, x, y):
        if self._is_eraser(gesture):
            self._erased = False
            self._erase_to(x, y)
            return
        samples = self._raw_begin(x, y)
        if samples is None:
            self._begin(x, y, self._pressure(gesture))
            return
        self._begin(*samples[0])
        if self._stroke is None:  # the tap placed a signature, date or text
            self.raw.drawing = False
            return
        for sample in samples[1:]:
            self._extend(*sample)
        self.raw_counts[0] += len(samples)
        self._raw_tick = self.add_tick_callback(self._raw_more)

    def _pen_motion(self, gesture, x, y):
        if self._eraser is not None:
            for sx, sy, _pressure in self._backlog(gesture):
                self._erase_to(sx, sy)
            self._erase_to(x, y)
            return
        if self._raw_tick is not None:
            self.raw_counts[1] += 1 + len(self._backlog(gesture))
            self._raw_more()
            return
        for sample in self._backlog(gesture):
            self._extend(*sample)
        self._extend(x, y, self._pressure(gesture))

    def _pen_up(self, gesture, x, y):
        if self._eraser is not None:
            self._erase_to(x, y)
            self._eraser = None
            self.queue_draw()
            return
        if self._raw_tick is not None:
            self._raw_stop()
        else:
            self._extend(x, y, self._pressure(gesture))
        self._finish()

    def _pen_cancel(self):
        self._raw_stop()
        self._eraser = None
        self._finish()

    def _mouse_begin(self, gesture, x, y):
        # The stylus gesture handles the pen, which GTK also reports as a
        # drag, from a device that says it is a mouse ("Wayland Tablet
        # Pointer"): only the event's tool gives it away. Its late positions
        # mixed into a pen stroke drew needles and bumps. Fingers never draw.
        event = gesture.get_current_event()
        device = gesture.get_device()
        pen = event is not None and event.get_device_tool() is not None
        if pen or (self._stroke is not None and not self._by_mouse) or (
                device is not None and device.get_source() in PEN_SOURCES):
            if pen and not self._logged_drag:
                self._logged_drag = True
                log("pen: also reported as a pointer drag, ignored")
            gesture.set_state(Gtk.EventSequenceState.DENIED)
            return
        self._mouse_origin = (x, y)
        if self.window.erasing():
            self._erased = False
            self._erase_to(x, y)
        else:
            self._begin(x, y, NO_PRESSURE, time.monotonic())
            self._by_mouse = self._stroke is not None

    # A mouse stroke runs on the clock, a pen stroke counts reports: the two
    # never feed the same stroke.
    def _mouse_update(self, gesture, dx, dy):
        x, y = self._mouse_origin
        if self._eraser is not None:
            self._erase_to(x + dx, y + dy)
        elif self._by_mouse:
            self._extend(x + dx, y + dy, NO_PRESSURE, time.monotonic())

    def _mouse_end(self, gesture, dx, dy):
        self._eraser = None
        if self._by_mouse:
            self._by_mouse = False
            self._finish()
        self.queue_draw()

    # -- drawing ---------------------------------------------------------------
    def _page_surface(self, scale):
        """The page with its finished marks, rendered at `scale` device
        pixels per point. Cached: new marks are drawn onto it as they come,
        so a frame does not redraw every stroke on the page (thousands of
        segments); erasing, undo, another page or zoom render it again."""
        index = self.window.index
        marks = self.window.marks.get(index, [])
        cached = self._cache
        if (cached and cached[0] == index and abs(cached[1] / scale - 1) < 0.15
                and len(cached[3]) <= len(marks) and all(a is b for a, b in zip(cached[3], marks))):
            _index, scale, surface, drawn = cached
        else:
            w, h = self.page_size()
            surface = cairo.ImageSurface(cairo.FORMAT_RGB24, max(1, int(w * scale)), max(1, int(h * scale)))
            cr = cairo.Context(surface)
            cr.set_source_rgb(1, 1, 1)
            cr.paint()
            cr.scale(scale, scale)
            self.window.page().render(cr)
            drawn = ()
        if len(drawn) < len(marks):
            cr = cairo.Context(surface)
            cr.scale(scale, scale)
            draw_marks(cr, marks[len(drawn):])
            drawn = tuple(marks)
        self._cache = (index, scale, surface, drawn)
        return scale, surface

    def _draw(self, area, cr, width, height):
        if not self.fitted:
            self.fit()
        v = self.view
        w, h = self.page_size()
        # The page floats over the background on a soft shadow: stacked
        # rounded layers, darkest at the page's edge, fading out in 14 px.
        cr.set_source_rgba(0, 0, 0, 0.012)
        for spread in range(1, 15):
            x, y = v.ox - spread, v.oy - spread + 3
            x1, y1 = v.ox + w * v.zoom + spread, v.oy + h * v.zoom + spread + 3
            cr.new_sub_path()
            cr.arc(x1 - spread, y + spread, spread, -math.pi / 2, 0)
            cr.arc(x1 - spread, y1 - spread, spread, 0, math.pi / 2)
            cr.arc(x + spread, y1 - spread, spread, math.pi / 2, math.pi)
            cr.arc(x + spread, y + spread, spread, math.pi, 3 * math.pi / 2)
            cr.close_path()
            cr.fill()
        cr.save()
        cr.translate(v.ox, v.oy)
        scale, surface = self._page_surface(v.zoom * self.get_scale_factor())
        cr.save()
        cr.scale(v.zoom / scale, v.zoom / scale)
        cr.set_source_surface(surface, 0, 0)
        cr.paint()
        cr.restore()
        cr.scale(v.zoom, v.zoom)
        cr.rectangle(0, 0, w, h)
        cr.clip()
        if self._stroke is not None:
            # Drawn up to where the pen is predicted to be by now.
            tail = [(*v.to_page(x, y), p) for x, y, p in self._model.predict()]
            draw_marks(cr, [Stroke(self._stroke.points + tail, self._stroke.width, self._stroke.color)])
        if self._eraser is not None:
            cr.arc(*self._eraser, ERASER_RADIUS / v.zoom, 0, 2 * math.pi)
            cr.set_source_rgba(1, 1, 1, 0.6)
            cr.fill_preserve()
            cr.set_source_rgba(0.3, 0.3, 0.3, 0.9)
            cr.set_line_width(1.5 / v.zoom)
            cr.stroke()
        if self._hover and self.window.pending:
            # A faint preview of what the tap will place.
            cr.push_group()
            draw_marks(cr, self.window.pending(*v.to_page(*self._hover)))
            cr.pop_group_to_source()
            cr.paint_with_alpha(0.4)
        cr.restore()


class SignWindow(Adw.ApplicationWindow):
    _css_loaded = False

    def __init__(self, app, path, on_done):
        """on_done(signed path or None) runs once, when the window closes."""
        super().__init__(application=app, title=os.path.basename(path),
                         default_width=1000, default_height=820)
        if not SignWindow._css_loaded:
            load_css()
            SignWindow._css_loaded = True
        self.path = path
        self.on_done = on_done
        self.doc = open_document(path)
        self.index = 0
        self.marks = {}       # page index -> [Stroke | Text]
        self.history = []     # (page index, its marks before a change), for undo
        self.pending = None   # marks waiting for a tap to be placed
        self.result = None
        self.signature = load_signature(SIGNATURE)
        self.pen_color, self.pen_width, self.pen_smoothing = load_pen(PEN)

        header = Adw.HeaderBar(show_end_title_buttons=False)
        cancel = Gtk.Button(label=_("Cancel"))
        cancel.connect("clicked", lambda b: self._cancel())
        header.pack_start(cancel)
        self.done = Gtk.Button(label=_("Done"), css_classes=["suggested-action"])
        self.done.connect("clicked", lambda b: self._save())
        header.pack_end(self.done)
        self.title = Adw.WindowTitle(title=os.path.basename(path),
                                     subtitle=_("Write with the pen · hold its button to erase"))
        header.set_title_widget(self.title)

        self.area = PageArea(self)
        overlay = Gtk.Overlay(child=self.area)
        overlay.add_overlay(self._tools())
        overlay.add_overlay(self._pages())
        overlay.add_overlay(self._hint())
        overlay.add_overlay(self._keep_chip())
        self.toasts = Adw.ToastOverlay(child=overlay)

        view = Adw.ToolbarView(content=self.toasts)
        view.add_top_bar(header)
        self.set_content(view)

        keys = Gtk.ShortcutController()
        keys.add_shortcut(Gtk.Shortcut.new(Gtk.ShortcutTrigger.parse_string("<Control>z"),
                                           Gtk.CallbackAction.new(lambda *a: self._undo() or True)))
        keys.add_shortcut(Gtk.Shortcut.new(Gtk.ShortcutTrigger.parse_string("Escape"),
                                           Gtk.CallbackAction.new(lambda *a: self._escape() or True)))
        self.add_controller(keys)
        self.connect("close-request", self._on_close)
        # Should the window ever freeze, the journal gets where it is stuck.
        self._watching = True
        GLib.timeout_add(500, self._alive)
        self.show_page(0)

    # -- toolbar ---------------------------------------------------------------
    def _tools(self):
        """The floating toolbar under the page."""
        bar = Gtk.Box(spacing=2, css_classes=["floating-bar"], halign=Gtk.Align.CENTER,
                      valign=Gtk.Align.END, margin_bottom=18)
        bar.append(self._pen_button())
        eraser_icon = Gtk.DrawingArea(content_width=20, content_height=20)
        eraser_icon.set_draw_func(self._draw_eraser_icon)
        self.eraser = Gtk.ToggleButton(child=eraser_icon, tooltip_text=_("Eraser"), css_classes=["flat"])
        bar.append(self.eraser)
        bar.append(Gtk.Separator(orientation=Gtk.Orientation.VERTICAL))
        self.undo = self._tool(bar, "edit-undo-symbolic", _("Undo"), self._undo)
        self._tool(bar, "user-trash-symbolic", _("Clear page"), self._clear_page)
        bar.append(Gtk.Separator(orientation=Gtk.Orientation.VERTICAL))
        self.signature_thumb = Gtk.DrawingArea(content_width=58, content_height=28)
        self.signature_thumb.set_draw_func(self._draw_signature_thumb)
        self.sign_button = Gtk.Button(child=self.signature_thumb, tooltip_text=_("My signature"),
                                      css_classes=["flat"])
        self.sign_button.connect("clicked", lambda b: self._place_signature())
        bar.append(self.sign_button)
        self._tool(bar, "x-office-calendar-symbolic", _("Date"), self._place_date)
        self._tool(bar, "insert-text-symbolic", _("Text"), self._ask_text)
        bar.append(Gtk.Separator(orientation=Gtk.Orientation.VERTICAL))
        self._tool(bar, "zoom-fit-best-symbolic", _("Fit"), self.area.fit)
        return bar

    def _tool(self, bar, icon, tooltip, callback):
        button = Gtk.Button(icon_name=icon, tooltip_text=tooltip, css_classes=["flat"])
        button.connect("clicked", lambda b: callback())
        bar.append(button)
        return button

    def _pages(self):
        """Page arrows and number, bottom left; only for several pages."""
        self.pages = Gtk.Box(css_classes=["floating-bar"], halign=Gtk.Align.START, valign=Gtk.Align.END,
                             margin_start=18, margin_bottom=18, visible=self.doc.get_n_pages() > 1)
        self.prev = self._tool(self.pages, "go-previous-symbolic", _("Previous page"),
                               lambda: self.show_page(self.index - 1))
        self.page_label = Gtk.Label(css_classes=["page-number"])
        self.pages.append(self.page_label)
        self.next = self._tool(self.pages, "go-next-symbolic", _("Next page"),
                               lambda: self.show_page(self.index + 1))
        return self.pages

    def _hint(self):
        """What a tap will do, while a signature, date or text waits to be placed."""
        self.hint_label = Gtk.Label()
        close = Gtk.Button(icon_name="window-close-symbolic", tooltip_text=_("Cancel"), css_classes=["flat"])
        close.connect("clicked", lambda b: self._disarm())
        box = Gtk.Box(spacing=10, css_classes=["hint-bar"])
        box.append(self.hint_label)
        box.append(close)
        self.hint = Gtk.Revealer(child=box, halign=Gtk.Align.CENTER, valign=Gtk.Align.START, margin_top=14,
                                 transition_type=Gtk.RevealerTransitionType.SLIDE_DOWN)
        return self.hint

    def _keep_chip(self):
        """Offered once something is drawn and no signature is saved yet."""
        button = Gtk.Button(child=Adw.ButtonContent(icon_name="document-save-symbolic",
                                                    label=_("Keep as my signature")), css_classes=["flat"])
        button.connect("clicked", lambda b: self._keep_signature())
        self.keep_button = Gtk.Box(css_classes=["floating-bar"], halign=Gtk.Align.CENTER, valign=Gtk.Align.END,
                                   margin_bottom=18 + 52 + 12, visible=False)
        self.keep_button.append(button)
        return self.keep_button

    def _pen_button(self):
        """Shows the pen's colour and width; its popover changes them."""
        self.pen_swatch = Gtk.DrawingArea(content_width=24, content_height=24)
        self.pen_swatch.set_draw_func(lambda a, cr, w, h: self._dot(
            cr, w, h, self.pen_color, 6 + 14 * (self.pen_width - MIN_WIDTH) / (MAX_WIDTH - MIN_WIDTH)))
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10,
                      margin_top=8, margin_bottom=8, margin_start=8, margin_end=8)
        colors = Gtk.Box(spacing=4, halign=Gtk.Align.CENTER)
        first = None
        for name, color in ((_("Blue ink"), INK), (_("Black"), (0.08, 0.08, 0.1)),
                            (_("Blue"), (0.1, 0.3, 0.85)), (_("Red"), (0.8, 0.1, 0.1)),
                            (_("Green"), (0.1, 0.5, 0.2))):
            dot = Gtk.DrawingArea(content_width=38, content_height=38)
            button = Gtk.ToggleButton(child=dot, tooltip_text=name, css_classes=["swatch"],
                                      active=color == self.pen_color)
            dot.set_draw_func(lambda a, cr, w, h, c=color, b=button: self._draw_swatch(cr, w, h, c, b.get_active()))
            if first:
                button.set_group(first)
            first = first or button
            button.connect("toggled", lambda b, c=color, d=dot: self._swatch_toggled(b, c, d))
            colors.append(button)
        box.append(colors)
        box.append(Gtk.Label(label=_("Thickness"), xalign=0, css_classes=["caption-heading", "dim-label"]))
        scale = Gtk.Scale.new_with_range(Gtk.Orientation.HORIZONTAL, MIN_WIDTH, MAX_WIDTH, 0.1)
        scale.set_value(self.pen_width)
        scale.connect("value-changed", lambda s: self._set_pen(width=s.get_value()))
        box.append(scale)
        box.append(Gtk.Label(label=_("Stabilization"), xalign=0, css_classes=["caption-heading", "dim-label"]))
        steady = Gtk.Scale.new_with_range(Gtk.Orientation.HORIZONTAL, 0, LEVELS, 1)
        steady.set_value(self.pen_smoothing)
        steady.connect("value-changed", lambda s: self._set_pen(smoothing=round(s.get_value())))
        box.append(steady)
        self.pen_preview = Gtk.DrawingArea(content_width=240, content_height=56)
        self.pen_preview.set_draw_func(self._draw_preview)
        frame = Gtk.Frame(child=self.pen_preview, css_classes=["view"])
        box.append(frame)
        popover = Gtk.Popover(child=box)
        # Picking the pen means drawing again.
        popover.connect("show", lambda p: self.eraser.set_active(False))
        return Gtk.MenuButton(child=self.pen_swatch, popover=popover, tooltip_text=_("Pen"),
                              css_classes=["flat"])

    def _swatch_toggled(self, button, color, dot):
        dot.queue_draw()
        if button.get_active():
            self._set_pen(color=color)

    @staticmethod
    def _dot(cr, width, height, color, size):
        cr.set_source_rgb(*color)
        cr.arc(width / 2, height / 2, size / 2, 0, 2 * math.pi)
        cr.fill()

    def _draw_swatch(self, cr, width, height, color, chosen):
        self._dot(cr, width, height, color, 24)
        if chosen:  # a ring around the chosen ink
            cr.set_line_width(2.5)
            cr.arc(width / 2, height / 2, 16, 0, 2 * math.pi)
            cr.stroke()

    def _draw_preview(self, area, cr, width, height):
        # A short wave written lightly, then firmly, then lightly again.
        cr.set_source_rgb(1, 1, 1)
        cr.paint()
        cr.scale(1.5, 1.5)
        points = [(10 + i * 1.3, height / 3 + 7 * math.sin(i / 9), 0.05 + 0.55 * math.sin(math.pi * i / 100))
                  for i in range(101)]
        draw_marks(cr, [Stroke(points, width=self.pen_width, color=self.pen_color)])

    def _draw_eraser_icon(self, area, cr, width, height):
        color = area.get_color()
        cr.set_source_rgba(color.red, color.green, color.blue, color.alpha)
        cr.translate(width / 2, height / 2)
        cr.rotate(math.pi / 4)
        # An eraser tilted 45 degrees: solid body, outlined rubber end.
        cr.rectangle(-4, -8, 8, 9)
        cr.fill()
        cr.set_line_width(1.6)
        cr.rectangle(-3.2, 1.8, 6.4, 5.4)
        cr.stroke()

    def _draw_signature_thumb(self, area, cr, width, height):
        if not self.signature:
            # A dim scribble: no signature saved yet.
            color = area.get_color()
            cr.set_source_rgba(color.red, color.green, color.blue, 0.35)
            cr.set_line_width(1.6)
            cr.move_to(6, height * 0.65)
            for i in range(1, 47):
                cr.line_to(6 + i, height * 0.65 - 6 * math.sin(i / 3.2) * (1 - i / 60))
            cr.stroke()
            return
        strokes = self.signature["strokes"]
        tall = max((pt[1] for s in strokes for pt in s), default=0) or 0.01
        scale = min(width - 6, (height - 6) / tall)
        ox, oy = (width - scale) / 2, (height - tall * scale) / 2
        draw_marks(cr, [Stroke([(ox + x * scale, oy + y * scale, p) for x, y, p in s], width=2.2,
                               color=self.pen_color) for s in strokes])

    def _set_pen(self, color=None, width=None, smoothing=None):
        self.pen_color = color or self.pen_color
        self.pen_width = width or self.pen_width
        self.pen_smoothing = self.pen_smoothing if smoothing is None else smoothing
        save_pen(PEN, self.pen_color, self.pen_width, self.pen_smoothing)
        self.pen_swatch.queue_draw()
        self.pen_preview.queue_draw()
        self.signature_thumb.queue_draw()

    def erasing(self):
        return self.eraser.get_active()

    def toast(self, text, timeout=4):
        self.toasts.add_toast(Adw.Toast(title=text, timeout=timeout))

    # -- pages and marks -------------------------------------------------------
    def page(self):
        return self.doc.get_page(self.index)

    def show_page(self, index):
        n = self.doc.get_n_pages()
        self.index = max(0, min(n - 1, index))
        self.page_label.set_label(f"{self.index + 1} / {n}")
        self.prev.set_sensitive(self.index > 0)
        self.next.set_sensitive(self.index < n - 1)
        self.area.fitted = False
        self._update()

    def add_mark(self, *marks):
        """Add marks to this page, undone in one step."""
        current = self.marks.setdefault(self.index, [])
        self.history.append((self.index, list(current)))
        current.extend(marks)
        self._update()

    def erase(self, a, b, radius, new_step):
        """Remove the marks an eraser going from a to b touches; True if
        any. new_step: undo restores them separately from earlier erasing."""
        current = self.marks.get(self.index, [])
        keep = [m for m in current if not touches(m, a, b, radius)]
        if len(keep) == len(current):
            return False
        if new_step:
            self.history.append((self.index, current))
        self.marks[self.index] = keep
        self._update()
        return True

    def _undo(self):
        if self.history:
            index, marks = self.history.pop()
            self.marks[index] = marks
            if index != self.index:
                self.show_page(index)
        self._update()

    def _clear_page(self):
        marks = self.marks.pop(self.index, [])
        if marks:
            self.history.append((self.index, marks))
        self._update()

    def _update(self):
        strokes = [m for m in self.marks.get(self.index, []) if isinstance(m, Stroke)]
        self.undo.set_sensitive(bool(self.history))
        self.sign_button.set_sensitive(self.signature is not None)
        self.keep_button.set_visible(self.signature is None and bool(strokes))
        self.done.set_sensitive(any(self.marks.values()))
        self.area.queue_draw()

    # -- placing ---------------------------------------------------------------
    def _arm(self, make_marks, message):
        """Next tap (pen, finger or mouse) places make_marks(x, y)."""
        self.pending = make_marks
        self.eraser.set_active(False)
        self.hint_label.set_label(message)
        self.hint.set_reveal_child(True)

    def _disarm(self):
        self.pending = None
        self.hint.set_reveal_child(False)
        self.area.queue_draw()

    def place_at(self, x, y):
        """Called by the page on a tap; True if it placed something."""
        if self.pending is None:
            return False
        self.add_mark(*self.pending(x, y))
        self._disarm()
        return True

    def _escape(self):
        if self.pending:
            self._disarm()
        else:
            self._cancel()

    def _place_signature(self):
        if self.signature:
            self._arm(lambda x, y: place(self.signature, x, y, pen=self.pen_width, color=self.pen_color),
                      _("Tap where your signature goes"))

    def _keep_signature(self):
        strokes = [m for m in self.marks.get(self.index, []) if isinstance(m, Stroke)]
        if strokes:
            save_signature(SIGNATURE, strokes)
            self.signature = load_signature(SIGNATURE)
            self.signature_thumb.queue_draw()
            self._update()
            self.toast(_("Saved: next time, “My signature” places it with one tap"))

    def _place_date(self):
        today = datetime.date.today().strftime("%d/%m/%Y")
        self._arm(lambda x, y: [Text(x, y, today, color=self.pen_color)], _("Tap where the date goes"))

    def _ask_text(self):
        dialog = Adw.MessageDialog(transient_for=self, heading=_("Text to add"))
        entry = Gtk.Entry(activates_default=True)
        dialog.set_extra_child(entry)
        dialog.add_response("cancel", _("Cancel"))
        dialog.add_response("ok", _("Place"))
        dialog.set_default_response("ok")
        dialog.set_response_appearance("ok", Adw.ResponseAppearance.SUGGESTED)

        def answered(d, response):
            text = entry.get_text().strip()
            if response == "ok" and text:
                self._arm(lambda x, y: [Text(x, y, text, color=self.pen_color)], _("Tap where the text goes"))
        dialog.connect("response", answered)
        dialog.present()

    # -- closing ---------------------------------------------------------------
    def _save(self, dst=None):
        self.done.set_sensitive(False)
        self.done.set_child(Gtk.Spinner(spinning=True))
        marks = {i: list(m) for i, m in self.marks.items() if m}

        def work():
            try:
                result = save_signed(self.path, marks, dst)
            except Exception as e:  # noqa: BLE001 - shown to the user
                log("signing: could not save:", repr(e))
                result = e
            GLib.idle_add(once(self._saved, result, dst is None))
        threading.Thread(target=work, daemon=True).start()

    def _saved(self, result, next_to_original):
        if isinstance(result, PermissionError) and next_to_original:
            # A read-only folder, or a Flatpak that may only touch the file it
            # was given: ask where to put the signed copy.
            self._save_as()
            return
        if isinstance(result, Exception):
            self.toast(_("Could not save: {error}").format(error=result), timeout=0)
            self._saving_over()
            return
        self.result = result
        self.close()

    def _saving_over(self):
        self.done.set_label(_("Done"))
        self.done.set_sensitive(True)

    def _save_as(self):
        self.toast(_("Choose where to save the signed copy"))
        name = os.path.basename(output_path(self.path))
        dialog = Gtk.FileDialog(title=_("Save the signed PDF"), initial_name=name)

        def chosen(dialog, result):
            try:
                file = dialog.save_finish(result)
            except GLib.Error:
                file = None  # cancelled: keep the marks, let the user retry
            if file is None or not file.get_path():
                self._saving_over()
                return
            self._save(file.get_path())
        dialog.save(self, None, chosen)

    def _cancel(self):
        if not any(self.marks.values()):
            self.close()
            return
        dialog = Adw.MessageDialog(transient_for=self, heading=_("Discard the signature?"))
        dialog.add_response("keep", _("Keep signing"))
        dialog.add_response("discard", _("Discard"))
        dialog.set_response_appearance("discard", Adw.ResponseAppearance.DESTRUCTIVE)
        dialog.connect("response", lambda d, r: r == "discard" and self._discard())
        dialog.present()

    def _discard(self):
        self.marks.clear()
        self.close()

    def _alive(self):
        """Re-arms the stack dump, which fires only if this stops running."""
        if self._watching:
            faulthandler.dump_traceback_later(STALL)
        return self._watching

    def _on_close(self, _window):
        self._watching = False
        faulthandler.cancel_dump_traceback_later()
        pen_feed.unlisten(self.area.raw.reader)
        drawn, desktop = self.area.raw_counts
        if drawn:
            log(f"pen: drew {drawn} tablet samples; the desktop passed on {desktop} meanwhile")
        callback, self.on_done = self.on_done, None
        if callback:
            callback(self.result)
        return False
