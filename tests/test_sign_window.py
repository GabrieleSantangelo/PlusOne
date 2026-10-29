"""The signing window, driven without input devices.

Needs a display: CI runs it on GTK's Broadway backend (tools/ci.sh sets
PLUSONE_GUI_TESTS=1); on a desktop it is skipped so no window pops up.
"""
import math
import os
import struct
import time

import pytest

if not os.environ.get("PLUSONE_GUI_TESTS"):
    pytest.skip("GUI tests run in CI (set PLUSONE_GUI_TESTS=1)", allow_module_level=True)

pypdf = pytest.importorskip("pypdf")

import cairo  # noqa: E402
import gi  # noqa: E402

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Adw, Gdk, GLib, Gtk  # noqa: E402

from test_pdf import make_pdf  # noqa: E402

from plusone import sign, signing  # noqa: E402
from plusone.ink import load_pen  # noqa: E402
from plusone.pen import feed  # noqa: E402
from plusone.pdf import Stroke, Text  # noqa: E402


def spin(until, timeout=5):
    """Run the GLib main context until until() is true."""
    context = GLib.MainContext.default()
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        while context.pending():
            context.iteration(False)
        if until():
            return True
        time.sleep(0.01)
    return False


@pytest.fixture
def window(tmp_path, monkeypatch):
    monkeypatch.setattr(sign, "PEN", str(tmp_path / "pen.json"))
    monkeypatch.setattr(signing, "SENT", str(tmp_path / "sent"))  # the copy the window signs
    src = tmp_path / "contract.pdf"
    make_pdf(src)
    app = Adw.Application(application_id="io.github.gabrielesantangelo.PlusOne.Test")
    app.register(None)
    results = []
    win = sign.SignWindow(app, str(src), results.append)
    win.present()
    assert spin(lambda: win.area.fitted), "page never laid out"
    yield win, results, src
    if win.on_done:
        win.close()


def test_opens_on_first_page(window):
    win, _results, _src = window
    assert win.doc.get_n_pages() == 2
    assert not win.done.get_sensitive()        # nothing to save yet
    assert win.signature is None               # no saved signature yet
    assert win.area.fitted


def test_draw_undo_and_pages(window):
    win, _results, _src = window
    win.add_mark(Stroke([(100, 100, 0.5), (150, 120, 0.7)]))
    assert win.done.get_sensitive() and win.keep_button.get_visible()
    win.show_page(1)
    win.add_mark(Text(80, 300, "page two"))
    win._undo()
    assert win.marks[1] == []
    win._undo()                                # goes back to page 1 to undo there
    assert win.index == 0 and win.marks[0] == []
    assert not win.done.get_sensitive()


def test_eraser_and_undo(window):
    win, _results, _src = window
    keep, gone = Stroke([(100, 100, 0.5), (150, 100, 0.5)]), Stroke([(100, 200, 0.5), (150, 200, 0.5)])
    other = Stroke([(100, 300, 0.5), (150, 300, 0.5)])
    win.add_mark(keep, gone, other)
    assert win.erase((120, 190), (120, 210), 3, new_step=True)     # crosses `gone`
    assert win.marks[0] == [keep, other]
    assert not win.erase((300, 0), (300, 50), 3, new_step=False)  # touches nothing
    assert win.erase((120, 290), (120, 310), 3, new_step=False)    # same eraser stroke
    assert win.marks[0] == [keep]
    win._undo()                                                   # one step brings both back
    assert win.marks[0] == [keep, gone, other]
    win._clear_page()
    assert not win.marks.get(0)
    win._undo()                                                   # clearing can be undone too
    assert win.marks[0] == [keep, gone, other]
    win._undo()                                                   # the three strokes were one step
    assert win.marks[0] == []


def test_keep_and_place_signature(window):
    win, _results, _src = window
    win.add_mark(Stroke([(100, 100, 0.5), (160, 140, 0.7)]))
    win._keep_signature()
    assert win.signature
    win._place_signature()
    assert win.place_at(300, 500)              # the tap places it
    assert not win.place_at(310, 510)          # and only once
    assert win.selection is not None           # picked, to move it if it is off
    win.deselect()
    assert len(win.marks[0]) == 2


def test_date_and_save(window):
    win, results, src = window
    win._place_date()
    win.place_at(100, 700)
    win.add_mark(Stroke([(200, 600, 0.9), (260, 640, 0.9)]))
    win._save()
    assert spin(lambda: results, timeout=10)
    out = results[0]
    assert out == str(src).replace(".pdf", "-signed.pdf")
    text = pypdf.PdfReader(out).pages[0].extract_text()
    import datetime
    assert sign.date_text(datetime.date.today()) in text and "Original text page 1" in text


