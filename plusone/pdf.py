"""Ink, text and saved signatures on top of an existing PDF.

Marks live in display space: PDF points with the origin at the top-left of
the page as a viewer shows it (crop box, /Rotate applied), y downwards. That
is the space poppler renders into, so the signing window can store what the
pen draws as is. Saving draws the marks into a transparent PDF with cairo and
stamps it onto the original pages with pypdf, so the original text, forms and
metadata stay as they are.
"""
import io
import math
import os
from dataclasses import dataclass, field

import cairo

INK = (0.05, 0.1, 0.35)  # dark blue, reads as a pen signature
PEN_WIDTH = 1.6          # points at full pressure
FULL_PRESSURE = 0.6      # firm writing with the S Pen reads 0.3-0.5


@dataclass
class Stroke:
    points: list = field(default_factory=list)  # (x, y, pressure 0..1)
    width: float = PEN_WIDTH
    color: tuple = INK

    def bounds(self):
        xs = [p[0] for p in self.points]
        ys = [p[1] for p in self.points]
        return min(xs), min(ys), max(xs), max(ys)


@dataclass
class Text:
    x: float
    y: float          # baseline
    text: str
    size: float = 11.0
    color: tuple = INK


def curve(points):
    """Smooth path through the pen samples: for each pair of samples, the
    Bézier control points of a Catmull-Rom spline with chord-length
    tangents (no overshoot where samples are unevenly spaced, which they are:
    the pen reports at a fixed rate whatever its speed).

    Yields (start, control 1, control 2, end, pressure at start, at end)."""
    pts = [tuple(points[0])]
    for pt in points[1:]:
        if tuple(pt[:2]) != pts[-1][:2]:
            pts.append(tuple(pt))
    for i in range(len(pts) - 1):
        p0, p1, p2 = pts[max(i - 1, 0)], pts[i], pts[i + 1]
        p3 = pts[min(i + 2, len(pts) - 1)]
        d01, d12, d23 = math.dist(p0[:2], p1[:2]), math.dist(p1[:2], p2[:2]), math.dist(p2[:2], p3[:2])
        c1 = [p1[k] + (p2[k] - p0[k]) * d12 / (3 * (d01 + d12)) for k in (0, 1)]
        c2 = [p2[k] - (p3[k] - p1[k]) * d12 / (3 * (d12 + d23)) for k in (0, 1)]
        yield p1[:2], c1, c2, p2[:2], p1[2], p2[2]


def ink_width(width, pressure):
    """Line width for a pen pressure (0..1): from a hairline for a light
    touch to the full width for firm writing, so pressure shows."""
    return width * (0.1 + 0.9 * min(1.0, max(0.0, pressure) / FULL_PRESSURE))


def draw_segments(cr, stroke, segments):
    """Draw some of a stroke's curve() segments, exactly as draw_marks()
    does: segment by segment, so the width follows the pen pressure."""
    cr.set_line_cap(cairo.LINE_CAP_ROUND)
    cr.set_line_join(cairo.LINE_JOIN_ROUND)
    cr.set_source_rgb(*stroke.color)
    for start, c1, c2, end, p0, p1 in segments:
        cr.set_line_width(ink_width(stroke.width, (p0 + p1) / 2))
        cr.move_to(*start)
        cr.curve_to(*c1, *c2, *end)
        cr.stroke()


def draw_marks(cr, marks):
    """Draw marks on a cairo context set up in display space (points)."""
    cr.set_line_cap(cairo.LINE_CAP_ROUND)
    cr.set_line_join(cairo.LINE_JOIN_ROUND)
    for mark in marks:
        cr.set_source_rgb(*mark.color)
        if isinstance(mark, Stroke):
            if not mark.points:
                continue
            segments = list(curve(mark.points))
            if not segments:
                x, y, p = mark.points[0]
                cr.arc(x, y, ink_width(mark.width, p) / 2, 0, 2 * math.pi)
                cr.fill()
                continue
            draw_segments(cr, mark, segments)
        elif isinstance(mark, Text):
            cr.select_font_face("Sans", cairo.FONT_SLANT_NORMAL, cairo.FONT_WEIGHT_NORMAL)
            cr.set_font_size(mark.size)
            cr.move_to(mark.x, mark.y)
            cr.show_text(mark.text)


def display_size(page):
    """(width, height) of a pypdf page as a viewer shows it."""
    box = page.cropbox
    w, h = float(box.width), float(box.height)
    return (h, w) if page.rotation % 180 else (w, h)


def display_to_user(page):
    """Matrix (a, b, c, d, e, f) from a display-space overlay page (PDF
    coordinates, origin bottom-left) to the page's own user space."""
    box = page.cropbox
    x0, y0 = float(box.left), float(box.bottom)
    w, h = float(box.width), float(box.height)
    rotation = page.rotation % 360
    if rotation == 90:
        return (0, 1, -1, 0, w + x0, y0)
    if rotation == 180:
        return (-1, 0, 0, -1, w + x0, h + y0)
    if rotation == 270:
        return (0, -1, 1, 0, x0, h + y0)
    return (1, 0, 0, 1, x0, y0)


def output_path(src):
    """name-signed.pdf next to src, never an existing file."""
    base, ext = os.path.splitext(src)
    candidate, n = f"{base}-signed{ext or '.pdf'}", 2
    while os.path.exists(candidate):
        candidate, n = f"{base}-signed-{n}{ext or '.pdf'}", n + 1
    return candidate


def save_signed(src, marks_by_page, dst=None):
    """Write src with marks_by_page ({page index: [Stroke | Text]}) on top.
    Returns the path written."""
    from pypdf import PdfReader, PdfWriter, Transformation

    reader = PdfReader(src)
    writer = PdfWriter(clone_from=reader)
    for index, marks in sorted(marks_by_page.items()):
        if not marks:
            continue
        page = writer.pages[index]
        width, height = display_size(page)
        buf = io.BytesIO()
        surface = cairo.PDFSurface(buf, width, height)
        draw_marks(cairo.Context(surface), marks)
        surface.finish()
        overlay = PdfReader(buf).pages[0]
        page.merge_transformed_page(overlay, Transformation(display_to_user(page)))
    dst = dst or output_path(src)
    tmp = dst + ".part"
    with open(tmp, "wb") as f:
        writer.write(f)
    os.replace(tmp, dst)
    return dst
