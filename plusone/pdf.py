"""Ink, text and saved signatures on top of an existing PDF.

Marks live in display space: PDF points with the origin at the top-left of
the page as a viewer shows it (crop box, /Rotate applied), y downwards. That
is the space poppler renders into, so the signing window can store what the
pen draws as is. Saving draws the marks into a transparent PDF with cairo and
adds it to the original pages with pypdf, as a form drawn after their own
content: the original content streams are left as they are (not decoded and
written again, which bloated files and, in pypdf 4.0, could let their
graphics state leak into the ink), and so are text, forms and metadata.
"""
import io
import math
import os
import tempfile
from dataclasses import dataclass, field

import cairo
import gi

gi.require_version("Pango", "1.0")
gi.require_version("PangoCairo", "1.0")
from gi.repository import Pango, PangoCairo  # noqa: E402

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
    y: float          # baseline's left end
    text: str
    size: float = 11.0
    color: tuple = INK
    angle: float = 0.0  # radians, clockwise (y points down), about x, y


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
            draw_text(cr, mark)


def draw_text(cr, mark):
    """A text mark with Pango: any script, shaped, with font fallback (cairo's
    own text drew boxes for Chinese and did not join Arabic)."""
    layout = PangoCairo.create_layout(cr)
    font = Pango.FontDescription.from_string("Sans")
    font.set_absolute_size(mark.size * Pango.SCALE)  # points, whatever the screen's dpi
    layout.set_font_description(font)
    layout.set_text(mark.text, -1)
    cr.save()
    cr.translate(mark.x, mark.y)
    cr.rotate(mark.angle)
    cr.move_to(0, -layout.get_baseline() / Pango.SCALE)  # x, y is the baseline's left end
    PangoCairo.show_layout(cr, layout)
    cr.restore()


def _box(page, name):
    """A page box as (left, bottom, right, top), corners in any order put
    right; None if missing or empty."""
    try:
        # Each corner may be an indirect object (a height kept elsewhere).
        values = [float(v.get_object() if hasattr(v, "get_object") else v) for v in page[name].get_object()]
    except (KeyError, TypeError, ValueError, AttributeError):
        return None
    if len(values) != 4:
        return None
    x0, x1 = sorted(values[0::2])
    y0, y1 = sorted(values[1::2])
    return (x0, y0, x1, y1) if x1 > x0 and y1 > y0 else None


def visible_box(page):
    """The part of a pypdf page a viewer shows, as poppler takes it: the crop
    box within the media box (US Letter when the media box makes no sense)."""
    media = _box(page, "/MediaBox") or (0.0, 0.0, 612.0, 792.0)
    crop = _box(page, "/CropBox")
    if crop:
        x0, y0 = max(crop[0], media[0]), max(crop[1], media[1])
        x1, y1 = min(crop[2], media[2]), min(crop[3], media[3])
        if x1 > x0 and y1 > y0:
            return x0, y0, x1, y1
    return media


def display_size(page):
    """(width, height) of a pypdf page as a viewer shows it."""
    x0, y0, x1, y1 = visible_box(page)
    w, h = x1 - x0, y1 - y0
    return (h, w) if page.rotation % 180 else (w, h)


def display_to_user(page):
    """Matrix (a, b, c, d, e, f) from a display-space overlay page (PDF
    coordinates, origin bottom-left) to the page's own user space."""
    x0, y0, x1, y1 = visible_box(page)
    w, h = x1 - x0, y1 - y0
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


MAX_SIZE = 256 * 1024 * 1024  # bytes: what the tablet's app takes (Wire.kt MAX_PAYLOAD)