def test_what_is_signed_is_the_pdf_shown(window, tmp_path):
    # The original replaced by another document while the window is open:
    # the marks go on the one shown, the signed copy next to the original.
    win, results, src = window
    other = tmp_path / "other.pdf"
    surface = cairo.PDFSurface(str(other), 595, 842)
    context = cairo.Context(surface)
    context.move_to(72, 72)
    context.show_text("Another document")
    surface.finish()
    os.replace(other, src)
    win.add_mark(Stroke([(200, 600, 0.9), (260, 640, 0.9)]))
    win._save()
    assert spin(lambda: results, timeout=10)
    assert results == [str(src).replace(".pdf", "-signed.pdf")]
    assert "Original text page 1" in pypdf.PdfReader(results[0]).pages[0].extract_text()
    assert not os.path.exists(win.copy)  # closed: its copy gone


def test_the_pdf_shown_is_signed_after_the_original_went(window):
    win, results, src = window
    os.unlink(src)  # moved, or deleted, while the window is open
    win.add_mark(Stroke([(200, 600, 0.9), (260, 640, 0.9)]))
    win._save()
    assert spin(lambda: results, timeout=10)
    assert results == [str(src).replace(".pdf", "-signed.pdf")]


def test_the_overview_is_left_alone_while_no_tile_waits(window, monkeypatch):
    # The page drawn small stands in for tiles still to render: brought to
    # the marks at every frame all the same, it took a pass of poppler more
    # at each frame of an eraser or an undo (twice the work on a scan).
    win, _results, _src = window
    area = win.area
    assert spin(lambda: area._raster is not None and not area._overview_shown)  # every tile there
    frames, overviews = [], []
    page_raster, overview_raster = sign.PageArea._page_raster, sign.PageArea._overview_raster
    monkeypatch.setattr(sign.PageArea, "_page_raster", lambda self, scale: frames.append(1) or page_raster(self, scale))
    monkeypatch.setattr(sign.PageArea, "_overview_raster", lambda self: overviews.append(1) or overview_raster(self))
    win.add_mark(Stroke([(200, 600, 0.9), (260, 640, 0.9)]))
    assert spin(lambda: frames)
    win._undo()
    assert spin(lambda: len(frames) >= 2)
    assert overviews == []


def test_no_way_out_while_saving(window):
    # Cancel, Escape or the window manager closing the window while the copy
    # is written would report nothing although the file appears.
    win, results, src = window
    win.add_mark(Stroke([(200, 600, 0.9), (260, 640, 0.9)]))
    win._save()
    win._escape()
    win.close()
    assert spin(lambda: results, timeout=10)
    assert results == [str(src).replace(".pdf", "-signed.pdf")]


def test_cancel_without_marks_reports_nothing(window):
    win, results, _src = window
    win._cancel()
    assert spin(lambda: results)
    assert results == [None]


class Stylus:
    """What the pen handlers ask of GTK's stylus gesture: a plain pen."""
    def get_device_tool(self):
        return None

    def get_current_event_state(self):
        return Gdk.ModifierType(0)

    def get_axis(self, axis):
        return False, 0.0

    def get_backlog(self):
        return False, []


def tablet(bits, x, y):
    """A pen report from the tablet at widget pixel x, y (the fake mapping
    below makes the tablet 1000x1000 widget pixels)."""
    feed.publish(struct.pack("<BBHHHbb", 0x14, bits, round(x * 32.767), round(y * 32.767), 1500, 0, 0))


def to_line(point, points):
    """Distance from point to the polyline through points."""
    best = math.inf
    for (ax, ay, _p), (bx, by, _q) in zip(points, points[1:]):
        dx, dy = bx - ax, by - ay
        t = max(0.0, min(1.0, ((point[0] - ax) * dx + (point[1] - ay) * dy) / ((dx * dx + dy * dy) or 1)))
        best = min(best, math.dist(point, (ax + t * dx, ay + t * dy)))
    return best


