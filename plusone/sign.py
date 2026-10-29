"""Full-screen window for signing a PDF with the pen, shown on the tablet.

The S Pen draws (with pressure) and, with its side button held, erases;
fingers never draw, so a palm resting on the screen leaves no marks. Two
fingers zoom and move the page. A mouse also draws, for signing on the PC.
"""
import collections
import datetime
import faulthandler
import itertools
import math
import os
import sys
import threading
import time

import cairo
import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
gi.require_version("Graphene", "1.0")
gi.require_version("Gsk", "4.0")
gi.require_version("Poppler", "0.18")
from gi.repository import Adw, Gdk, Gio, GLib, Graphene, Gsk, Gtk, Poppler  # noqa: E402

from . import log, once, selection as picking, signatures  # noqa: E402
from .i18n import _  # noqa: E402
from .ink import (ERASER_RADIUS, NO_PRESSURE, RawPen, View, bounds, load_pen, normalize, pen_width, place,  # noqa: E402
                  save_pen, thickness, touches)
from .modeler import LEVELS, Params, StrokeModeler  # noqa: E402
from .pdf import INK, Stroke, Text, curve, draw_marks, draw_segments, output_path, save_signed  # noqa: E402
from .pen import feed as pen_feed  # noqa: E402
from .signature_pad import (ERASE_BUTTONS, SignatureList, SignaturePad, draw_eraser_icon,  # noqa: E402
                            draw_thumb)

from .signing import PEN  # noqa: E402 - shared with the tablet's app
PEN_SOURCES = (Gdk.InputSource.PEN, Gdk.InputSource.TOUCHSCREEN)
RAW_TOLERANCE = 24  # widget pixels between where the tablet and GTK put the tip
TOOLBAR_SPACE = 84  # pixels kept clear under the page for the floating toolbar
# The S Pen reports at a steady rate, but they reach us in bursts over
# Wi-Fi: a stroke's clock counts reports rather than reading arrival times.
REPORT_INTERVAL = 1 / 285
MIN_STEP = 1.0  # widget pixels a modelled point must move to be kept
STALL = 3  # seconds without answering: GNOME is about to call the window frozen
CHUNK = 24  # settled stroke segments turned into one texture while drawing
# Past this the page is magnified rather than rendered bigger: an A4 page at
# the 8x zoom is 32 million pixels, 128 MB plus a copy for the texture.
MAX_PAGE_PIXELS = 16_000_000
TILE = 512  # device pixels per side of a page texture tile
OVERVIEW_PIXELS = 3_000_000
PEN_SAVE_DELAY = 500  # ms after the last change of colour, thickness or stabilization
UNDO_STEPS = 300
# cairo's 32-bit pixels are native-endian words: B, G, R, A bytes here.
LITTLE = sys.byteorder == "little"
OPAQUE = Gdk.MemoryFormat.B8G8R8X8 if LITTLE else Gdk.MemoryFormat.X8R8G8B8
PREMULTIPLIED = (Gdk.MemoryFormat.B8G8R8A8_PREMULTIPLIED if LITTLE
                 else Gdk.MemoryFormat.A8R8G8B8_PREMULTIPLIED)

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


def draw_lasso_icon(area, cr, width, height):
    """Select: a lasso, its loop and a bit of rope."""
    color = area.get_color()
    cr.set_source_rgba(color.red, color.green, color.blue, color.alpha)
    cr.set_line_width(1.6)
    cr.set_line_cap(cairo.LINE_CAP_ROUND)
    cr.save()
    cr.translate(width / 2, height / 2 - 2)
    cr.scale(8, 5)
    cr.arc(0, 0, 1, math.radians(150), math.radians(150 + 340))
    cr.restore()
    cr.stroke()
    cr.move_to(width / 2 - 6, height / 2 + 1)
    cr.curve_to(width / 2 - 8, height / 2 + 4, width / 2 - 6, height / 2 + 7, width / 2 - 3, height / 2 + 8)
    cr.stroke()


def load_css():
    provider = Gtk.CssProvider()
    if hasattr(provider, "load_from_string"):  # GTK 4.12
        provider.load_from_string(CSS)
    else:
        provider.load_from_data(CSS.encode(), -1)
    Gtk.StyleContext.add_provider_for_display(Gdk.Display.get_default(), provider,
                                              Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)


def rgba(spec):
    color = Gdk.RGBA()
    color.parse(spec)
    return color


WHITE = rgba("white")
PAGE_SHADOW = Gsk.Shadow()
PAGE_SHADOW.color = rgba("rgba(0, 0, 0, 0.28)")  # a copy: set it whole
PAGE_SHADOW.dx, PAGE_SHADOW.dy, PAGE_SHADOW.radius = 0, 3, 14


def texture(surface):
    """A GdkTexture with a copy of a cairo image surface's pixels."""
    surface.flush()
    fmt = OPAQUE if surface.get_format() == cairo.FORMAT_RGB24 else PREMULTIPLIED
    return Gdk.MemoryTexture.new(surface.get_width(), surface.get_height(), fmt,
                                 GLib.Bytes.new(bytes(surface.get_data())), surface.get_stride())


def rect(x, y, w, h):
    return Graphene.Rect().init(x, y, w, h)