def problem(path):
    """Why path cannot be signed, or None: an English template (i18n) with
    its arguments. Checked before anything goes to the tablet."""
    try:
        if not os.path.isfile(path):
            return "{name} is not a file", {}
        if os.path.getsize(path) > MAX_SIZE:
            return "{name} is too big to sign: more than {size} MB", {"size": MAX_SIZE // (1024 * 1024)}
        with open(path, "rb") as f:
            if b"%PDF-" not in f.read(1024):
                return "{name} is not a PDF", {}
    except OSError as e:
        return "{name} cannot be read: {error}", {"error": e.strerror or str(e)}
    from pypdf import PdfReader
    try:
        reader = PdfReader(path)
        if reader.is_encrypted and not reader.decrypt(""):
            return "{name} needs a password to open: unlock it first", {}
    except Exception:  # noqa: BLE001 - one poppler may still read: the window says if not
        pass
    return None


def digitally_signed(src):
    """Whether src carries a digital signature (or a certification): the
    signed copy is a new file, where it no longer holds."""
    from pypdf import PdfReader
    try:
        root = PdfReader(src).trailer["/Root"].get_object()
        form = root.get("/AcroForm")
        form = form.get_object() if form is not None else {}
        return "/Perms" in root or bool(int(form.get("/SigFlags", 0)) & 1)
    except Exception:  # noqa: BLE001 - a PDF pypdf cannot read this far: none known
        return False


def _ink_form(writer, marks, width, height):
    """The marks as a form XObject of the writer, in display space."""
    from pypdf import PdfReader
    from pypdf.generic import ArrayObject, FloatObject, NameObject, StreamObject

    buf = io.BytesIO()
    surface = cairo.PDFSurface(buf, width, height)
    draw_marks(cairo.Context(surface), marks)
    surface.finish()
    overlay = PdfReader(buf).pages[0]
    form = StreamObject()
    form[NameObject("/Type")] = NameObject("/XObject")
    form[NameObject("/Subtype")] = NameObject("/Form")
    form[NameObject("/BBox")] = ArrayObject([FloatObject(0), FloatObject(0), FloatObject(width), FloatObject(height)])
    form[NameObject("/Resources")] = overlay["/Resources"].clone(writer)
    form.set_data(overlay.get_contents().get_data())
    return writer._add_object(form.flate_encode())


def _page_resources(page):
    """The page's own resources dictionary, copied from its page tree when
    inherited (adding to a shared one would touch other pages)."""
    from pypdf.generic import DictionaryObject, IndirectObject, NameObject
    node, seen = page, set()
    while "/Resources" not in node and "/Parent" in node:
        parent = node.raw_get("/Parent")
        key = (parent.idnum, parent.generation) if isinstance(parent, IndirectObject) else id(parent)
        if key in seen:
            node = DictionaryObject()  # a tree that is its own parent: nothing to inherit (it went round for ever)
            break
        seen.add(key)
        node = parent.get_object()
    found = node.get("/Resources")
    resources = DictionaryObject(found.get_object()) if found is not None else DictionaryObject()
    page[NameObject("/Resources")] = resources
    return resources


def _stamp(writer, page, form):
    """Draws form on top of page, its content left as it is: wrapped in q/Q
    (whatever state it leaves), then the form through the display matrix."""
    from pypdf.generic import ArrayObject, DictionaryObject, IndirectObject, NameObject, StreamObject
    resources = _page_resources(page)
    forms = resources.get("/XObject")
    forms = DictionaryObject(forms.get_object()) if forms is not None else DictionaryObject()
    resources[NameObject("/XObject")] = forms
    n = 0
    while NameObject(f"/PlusOneInk{n}") in forms:
        n += 1
    name = NameObject(f"/PlusOneInk{n}")
    forms[name] = form
    before, after = StreamObject(), StreamObject()
    before.set_data(b"q\n")
    matrix = " ".join(f"{v:.6f}" for v in display_to_user(page))
    after.set_data(f"\nQ\nq {matrix} cm {name} Do Q\n".encode())
    contents = page.get("/Contents")
    contents = contents.get_object() if contents is not None else ArrayObject()
    original = list(contents) if isinstance(contents, ArrayObject) else [page["/Contents"]]
    # Each content stream an indirect object, as the format wants in an array.
    original = [c if isinstance(c, IndirectObject) else writer._add_object(c) for c in original]
    page[NameObject("/Contents")] = ArrayObject([writer._add_object(before), *original, writer._add_object(after)])


def _pypdf_before(version):
    import pypdf
    try:
        return tuple(int(part) for part in pypdf.__version__.split(".")[:2]) < version
    except ValueError:
        return False


def save_signed(src, marks_by_page, dst=None, like=None):
    """Write src with marks_by_page ({page index: [Stroke | Text]}) on top;
    marks for a page it does not have are left out. Returns the path
    written: dst, or name-signed.pdf next to like (src when not given),
    never over an existing file. like: the original, when src is a copy
    of it (signing.py): its folder and permissions."""
    from pypdf import PdfReader, PdfWriter
    from pypdf.generic import DictionaryObject

    reader = PdfReader(src)
    writer = PdfWriter(clone_from=reader)
    if "/Info" not in reader.trailer and _pypdf_before((4, 3)):
        # pypdf 4.0 pointed the trailer's /Info at another object (the page
        # tree) when the original had none: Ghostscript then saw no pages.
        writer._info = writer._add_object(DictionaryObject())
    pages = writer.pages
    for index, marks in sorted(marks_by_page.items()):
        if not marks or not 0 <= index < len(pages):
            continue
        page = pages[index]
        _stamp(writer, page, _ink_form(writer, marks, *display_size(page)))
    like = like or src
    folder = os.path.dirname(dst or like) or "."
    # A temporary file of our own next to the result, then the signed copy in
    # one step, with the original's permissions (not the umask's).
    fd, tmp = tempfile.mkstemp(prefix=".plus-one-", suffix=".part", dir=folder)
    try:
        with os.fdopen(fd, "wb") as f:
            writer.write(f)
        try:
            os.chmod(tmp, os.stat(like).st_mode & 0o777)
        except OSError:
            pass  # a mount that keeps its own (SMB through gvfs): the file is still whole
        if dst:
            os.replace(tmp, dst)
            return dst
        # A name nobody took: link() fails rather than replace what appeared
        # since output_path() looked.
        while True:
            dst = output_path(like)
            try:
                os.link(tmp, dst)
                break
            except FileExistsError:
                continue
            except OSError:
                os.replace(tmp, dst)  # no hard links there (some mounts): as before
                return dst
        os.unlink(tmp)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise
    return dst