def test_strokes_use_every_tablet_sample(window, monkeypatch):
    win, _results, _src = window
    area = win.area
    monkeypatch.setattr(area, "_tablet_mapping", lambda: (1000.0, 1000.0, 0.0, 0.0))
    for i in range(10):
        tablet(0x11, 200 + i, 300)
    area._pen_down(Stylus(), 200, 300)          # GTK reports the touch afterwards
    for i in range(10, 40):
        tablet(0x11, 200 + i, 300 + i / 4)
    tablet(0x10, 240, 310)                     # tip lifted
    area._pen_motion(Stylus(), 239, 309.75)    # GTK passed on only the last of the burst
    area._pen_up(Stylus(), 240, 310)
    stroke = win.marks[0][-1]
    # Smoothed, but from the touch to where the tip lifted.
    assert stroke.points[0][:2] == pytest.approx(area.view.to_page(200, 300), abs=0.05)
    assert stroke.points[-1][:2] == pytest.approx(area.view.to_page(239, 309.75), abs=0.05)
    assert stroke.points[0][2] == pytest.approx(1500 / 4096)
    samples = [area.view.to_page(200 + i, 300 + (i / 4 if i >= 10 else 0)) for i in range(40)]
    # Within 2 px of every sample (the 2.5 px step at the tenth is rounded off).
    assert all(to_line(s, stroke.points) * area.view.zoom < 2.0 for s in samples)
    assert area.raw_counts == [40, 1] and area._raw_tick is None


def test_tablet_samples_that_do_not_line_up_are_ignored(window, monkeypatch):
    win, _results, _src = window
    area = win.area
    monkeypatch.setattr(area, "_tablet_mapping", lambda: (1000.0, 1000.0, 0.0, 0.0))
    tablet(0x11, 600, 600)
    area._pen_down(Stylus(), 200, 300)          # far away: not this pen
    area._pen_motion(Stylus(), 220, 300)
    area._pen_up(Stylus(), 240, 300)
    stroke = win.marks[0][-1]
    assert stroke.points[0][:2] == pytest.approx(area.view.to_page(200, 300))
    assert area.raw_counts == [0, 0]


def test_pen_colour_and_width(window):
    win, _results, _src = window
    win._set_pen(color=(0.8, 0.1, 0.1), width=4.0, smoothing=0)
    win.area._begin(100, 100, 0.5)
    win.area._extend(150, 120, 0.5)
    win.area._finish()
    assert (win.marks[0][-1].color, win.marks[0][-1].width) == ((0.8, 0.1, 0.1), 4.0)
    win._place_date()
    win.place_at(100, 700)
    assert win.marks[0][-1].color == (0.8, 0.1, 0.1)
    assert spin(lambda: load_pen(sign.PEN) == ((0.8, 0.1, 0.1), 4.0, 0))   # remembered for next time


class Drag:
    """What the mouse handlers ask of GTK's drag gesture; tool set for the pen."""
    def __init__(self, tool=None):
        self.tool, self.state = tool, None

    def get_current_event(self):
        return self

    def get_device_tool(self):
        return self.tool

    def get_device(self):
        return None  # GTK's tablet device says it is a mouse anyway

    def set_state(self, state):
        self.state = state


def test_the_pen_as_a_pointer_drag_never_draws(window):
    win, _results, _src = window
    area = win.area
    area._pen_down(Stylus(), 200, 300)
    pen_drag = Drag(tool=object())
    area._mouse_begin(pen_drag, 200, 300)       # GTK reports the pen as a drag too
    assert pen_drag.state == Gtk.EventSequenceState.DENIED
    area._pen_motion(Stylus(), 205, 300)
    area._pen_up(Stylus(), 210, 300)
    assert len(win.marks[0]) == 1
    # A mouse draws, and a pen stroke in progress keeps it out.
    mouse = Drag()
    area._mouse_begin(mouse, 100, 100)
    area._mouse_update(mouse, 40, 0)
    area._mouse_end(mouse, 40, 0)
    assert mouse.state is None and len(win.marks[0]) == 2
    area._pen_down(Stylus(), 300, 300)
    late = Drag()
    area._mouse_begin(late, 300, 300)
    assert late.state == Gtk.EventSequenceState.DENIED
    area._pen_up(Stylus(), 310, 300)


def test_a_long_stroke_is_inked_once(window):
    # Settled segments become textures as the stroke grows, and go onto the
    # cached page when it ends; that page must match one drawn from scratch
    # (drawn twice, the line would come out bolder).
    win, _results, _src = window
    area = win.area
    area._begin(200, 300, 0.4)
    for i in range(1, 240):
        area._extend(200 + i, 300 + 40 * math.sin(i / 15), 0.3 + 0.2 * math.sin(i / 30))
        if i % 25 == 0:
            area.do_snapshot(Gtk.Snapshot())
    assert area._inked > 150 and len(area._chunks) == area._inked // sign.CHUNK
    area._finish()
    area.do_snapshot(Gtk.Snapshot())
    raster = area._raster
    assert raster.drawn == tuple(win.marks[0])
    fresh = sign.PageRaster(raster.index, raster.scale, win.page(), area.page_size())
    fresh.update(win.marks[0])
    for key in raster.tile_boxes():
        raster.tile(*key)
        fresh.tile(*key)
    raster.surface.flush()
    fresh.surface.flush()
    a, b = bytes(raster.surface.get_data()), bytes(fresh.surface.get_data())
    # Chunks are composited in 8 bits: at most a rounding step apart.
    assert len(a) == len(b) and max(abs(x - y) for x, y in zip(a, b)) <= 2