class PageRaster:
    """A page with its finished marks, rendered at one scale and handed to
    GTK as tiles of textures. Tiles are rendered when first shown, so a
    zoomed-in page costs what is on screen; a change (a stroke added,
    erased, undone) re-renders only its area and makes new textures only
    for the tiles it covers. One texture for the whole page meant copying
    and uploading all of it (64 MB at high zoom) after every stroke and
    every erased one."""

    def __init__(self, index, scale, page, size):
        w, h = size
        # Cairo's surfaces are at most 32767 pixels a side (a long receipt, zoomed).
        scale = min(scale, 32767 / max(w, 1e-6), 32767 / max(h, 1e-6))
        self.index, self.scale, self.page = index, scale, page
        self.surface = cairo.ImageSurface(cairo.FORMAT_RGB24, max(1, int(w * scale)), max(1, int(h * scale)))
        self.drawn = ()        # marks on the surface (where rendered), in order
        self.tiles = {}        # (column, row) -> texture, made when first needed
        self.rendered = set()  # tiles whose pixels are up to date

    def area(self, marks):
        """Device pixel box (x0, y0, x1, y1) covering marks' ink, or None."""
        boxes = [bounds(m) for m in marks]
        if not boxes:
            return None
        s, w, h = self.scale, self.surface.get_width(), self.surface.get_height()
        x0 = max(0, math.floor(min(b[0] for b in boxes) * s))
        y0 = max(0, math.floor(min(b[1] for b in boxes) * s))
        x1 = min(w, math.ceil(max(b[2] for b in boxes) * s))
        y1 = min(h, math.ceil(max(b[3] for b in boxes) * s))
        return (x0, y0, x1, y1) if x0 < x1 and y0 < y1 else None

    def _render(self, area, marks):
        """The page and marks again, within area (all of it if None)."""
        self._render_boxes([area] if area is not None else None, marks)

    def _render_boxes(self, boxes, marks):
        """The page and marks again within boxes (all of it if None), in one
        pass of poppler: what costs is the pass (a scanned page's image is
        decoded whatever the clip), not the area. One pass per tile took 9
        times as long for 9 tiles (1 s on a scanned page, measured)."""
        cr = cairo.Context(self.surface)
        if boxes is not None:
            if not boxes:
                return
            for x0, y0, x1, y1 in boxes:
                cr.rectangle(x0, y0, x1 - x0, y1 - y0)
            cr.clip()
            marks = [m for m in marks if (box := self.area([m])) and any(_overlap(box, b) for b in boxes)]
        cr.set_source_rgb(1, 1, 1)
        cr.paint()
        cr.scale(self.scale, self.scale)
        self.page.render(cr)
        draw_marks(cr, marks)
        for box in boxes if boxes is not None else [None]:
            self.changed(box)

    def render_tiles(self, keys):
        """These tiles, those not rendered yet, in one pass."""
        missing = [key for key in keys if key not in self.rendered]
        if missing:
            self._render_boxes([self._tile_box(*key) for key in missing], self.drawn)
            self.rendered.update(missing)

    def changed(self, area):
        """Pixels within area (all if None) changed: their tiles go."""
        if area is None:
            self.tiles = {}
            return
        for key in [k for k in self.tiles if _overlap(self._tile_box(*k), area)]:
            del self.tiles[key]

    def update(self, marks):
        """Bring the surface to these marks. Only rendered tiles are
        touched: the others are rendered with all marks when first shown."""
        drawn = self.drawn
        if len(drawn) <= len(marks) and all(a is b for a, b in zip(drawn, marks)):
            new = marks[len(drawn):]  # on top of what is there
            for box in self._rendered_boxes(self.area(new)):
                cr = cairo.Context(self.surface)
                cr.rectangle(box[0], box[1], box[2] - box[0], box[3] - box[1])
                cr.clip()
                cr.scale(self.scale, self.scale)
                draw_marks(cr, [m for m in new if _overlap(self.area([m]) or box, box)])
                self.changed(box)
        else:
            # Erased, undone or cleared: render again where something changed.
            before, after = {id(m) for m in drawn}, {id(m) for m in marks}
            area = self.area([m for m in drawn if id(m) not in after]
                             + [m for m in marks if id(m) not in before])
            self._render_boxes(self._rendered_boxes(area), marks)
        self.drawn = tuple(marks)

    def _rendered_boxes(self, area):
        """Parts of area (device pixels) on rendered tiles, a box per tile."""
        if area is None:
            return []
        boxes = []
        for key in self.tile_boxes(area):
            if key in self.rendered:
                t = self._tile_box(*key)
                boxes.append((max(t[0], area[0]), max(t[1], area[1]), min(t[2], area[2]), min(t[3], area[3])))
        return boxes

    def add(self, mark, paint):
        """mark goes on top, drawn by paint(cairo context in device pixels)."""
        paint(cairo.Context(self.surface))
        self.changed(self.area([mark]))
        self.drawn += (mark,)

    def _tile_box(self, column, row):
        w, h = self.surface.get_width(), self.surface.get_height()
        return column * TILE, row * TILE, min(w, (column + 1) * TILE), min(h, (row + 1) * TILE)

    def tile_boxes(self, area=None):
        """Tiles, those overlapping area (device pixels) if given."""
        w, h = self.surface.get_width(), self.surface.get_height()
        x0, y0, x1, y1 = area or (0, 0, w, h)
        return [(c, r) for r in range(max(0, int(y0 // TILE)), min(-(-h // TILE), int(y1 // TILE) + 1))
                for c in range(max(0, int(x0 // TILE)), min(-(-w // TILE), int(x1 // TILE) + 1))]

    def tile(self, column, row):
        """(texture, its device pixel box) for one tile."""
        box = self._tile_box(column, row)
        if (column, row) not in self.rendered:
            self._render(box, self.drawn)
            self.rendered.add((column, row))
        tex = self.tiles.get((column, row))
        if tex is None:
            x0, y0, x1, y1 = box
            self.surface.flush()
            data, stride = self.surface.get_data(), self.surface.get_stride()
            rows = b"".join(bytes(data[y * stride + 4 * x0:y * stride + 4 * x1]) for y in range(y0, y1))
            tex = self.tiles[column, row] = Gdk.MemoryTexture.new(
                x1 - x0, y1 - y0, OPAQUE, GLib.Bytes.new(rows), 4 * (x1 - x0))
        return tex, box


def _overlap(a, b):
    return a[0] < b[2] and b[0] < a[2] and a[1] < b[3] and b[1] < a[3]


def date_text(day):
    """A date as the user's locale writes it (GTK set the locale): 05/10 is
    October in Italy, May in the US. Without one (C), ISO 8601: unambiguous."""
    import locale
    language, _encoding = locale.getlocale(locale.LC_TIME)
    return day.strftime("%x") if language and language not in ("C", "POSIX") else day.isoformat()


def _forget(path):
    try:
        os.unlink(path)
    except OSError:
        pass


def open_document(path):
    """A Poppler document, or raise GLib.Error (missing, damaged, password)."""
    doc = Poppler.Document.new_from_file(Gio.File.new_for_path(path).get_uri(), None)
    if doc.get_n_pages() == 0:
        raise GLib.Error("the PDF has no pages")  # shown as "Cannot open"
    for i in range(doc.get_n_pages()):
        width, height = doc.get_page(i).get_size()
        if not (width > 0 and height > 0):  # a crop box outside its media box, say
            raise GLib.Error(f"page {i + 1} has no size")
    return doc


class Lag:
    """How long pen samples wait before the window draws them, and how
    often it draws while a stroke is on: logged for slow strokes and for
    the whole window, to tell the window's delay from the stream's."""

    SLOW = 0.030  # seconds a sample may wait on average

    def __init__(self):
        self.waits, self.gaps = [], []      # this stroke
        # The window's: the last ten thousand or so.
        self.all_waits, self.all_gaps = collections.deque(maxlen=20000), collections.deque(maxlen=5000)
        self.last_frame = None

    def waited(self, seconds):
        self.waits.append(seconds)

    def frame(self):
        now = time.monotonic()
        if self.last_frame is not None:
            self.gaps.append(now - self.last_frame)
        self.last_frame = now

    def stroke_done(self):
        if self.waits and sum(self.waits) / len(self.waits) > self.SLOW:
            log("pen: slow stroke: " + self._describe(self.waits, self.gaps))
        self.all_waits.extend(self.waits)
        self.all_gaps.extend(self.gaps)
        self.waits, self.gaps, self.last_frame = [], [], None

    def summary(self):
        return self._describe(self.all_waits, self.all_gaps)

    @staticmethod
    def _describe(waits, gaps):
        def ms(values):
            if not values:
                return "-"
            return f"{1000 * sum(values) / len(values):.0f} ms (max {1000 * max(values):.0f})"
        return f"samples waited {ms(waits)}, frames every {ms(gaps)}"


class PageArea(Gtk.Widget):
    """One page with its marks; turns pen/mouse input into page coordinates.

    Drawn as render nodes, not one cairo picture: the page with its finished
    marks is a texture, uploaded once, and so is each run of settled segments
    of the stroke being drawn, so a frame only rasterizes the stroke's last
    few segments and GTK repaints only where they are. Painting the whole
    page every frame took the window about 10 ms a frame, and it missed
    frames while the pen moved."""

    NATURAL = (480, 600)  # or a window that is not maximized shrinks to nothing

    def __init__(self, window):
        super().__init__(hexpand=True, vexpand=True, focusable=True, css_classes=["sign-canvas"])
        self.window = window
        self.view = View()
        self.fitted = False
        self._raster = None  # PageRaster of the page shown
        self._origin = (0.0, 0.0)  # where the page is drawn (whole device pixels)
        self._pinching = False
        self._overview = None   # the whole page at the fitted zoom: stands in for tiles not rendered yet
        self._overview_shown = False  # the last frame's tiles waited behind the overview
        self._fit_scale = 1.0
        self._stroke = None
        self._model = None    # the stroke's modeler, in widget pixels
        self._clock = 0.0     # time of the stroke's last sample, seconds
        self._skipped = None  # last modelled point, if too close to keep (yet)
        self._by_mouse = False  # the stroke being drawn is the mouse's
        self._inked = 0     # its segments in chunks already
        self._chunks = []   # (surface, texture, device pixel x, y) of those, at _chunk_scale
        self._chunk_scale = None
        self.stats = Lag()        # how far behind the pen the window runs
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
        self._grab = None     # what the pen or mouse holds of the selection (see _grab_at)
        self._grabbed = -1    # its handle, for the look
        self._lasso = None    # widget points of the lasso being drawn
        self._stepped = False  # the drag's undo step is taken

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
        tap.connect("released", lambda g, n, x, y: self._tapped(x, y))
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

    def do_measure(self, orientation, for_size):
        size = self.NATURAL[orientation == Gtk.Orientation.VERTICAL]
        return size, size, -1, -1

    def do_size_allocate(self, width, height, baseline):
        self.fit(width, height)

    def fit(self, w=None, h=None):
        w, h = w or self.get_width(), h or self.get_height()
        if w and h:
            self.view.fit(*self.page_size(), w, h, margin=24, bottom=TOOLBAR_SPACE)
            self._fit_scale = self.view.zoom * self.get_scale_factor()
            self.fitted = True
            self.queue_draw()

    def _zoom_begin(self, gesture, _seq):
        self._pinching = True
        ok, x, y = gesture.get_bounding_box_center()
        self._zoom_start = (self.view.zoom, self.view.to_page(x, y))

    def _zoom_changed(self, gesture, scale):
        ok, x, y = gesture.get_bounding_box_center()
        zoom, anchor = self._zoom_start
        # Pinch zooms, moving both fingers pans: the anchor follows them.
        self.view.zoom_at(zoom * scale, x, y, anchor)
        self.queue_draw()

    def _sharpen(self):
        """After a pinch, the page and its marks at exactly the new zoom (a
        two-finger pan kept it: nothing to render again)."""
        self._pinching = False
        if self.view.zoom != self._zoom_start[0]:
            self._raster = None
        self.queue_draw()

    def _scroll(self, controller, dx, dy):
        state = controller.get_current_event_state()
        # A mouse wheel says notches; a touchpad, pixels, many events a gesture.
        wheel = controller.get_unit() == Gdk.ScrollUnit.WHEEL
        if state & Gdk.ModifierType.CONTROL_MASK:
            x, y = self.get_width() / 2, self.get_height() / 2
            factor = (0.9 if dy > 0 else 1.1) if wheel else math.exp(-dy * 0.01)
            self.view.zoom_at(self.view.zoom * factor, x, y)
        else:
            step = 40 if wheel else 1
            self.view.pan(-dx * step, -dy * step)
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

    # Samples go through a stroke modeler (see modeler.py), as in note apps:
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
            stroke, raster, chunks, inked = self._stroke, self._raster, self._chunks, self._inked
            self.window.add_mark(stroke)
            marks = self.window.marks.get(self.window.index, [])
            if (chunks and raster is not None and raster.scale == self._chunk_scale
                    and raster.index == self.window.index and len(marks) == len(raster.drawn) + 1
                    and marks[-1] is stroke and all(a is b for a, b in zip(raster.drawn, marks))):
                # What was shown goes onto the page as is: the chunks, then
                # the rest of the stroke, rather than all of it again.
                def paint(cr):
                    for surface, _texture, x, y in chunks:
                        cr.set_source_surface(surface, x, y)
                        cr.paint()
                    cr.scale(raster.scale, raster.scale)
                    draw_segments(cr, stroke, self._segments(stroke.points, inked))
                raster.add(stroke, paint)
            self.stats.stroke_done()
            self._stroke = self._model = self._skipped = None
            self._chunks, self._inked = [], 0

    @staticmethod
    def _segments(points, first=0, upto=None):
        """curve() segments of points from the first-th, up to (not
        including) upto."""
        start = max(0, first - 1)  # the previous point shapes the first one
        return itertools.islice(curve(points[start:]), first - start, None if upto is None else upto - start)

    def _ink(self, scale):
        """Turn the stroke's settled segments (all four points that shape
        each are in) into textures, CHUNK at a time: a frame then rasterizes
        only the last few, however long the stroke."""
        if self._chunk_scale != scale:
            self._chunks, self._inked, self._chunk_scale = [], 0, scale
        settled = len(self._stroke.points) - 2
        while settled - self._inked >= CHUNK:
            segments = list(self._segments(self._stroke.points, self._inked, self._inked + CHUNK))
            # The curve stays within its control points; the ink, half a width around it.
            xs = [p[0] for seg in segments for p in seg[:4]]
            ys = [p[1] for seg in segments for p in seg[:4]]
            margin = self._stroke.width / 2 * scale + 2
            x0, y0 = math.floor(min(xs) * scale - margin), math.floor(min(ys) * scale - margin)
            x1, y1 = math.ceil(max(xs) * scale + margin), math.ceil(max(ys) * scale + margin)
            surface = cairo.ImageSurface(cairo.FORMAT_ARGB32, x1 - x0, y1 - y0)
            cr = cairo.Context(surface)
            cr.translate(-x0, -y0)
            cr.scale(scale, scale)
            draw_segments(cr, self._stroke, segments)
            self._chunks.append((surface, texture(surface), x0, y0))
            self._inked += CHUNK

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
        samples = self.raw.more()
        for sample in samples:
            self._extend(*self._from_tablet(sample))
            self.raw_counts[0] += 1
        if samples:
            self.stats.waited(time.monotonic() - self.raw.latest)
        return GLib.SOURCE_CONTINUE

    def _raw_stop(self):
        if self._raw_tick is not None:
            self._raw_more()
            self.remove_tick_callback(self._raw_tick)
            self._raw_tick = None
            self.raw.drawing = False

    def _pen_down(self, gesture, x, y):
        erasing = self._is_eraser(gesture)
        if self._select_down(x, y, erasing):
            return
        if erasing:
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
        if self._select_move(x, y):
            return
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
        if self._select_up():
            return
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
        self._select_up(cancelled=True)
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
        if self._select_down(x, y, self.window.erasing()):
            return
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
        if self._select_move(x + dx, y + dy):
            return
        if self._eraser is not None:
            self._erase_to(x + dx, y + dy)
        elif self._by_mouse:
            self._extend(x + dx, y + dy, NO_PRESSURE, time.monotonic())

    def _mouse_end(self, gesture, dx, dy):
        if self._select_up():
            return
        self._eraser = None
        if self._by_mouse:
            self._by_mouse = False
            self._finish()
        self.queue_draw()

    # -- selection -------------------------------------------------------------
    def _visible(self):
        """Where a handle can be touched: the widget less the toolbar."""
        return 0, 0, self.get_width(), self.get_height() - TOOLBAR_SPACE

    def _shape(self, chosen):
        return picking.shape([self.view.to_widget(*p) for p in chosen.corners()], self._visible())

    def _select_down(self, x, y, erasing):
        """Pen or mouse down: the selection's handles and inside take it,
        elsewhere the selection ends and the lasso, if chosen, starts. True
        if the selection took it."""
        window, chosen = self.window, self.window.selection
        if window.pending is not None:
            return False  # a tap places the signature, date or text first
        if chosen is not None and chosen.page == window.index and not erasing:
            quad, turn = self._shape(chosen)
            part = picking.hit(quad, turn, x, y)
            if part >= 0:
                point = self.view.to_page(x, y)
                corners = chosen.corners()
                if part == 5:
                    self._grab = ("move", point)
                elif part == 4:
                    centre = ((corners[0][0] + corners[2][0]) / 2, (corners[0][1] + corners[2][1]) / 2)
                    start = math.degrees(math.atan2(point[1] - centre[1], point[0] - centre[0]))
                    self._grab = ("turn", centre, start, chosen.matrix, chosen.angle)
                else:
                    # From where the pointer took the handle (drawn on a box grown
                    # for small marks): the size it had there, no jump.
                    self._grab = ("scale", corners[(part + 2) % 4], point, chosen.matrix)
                self._grabbed = -1 if part == 5 else part
                self._stepped = False
                self.queue_draw()
                return True
        window.deselect()
        if window.select_mode and not erasing:
            self._lasso = [(x, y)]
            self.queue_draw()
            return True
        return False

    def _select_move(self, x, y):
        chosen = self.window.selection
        if self._grab is not None and chosen is not None:
            if not self._stepped:  # one undo step per drag, once it moves
                self.window.step(chosen)
                self._stepped = True
            point = self.view.to_page(x, y)
            kind = self._grab[0]
            if kind == "move":
                last = self._grab[1]
                chosen.matrix = picking.multiply(chosen.matrix, picking.translation(point[0] - last[0],
                                                                                     point[1] - last[1]))
                self._grab = ("move", point)
            elif kind == "scale":
                _, anchor, start, matrix = self._grab
                k = picking.scale_along(anchor, start, point)
                chosen.matrix = picking.multiply(matrix, picking.scaling(k, *anchor))
            else:
                _, centre, start, matrix, angle = self._grab
                now = math.degrees(math.atan2(point[1] - centre[1], point[0] - centre[0]))
                target = picking.snap_angle(angle + now - start)
                chosen.matrix = picking.multiply(matrix, picking.rotation(target - angle, *centre))
            self.queue_draw()
            return True
        if self._lasso is not None:
            self._lasso.append((x, y))
            self.queue_draw()
            return True
        return False

    def _select_up(self, cancelled=False):
        if self._grab is not None:
            self._grab, self._grabbed = None, -1
            self.queue_draw()
            return True
        if self._lasso is not None:
            points, self._lasso = self._lasso, None
            if not cancelled:
                self._pick(points)
            self.queue_draw()
            return True
        return False

    def _pick(self, points):
        """The lasso closes: the marks its area touches; a tap: the mark under it."""
        window = self.window
        marks = window.marks.get(window.index, [])
        xs, ys = [p[0] for p in points], [p[1] for p in points]
        if max(xs) - min(xs) < picking.TAP and max(ys) - min(ys) < picking.TAP:
            point = self.view.to_page(*points[0])
            reach = picking.TAP / self.view.zoom
            picked = [m for m in marks if touches(m, point, point, reach)][-1:]
        else:
            polygon = [self.view.to_page(x, y) for x, y in points]
            picked = [m for m in marks if picking.picks(m, polygon)]
        window.select(picked)

    def _tapped(self, x, y):
        """A finger's tap: places what waits to be placed, or ends the selection."""
        if not self.window.place_at(*self.view.to_page(x, y)):
            chosen = self.window.selection
            if chosen is not None and picking.hit(*self._shape(chosen), x, y) < 0:
                self.window.deselect()

    # -- drawing ---------------------------------------------------------------
    def _page_raster(self, scale):
        """The page with its finished marks at about `scale` device pixels
        per point, kept up to date (see PageRaster); another page or a
        different zoom renders it again."""
        index, raster = self.window.index, self._raster
        # While fingers pinch, the page is only magnified: it is rendered
        # again at the new zoom when they let go (see _sharpen).
        if raster is None or raster.index != index or (
                abs(raster.scale / scale - 1) >= 0.15 and not self._pinching):
            raster = self._raster = PageRaster(index, scale, self.window.page(), self.page_size())
        raster.update(self.window.marks.get(index, []))
        return raster

    def _overview_raster(self):
        """The whole page, small (at the fitted zoom): rendered at once, it
        shows wherever the page at the current zoom has tiles to render."""
        index, overview = self.window.index, self._overview
        scale = min(self._fit_scale, math.sqrt(OVERVIEW_PIXELS / (self.page_size()[0] * self.page_size()[1])))
        if overview is None or overview.index != index or abs(overview.scale / scale - 1) >= 0.15:
            overview = self._overview = PageRaster(index, scale, self.window.page(), self.page_size())
        overview.update(self.window.marks.get(index, []))
        overview.render_tiles(overview.tile_boxes())  # all of it, in one pass
        return overview

    def do_snapshot(self, snapshot):
        if not self.fitted:
            self.fit()
        v = self.view
        w, h = self.page_size()
        # The page on whole device pixels, so its tiles meet without seams.
        sf = self.get_scale_factor()
        ox, oy = self._origin = round(v.ox * sf) / sf, round(v.oy * sf) / sf
        page_rect = rect(ox, oy, w * v.zoom, h * v.zoom)
        # The page floats over the background on a soft shadow.
        # (An outset shadow node left stale pixels behind when the page moved:
        # GTK 4.14 repainted too little around it.)
        snapshot.push_shadow([PAGE_SHADOW])
        snapshot.append_color(WHITE, page_rect)
        snapshot.pop()
        needed = min(v.zoom * sf, math.sqrt(MAX_PAGE_PIXELS / (w * h)))
        raster = self._page_raster(needed)
        scale = raster.scale
        k = v.zoom / scale  # widget pixels per page device pixel
        visible = (-ox / k, -oy / k, (self.get_width() - ox) / k, (self.get_height() - oy) / k)
        # Tiles not rendered yet (after a zoom, or coming into view): the
        # whole page drawn small shows this frame, and the next renders them
        # all in one pass (one per tile, a few per frame, made a scanned page
        # take a second of frames).
        keys = raster.tile_boxes(visible)
        missing = [key for key in keys if key not in raster.rendered]
        # Fingers zooming out: tiles at the old, bigger zoom would be thrown
        # away when they let go, so the overview alone fills in.
        if missing and self._overview_shown and not (self._pinching and raster.scale > 1.5 * needed):
            raster.render_tiles(missing)
            missing = []
        self._overview_shown = bool(missing)
        waiting = bool(missing)
        ready = [raster.tile(*key) for key in keys if key in raster.rendered]
        if waiting:
            # Brought to the marks only now: at each frame of an eraser or an
            # undo it was one more pass of poppler, for nothing shown.
            overview = self._overview_raster()
            ko = v.zoom / overview.scale
            for key in overview.tile_boxes():
                tile, (x0, y0, x1, y1) = overview.tile(*key)
                snapshot.append_texture(tile, rect(ox + x0 * ko, oy + y0 * ko, (x1 - x0) * ko, (y1 - y0) * ko))
            if not self._pinching:  # (a pinch draws anyway)
                GLib.idle_add(once(self.queue_draw))  # more next frame, not from inside this one
        for tile, (x0, y0, x1, y1) in ready:
            snapshot.append_texture(tile, rect(ox + x0 * k, oy + y0 * k, (x1 - x0) * k, (y1 - y0) * k))
        snapshot.push_clip(page_rect)
        if self._stroke is not None:
            self._ink(scale)
            self.stats.frame()
            for surface, chunk, x, y in self._chunks:
                snapshot.append_texture(chunk, rect(ox + x * k, oy + y * k,
                                                    surface.get_width() * k, surface.get_height() * k))
            # The rest of the stroke, up to where the pen is predicted to be by now.
            tail = [(*v.to_page(x, y), p) for x, y, p in self._model.predict()]
            base = max(0, self._inked - 1)  # what the segments not in chunks need
            points = self._stroke.points[base:] + tail
            segments = list(self._segments(points, self._inked - base)) if points else []
            if segments:
                # The curve stays within its control points.
                cr = self._cairo(snapshot, [p for seg in segments for p in seg[:4]], self._stroke.width / 2)
                draw_segments(cr, self._stroke, segments)
            elif points:  # a dot so far
                cr = self._cairo(snapshot, points[:1], self._stroke.width / 2)
                draw_marks(cr, [Stroke(points, self._stroke.width, self._stroke.color)])
        if self._eraser is not None:
            radius = ERASER_RADIUS / v.zoom
            cr = self._cairo(snapshot, [self._eraser], radius)
            cr.arc(*self._eraser, radius, 0, 2 * math.pi)
            cr.set_source_rgba(1, 1, 1, 0.6)
            cr.fill_preserve()
            cr.set_source_rgba(0.3, 0.3, 0.3, 0.9)
            cr.set_line_width(1.5 / v.zoom)
            cr.stroke()
        if self._hover and self.window.pending:
            # A faint preview of what the tap will place.
            snapshot.push_opacity(0.4)
            cr = snapshot.append_cairo(page_rect)
            cr.translate(ox, oy)
            cr.scale(v.zoom, v.zoom)
            draw_marks(cr, self.window.pending(*v.to_page(*self._hover)))
            snapshot.pop()
        chosen = self.window.selection
        if chosen is not None and chosen.page == self.window.index:
            # Picked marks are off the page's list (and its tiles): drawn here, as they move.
            marks = chosen.marks()
            reach = max((m.width for m in marks if isinstance(m, Stroke)), default=1)
            cr = self._cairo(snapshot, [p for m in marks for p in (bounds(m)[:2], bounds(m)[2:])], reach)
            draw_marks(cr, marks)
        snapshot.pop()
        whole = rect(0, 0, self.get_width(), self.get_height())
        if chosen is not None and chosen.page == self.window.index:
            quad, turn = self._shape(chosen)
            turned = chosen.angle if self._grab is not None and self._grab[0] == "turn" else None
            picking.draw(snapshot.append_cairo(whole), quad, turn, self._grabbed, turned)
        if self._lasso:
            picking.draw_lasso(snapshot.append_cairo(whole), self._lasso)

    def _cairo(self, snapshot, points, reach):
        """A cairo context in page points for drawing around points (and
        reach points beyond them): only that area is rasterized."""
        v, (ox, oy) = self.view, self._origin
        xs, ys = [p[0] for p in points], [p[1] for p in points]
        pad = reach * v.zoom + 2
        x0, y0 = ox + min(xs) * v.zoom - pad, oy + min(ys) * v.zoom - pad
        cr = snapshot.append_cairo(rect(x0, y0, (max(xs) - min(xs)) * v.zoom + 2 * pad,
                                        (max(ys) - min(ys)) * v.zoom + 2 * pad))
        cr.translate(ox, oy)
        cr.scale(v.zoom, v.zoom)
        return cr


class SignWindow(Adw.ApplicationWindow):
    _css_loaded = False

    def __init__(self, app, path, on_done):
        """on_done(signed path or None) runs once, when the window closes.
        Raises GLib.Error for a PDF it cannot open, before it is a window
        (one made would hold the application)."""
        from .signing import copy_of
        # What is shown is what is signed: a copy of the PDF as it is now,
        # whatever becomes of path meanwhile (moved, deleted, replaced by
        # another document); the signed copy still goes next to path.
        try:
            copy = copy_of(path)
        except OSError as e:
            raise GLib.Error(e.strerror or str(e)) from None
        try:
            doc = open_document(copy)
        except BaseException:
            _forget(copy)
            raise
        super().__init__(application=app, title=os.path.basename(path),
                         default_width=1000, default_height=820)
        if not SignWindow._css_loaded:
            load_css()
            SignWindow._css_loaded = True
        self.path, self.copy = path, copy
        self.on_done = on_done
        self.doc = doc
        self.index = 0
        self.marks = {}       # page index -> [Stroke | Text]
        # (page index, its marks before a change), for undo; the oldest steps
        # go, so a long session does not keep a copy of the page per stroke.
        self.history = collections.deque(maxlen=UNDO_STEPS)
        self.pending = None   # marks waiting for a tap to be placed
        self.selection = None  # picking.Selection: marks picked off their page's list
        self.result = None
        self.signature = self._first_signature()  # what "My signature" places
        self.pen_color, self.pen_width, self.pen_smoothing = load_pen(PEN)
        self._pen_save = None  # a pending write of the pen settings

        header = Adw.HeaderBar(show_end_title_buttons=False)
        self.cancel_button = Gtk.Button(label=_("Cancel"))
        self.cancel_button.connect("clicked", lambda b: self._cancel())
        header.pack_start(self.cancel_button)
        self._saving = False  # the signed copy is being written: no way out meanwhile
        self._may_close = False  # closing would not lose marks (answered, saved, nothing drawn)
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
        overlay.add_overlay(self._selection_bar())
        overlay.add_overlay(self._keep_chip())
        self.toasts = Adw.ToastOverlay(child=overlay)

        view = Adw.ToolbarView(content=self.toasts)
        view.add_top_bar(header)
        self.set_content(view)

        keys = Gtk.ShortcutController()
        keys.add_shortcut(Gtk.Shortcut.new(Gtk.ShortcutTrigger.parse_string("<Control>z"),
                                           Gtk.CallbackAction.new(lambda *a: self._undo() or True)))
        keys.add_shortcut(Gtk.Shortcut.new(Gtk.ShortcutTrigger.parse_string("Delete"),
                                           Gtk.CallbackAction.new(lambda *a: self.delete_selection() or True)))
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
        eraser_icon.set_draw_func(draw_eraser_icon)
        self.eraser = Gtk.ToggleButton(child=eraser_icon, tooltip_text=_("Eraser"), css_classes=["flat"])
        bar.append(self.eraser)
        lasso_icon = Gtk.DrawingArea(content_width=20, content_height=20)
        lasso_icon.set_draw_func(draw_lasso_icon)
        self.lasso = Gtk.ToggleButton(child=lasso_icon, tooltip_text=_("Select"), css_classes=["flat"])
        bar.append(self.lasso)
        # One tool at a time: pen (neither), eraser or lasso.
        self.eraser.connect("toggled", lambda b: b.get_active() and self.lasso.set_active(False))
        self.lasso.connect("toggled", lambda b: b.get_active() and self.eraser.set_active(False))
        bar.append(Gtk.Separator(orientation=Gtk.Orientation.VERTICAL))
        self.undo = self._tool(bar, "edit-undo-symbolic", _("Undo"), self._undo)
        self._tool(bar, "user-trash-symbolic", _("Clear page"), self._clear_page)
        bar.append(Gtk.Separator(orientation=Gtk.Orientation.VERTICAL))
        self.signature_thumb = Gtk.DrawingArea(content_width=58, content_height=28)
        self.signature_thumb.set_draw_func(self._draw_signature_thumb)
        # Its arrow opens the list: pick another, remove one, draw a new one.
        self.signature_list = SignatureList(self.pen_color, self.pen_width, on_pick=self._pick_signature,
                                            on_new=self._new_signature, on_change=self._signatures_changed)
        popover = Gtk.Popover(child=Gtk.ScrolledWindow(child=self.signature_list, propagate_natural_height=True,
                                                       max_content_height=420,
                                                       hscrollbar_policy=Gtk.PolicyType.NEVER))
        popover.connect("show", lambda p: self._show_signatures())
        self.sign_button = Adw.SplitButton(child=self.signature_thumb, tooltip_text=_("My signature"),
                                           dropdown_tooltip=_("My signatures"), popover=popover,
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

    def _selection_bar(self):
        """While marks are picked: what can be done, Delete, and Done."""
        label = Gtk.Label(label=_("Drag it, turn it, or resize from a corner"))
        delete = Gtk.Button(icon_name="user-trash-symbolic", tooltip_text=_("Delete"), css_classes=["flat"])
        delete.connect("clicked", lambda b: self.delete_selection())
        close = Gtk.Button(icon_name="window-close-symbolic", tooltip_text=_("Done with the selection"),
                           css_classes=["flat"])
        close.connect("clicked", lambda b: self.deselect())
        box = Gtk.Box(spacing=6, css_classes=["hint-bar"])
        for widget in (label, delete, close):
            box.append(widget)
        self.selection_bar = Gtk.Revealer(child=box, halign=Gtk.Align.CENTER, valign=Gtk.Align.START,
                                          margin_top=14, transition_type=Gtk.RevealerTransitionType.SLIDE_DOWN)
        return self.selection_bar

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
            cr, w, h, self.pen_color, 6 + 14 * thickness(self.pen_width)))
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
        scale = Gtk.Scale.new_with_range(Gtk.Orientation.HORIZONTAL, 0, 1, 0.01)
        scale.set_value(thickness(self.pen_width))
        scale.connect("value-changed", lambda s: self._set_pen(width=pen_width(s.get_value())))
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
        popover.connect("show", lambda p: (self.eraser.set_active(False), self.lasso.set_active(False)))
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

    def _draw_signature_thumb(self, area, cr, width, height):
        if self.signature:
            draw_thumb(cr, self.signature, width, height, self.pen_color)
            return
        # A dim scribble: no signature saved yet.
        color = area.get_color()
        cr.set_source_rgba(color.red, color.green, color.blue, 0.35)
        cr.set_line_width(1.6)
        cr.move_to(6, height * 0.65)
        for i in range(1, 47):
            cr.line_to(6 + i, height * 0.65 - 6 * math.sin(i / 3.2) * (1 - i / 60))
        cr.stroke()

    def _set_pen(self, color=None, width=None, smoothing=None):
        self.pen_color = color or self.pen_color
        self.pen_width = width or self.pen_width
        self.pen_smoothing = self.pen_smoothing if smoothing is None else smoothing
        # Sliders change it many times a second: write once they settle.
        if self._pen_save:
            GLib.source_remove(self._pen_save)
        self._pen_save = GLib.timeout_add(PEN_SAVE_DELAY, self._save_pen, True)
        self.pen_swatch.queue_draw()
        self.pen_preview.queue_draw()
        self.signature_thumb.queue_draw()

    def _save_pen(self, from_timer=False):
        """Write the pen settings if a change waits (when it is due, or at close)."""
        if self._pen_save is None:
            return GLib.SOURCE_REMOVE
        if not from_timer:
            GLib.source_remove(self._pen_save)
        self._pen_save = None
        save_pen(PEN, self.pen_color, self.pen_width, self.pen_smoothing)
        return GLib.SOURCE_REMOVE

    def erasing(self):
        return self.eraser.get_active()

    def toast(self, text, timeout=4):
        # Plain text: an error may carry a path with "&" or "<" in it.
        self.toasts.add_toast(Adw.Toast(title=GLib.markup_escape_text(text), timeout=timeout))

    # -- pages and marks -------------------------------------------------------
    def page(self):
        return self.doc.get_page(self.index)

    def show_page(self, index):
        self.deselect()
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
        if self.history and not self._saving:
            index, marks = self.history[-1]
            if self.selection is not None and self.selection.page == index:
                # The step restores the page as a whole: the selection was in it.
                self._drop_selection()
            else:
                self.deselect()  # on another page: back where it is, untouched by this step
            self.history.pop()
            self.marks[index] = marks
            if index != self.index:
                self.show_page(index)
        self._update()

    def _clear_page(self):
        if self._saving:
            return
        self.deselect()
        marks = self.marks.pop(self.index, [])
        if marks:
            self.history.append((self.index, marks))
        self._update()

    def _update(self):
        strokes = [m for m in self.marks.get(self.index, []) if isinstance(m, Stroke)]
        saving = getattr(self, "_saving", False)
        self.undo.set_sensitive(bool(self.history) and not saving)
        self.keep_button.set_visible(self.signature is None and bool(strokes))
        # While the copy is written, nothing changes and Done is not pressed twice.
        self.done.set_sensitive(not saving and (any(self.marks.values()) or self.selection is not None))
        self.area.queue_draw()

    # -- selection -------------------------------------------------------------
    @property
    def select_mode(self):
        return self.lasso.get_active()

    def select(self, marks):
        """Picks marks of the page shown: off its list while picked."""
        self.deselect()
        current = self.marks.get(self.index, [])
        mine = [m for m in marks if any(m is c for c in current)]
        if not mine:
            self.area.queue_draw()
            return
        self.marks[self.index] = [c for c in current if not any(c is m for m in mine)]
        self.selection = picking.Selection(self.index, mine)
        self.selection_bar.set_reveal_child(True)
        self._update()

    def deselect(self):
        """Puts the selection back on its page, as it is now."""
        chosen = self._drop_selection()
        if chosen is not None:
            self.marks.setdefault(chosen.page, []).extend(chosen.marks())
            self._update()

    def _drop_selection(self):
        chosen, self.selection = self.selection, None
        if chosen is not None:
            self.selection_bar.set_reveal_child(False)
            self.area._grab = None
        return chosen

    def step(self, chosen):
        """An undo step before the selection changes: its page as it is now."""
        self.history.append((chosen.page, self.marks.get(chosen.page, []) + chosen.marks()))
        self.undo.set_sensitive(True)

    def delete_selection(self):
        chosen = self.selection
        if chosen is not None:
            self.step(chosen)
            self._drop_selection()
            self._update()

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
        """Called by the page on a tap; True if it placed something. What
        it places comes picked: placed a little off, it moves, turns or
        scales right away."""
        if self.pending is None:
            return False
        self.deselect()
        placed = self.pending(x, y)
        self.add_mark(*placed)
        self._disarm()
        self.select(placed)
        return True

    def _escape(self):
        if self.selection is not None:
            self.deselect()
        elif self.pending:
            self._disarm()
        else:
            self._cancel()

    @staticmethod
    def _first_signature():
        found, _removed = signatures.load()
        return found[0] if found else None

    def _place_signature(self):
        """The first saved signature; with none yet, the pad to draw one."""
        if self.signature is None:
            self._new_signature()
            return
        signature = self.signature
        self._arm(lambda x, y: place(signature, x, y, pen=self.pen_width, color=self.pen_color),
                  _("Tap where your signature goes"))

    def _show_signatures(self):
        self.signature_list.color, self.signature_list.pen = self.pen_color, self.pen_width
        self.signature_list.reload()

    def _pick_signature(self, signature):
        self.sign_button.get_popover().popdown()
        signatures.use(signature["id"])
        self._signatures_changed()
        self._place_signature()

    def _new_signature(self):
        self.sign_button.get_popover().popdown()

        def saved(signature):
            self._signatures_changed()
            self._place_signature()
        SignaturePad(self, self.pen_color, self.pen_width, on_saved=saved).present()

    def _signatures_changed(self):
        self.signature = self._first_signature()
        self.signature_thumb.queue_draw()
        self._update()

    def _keep_signature(self):
        self.deselect()
        strokes = [m for m in self.marks.get(self.index, []) if isinstance(m, Stroke)]
        if strokes:
            try:
                signatures.add(normalize(strokes))
            except (OSError, ValueError) as e:
                self.toast(_("Could not save: {error}").format(error=getattr(e, "strerror", None) or e))
                return
            self._signatures_changed()
            self.toast(_("Saved: next time, “My signature” places it with one tap"))

    def _place_date(self):
        today = date_text(datetime.date.today())
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
        self.deselect()  # picked marks go back on their page, as they are
        self._saving = True
        self.cancel_button.set_sensitive(False)
        self.done.set_sensitive(False)
        self.area.set_sensitive(False)  # what would be drawn now would not be in the copy
        self.done.set_child(Gtk.Spinner(spinning=True))
        marks = {i: list(m) for i, m in self.marks.items() if m}

        def work():
            try:
                result = save_signed(self.copy, marks, dst, like=self.path)
            except Exception as e:  # noqa: BLE001 - shown to the user
                log("signing: could not save:", repr(e))
                result = e
            GLib.idle_add(once(self._saved, result, dst is None))
        threading.Thread(target=work, daemon=True).start()

    def _saved(self, result, next_to_original):
        self._saving = False
        if isinstance(result, OSError) and next_to_original:
            # A read-only folder (or file system, or a full one), or a Flatpak
            # that may only touch the file it was given: ask where to put the
            # signed copy.
            self._save_as()
            return
        if isinstance(result, Exception):
            self.toast(_("Could not save: {error}").format(error=result), timeout=0)
            self._saving_over()
            return
        self.result = result
        self._may_close = True
        self.close()

    def _saving_over(self):
        self.area.set_sensitive(True)
        self.cancel_button.set_sensitive(True)
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

    def _drawn(self):
        return any(self.marks.values()) or self.selection is not None

    def _cancel(self):
        if self._saving:
            return
        if not self._drawn():  # a placed signature comes picked: it counts
            self._may_close = True
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
        self._may_close = True
        self.close()

    def _alive(self):
        """Re-arms the stack dump, which fires only if this stops running."""
        if self._watching:
            faulthandler.dump_traceback_later(STALL)
        return self._watching

    def _on_close(self, _window):
        if self._saving:
            return True  # closing now would lose the result the file is being written for
        if not self._may_close and self._drawn():
            self._cancel()  # Alt+F4, the overview's ✕: asked as Cancel asks
            return True
        self._watching = False
        faulthandler.cancel_dump_traceback_later()
        try:
            self._save_pen()
        except OSError as e:  # the result still goes to whoever waits for it
            log("signing: pen not kept:", repr(e))
        pen_feed.unlisten(self.area.raw.reader)
        drawn, desktop = self.area.raw_counts
        if drawn:
            log(f"pen: drew {drawn} tablet samples; the desktop passed on {desktop} meanwhile; "
                + self.area.stats.summary())
        callback, self.on_done = self.on_done, None
        if callback:
            callback(self.result)
        _forget(self.copy)
        return False
