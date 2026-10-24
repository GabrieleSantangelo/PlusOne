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

import gi  # noqa: E402

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Adw, Gdk, GLib  # noqa: E402

from test_pdf import make_pdf  # noqa: E402

from plusone import sign  # noqa: E402
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
    monkeypatch.setattr(sign, "SIGNATURE", str(tmp_path / "signature.json"))
    monkeypatch.setattr(sign, "PEN", str(tmp_path / "pen.json"))
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
    assert not win.sign_button.get_sensitive()  # no saved signature yet
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
    assert win.signature and win.sign_button.get_sensitive()
    win._place_signature()
    assert win.place_at(300, 500)              # the tap places it
    assert not win.place_at(310, 510)          # and only once
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
    assert time.strftime("%d/%m/%Y") in text and "Original text page 1" in text


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
    # Smoothed, but from the touch to where the tip lifted, near every sample.
    assert stroke.points[0][:2] == pytest.approx(area.view.to_page(200, 300), abs=0.05)
    assert stroke.points[-1][:2] == pytest.approx(area.view.to_page(239, 309.75), abs=0.05)
    assert stroke.points[0][2] == pytest.approx(1500 / 4096)
    samples = [area.view.to_page(200 + i, 300 + (i / 4 if i >= 10 else 0)) for i in range(40)]
    assert all(min(math.dist(s, p[:2]) for p in stroke.points) * area.view.zoom < 1.5 for s in samples)
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
    assert load_pen(sign.PEN) == ((0.8, 0.1, 0.1), 4.0, 0)   # remembered for next time