def test_signature_pad_and_list(window):
    from plusone import signatures
    from plusone.signature_pad import SignatureList, SignaturePad
    win, _results, _src = window
    saved = []
    pad = SignaturePad(win, (0, 0, 0), 1.6, on_saved=saved.append)
    pad.present()
    assert spin(lambda: pad.area.get_width() > 0)
    pad._begin(pad.drag_gesture, 40, 80)
    for dx in range(0, 300, 20):
        pad._move(pad.drag_gesture, dx, dx / 4)
    pad._end(pad.drag_gesture, 300, 75)
    assert pad.save_button.get_sensitive()
    # The eraser takes a stroke away, and Save goes with the last one.
    pad._begin(pad.drag_gesture, 10, 200)
    pad._move(pad.drag_gesture, 50, 0)
    pad._end(pad.drag_gesture, 50, 0)
    assert len(pad.strokes) == 2
    pad.eraser.set_active(True)
    pad._begin(pad.drag_gesture, 30, 190)
    pad._move(pad.drag_gesture, 0, 20)
    pad._end(pad.drag_gesture, 0, 20)
    pad.eraser.set_active(False)
    assert len(pad.strokes) == 1 and pad.save_button.get_sensitive()
    pad._save()
    assert len(saved) == 1 and signatures.load()[0] == saved
    # The pad is 280 points wide: the drawn width comes back in points.
    assert 0 < saved[0]["width"] < 280
    picked = []
    listing = SignatureList((0, 0, 0), 1.6, on_pick=picked.append)
    rows = listing.rows
    assert rows.get_first_child() is not None
    signatures.remove(saved[0]["id"])
    listing.reload()
    assert isinstance(rows.get_first_child(), Gtk.Label)  # "No signature saved yet"


def test_select_move_turn_and_undo(window):
    win, _results, _src = window
    stroke = Stroke([(100, 100, 0.5), (200, 100, 0.5)])
    win.add_mark(stroke)
    area = win.area
    # The lasso, around the stroke.
    win.lasso.set_active(True)
    corners = [area.view.to_widget(x, y) for x, y in ((80, 80), (220, 80), (220, 120), (80, 120))]
    assert area._select_down(*corners[0], erasing=False)
    for point in corners[1:]:
        area._select_move(*point)
    area._select_up()
    assert win.selection is not None and win.marks[0] == []
    assert win.selection_bar.get_reveal_child()
    # Drag it 50 points right, from inside.
    start = area.view.to_widget(150, 100)
    assert area._select_down(*start, erasing=False)
    area._select_move(*area.view.to_widget(200, 100))
    area._select_up()
    win.deselect()
    moved = win.marks[0][0]
    assert moved.points[0][0] == pytest.approx(150, abs=0.01)
    # Undo puts it back where it was.
    win._undo()
    assert win.marks[0][0].points[0][0] == pytest.approx(100, abs=0.01)
    # A placed date comes picked.
    win._place_date()
    win.place_at(300, 500)
    assert win.selection is not None and isinstance(win.selection.originals[0], Text)


def test_undo_on_another_page_keeps_the_selection(window):
    # A step of another page undone: the marks picked on this page go back
    # where they are, not away for good.
    win, _results, _src = window
    picked = Stroke([(100, 100, 0.5), (200, 100, 0.5)])
    win.show_page(1)
    win.add_mark(Text(80, 300, "page two"))
    win.show_page(0)
    win.add_mark(picked)
    win.select([picked])
    win.history.insert(0, win.history.pop())  # page two's step last, as if drawn after
    win._undo()
    assert win.selection is None and win.marks[0] == [picked]


def test_a_picked_mark_is_something_to_discard(window):
    # A placed signature or date comes picked: Cancel and closing the window
    # still ask before throwing it away.
    win, results, _src = window
    stroke = Stroke([(100, 100, 0.5), (200, 100, 0.5)])
    win.add_mark(stroke)
    win.select([stroke])
    asked = []
    win._cancel = lambda: asked.append(True)
    assert win._on_close(win) is True and asked and not results
