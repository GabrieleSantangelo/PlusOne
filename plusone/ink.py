"""Toolkit-free helpers for the signing window: page view, pen samples,
pen settings and saved signature."""
import collections
import json
import math
import os

from .modeler import LEVELS
from .pdf import INK, PEN_WIDTH, Stroke, Text

SIGNATURE_WIDTH = 140.0  # points (about 5 cm) when a saved signature is placed
MIN_WIDTH, MAX_WIDTH = 0.3, 6.0  # pen widths offered, points
SMOOTHING = 2  # of modeler.LEVELS: the ink trails the pen by ~15 ms
TIP = 0x01  # tip switch bit of a pen report


class View:
    """Maps page points to widget pixels: widget = page * zoom + offset."""

    MIN_ZOOM, MAX_ZOOM = 0.2, 8.0

    def __init__(self):
        self.zoom, self.ox, self.oy = 1.0, 0.0, 0.0

    def fit(self, page_w, page_h, widget_w, widget_h, margin=16, bottom=0):
        """Whole page visible and centred, clear of the bottom `bottom`
        pixels (where the toolbar floats)."""
        height = widget_h - bottom
        self.zoom = max(self.MIN_ZOOM, min((widget_w - 2 * margin) / page_w,
                                           (height - 2 * margin) / page_h))
        self.ox = (widget_w - page_w * self.zoom) / 2
        self.oy = (height - page_h * self.zoom) / 2

    def to_page(self, x, y):
        return (x - self.ox) / self.zoom, (y - self.oy) / self.zoom

    def to_widget(self, px, py):
        return px * self.zoom + self.ox, py * self.zoom + self.oy

    def zoom_at(self, zoom, wx, wy, anchor=None):
        """Set zoom keeping page point `anchor` (default: the one under
        wx, wy) at widget position wx, wy."""
        px, py = anchor if anchor else self.to_page(wx, wy)
        self.zoom = max(self.MIN_ZOOM, min(self.MAX_ZOOM, zoom))
        self.ox, self.oy = wx - px * self.zoom, wy - py * self.zoom

    def pan(self, dx, dy):
        self.ox += dx
        self.oy += dy


class RawPen:
    """Pen samples straight from the tablet (a pen.Feed reader), so a stroke
    is drawn from all of them rather than from what the desktop passes on.
    Samples are (time, bits, x, y, pressure)."""

    HISTORY = 1.0  # seconds kept: the desktop reports a touch after the tablet

    def __init__(self, reader):
        self.reader = reader
        self.recent = collections.deque()
        self.drawing = False
        self.latest = None

    def _pull(self):
        new = []
        while self.reader:
            new.append(self.reader.popleft())
        if new:
            self.latest = new[-1][0]  # when the newest arrived
        self.recent.extend(new)
        while self.recent and self.recent[0][0] < self.recent[-1][0] - self.HISTORY:
            self.recent.popleft()
        return new

    def begin(self):
        """(x, y, pressure) of the stroke the tip is drawing, from where it
        touched down, or None if the tablet sent none."""
        self._pull()
        samples = list(self.recent)
        end = len(samples)
        while end and not samples[end - 1][1] & TIP:
            end -= 1
        start = end
        while start and samples[start - 1][1] & TIP:
            start -= 1
        if start == end:
            return None
        self.drawing = end == len(samples)  # tip still down: more to come
        return [s[2:5] for s in samples[start:end]]

    def more(self):
        """Samples that arrived since, up to the tip lifting."""
        out = []
        for sample in self._pull():
            if not self.drawing:
                break
            if sample[1] & TIP:
                out.append(sample[2:5])
            else:
                self.drawing = False
        return out


def thickness(width):
    """Position 0..1 on the thickness slider for a pen width. Squared: the
    thin end, where signatures are written, gets most of the slider."""
    return math.sqrt(max(0.0, min(1.0, (width - MIN_WIDTH) / (MAX_WIDTH - MIN_WIDTH))))


def pen_width(position):
    """Pen width for a position 0..1 on the thickness slider."""
    return MIN_WIDTH + (MAX_WIDTH - MIN_WIDTH) * position ** 2


def load_pen(path):
    """(colour, width, smoothing) last chosen for the pen."""
    try:
        with open(path) as f:
            data = json.load(f)
        color = tuple(float(c) for c in data["color"])
        width = float(data["width"])
        smoothing = round(float(data.get("smoothing", SMOOTHING)))
        if len(color) == 3 and all(0 <= c <= 1 for c in color):
            return color, max(MIN_WIDTH, min(MAX_WIDTH, width)), max(0, min(LEVELS, smoothing))
    except (OSError, ValueError, TypeError, KeyError, AttributeError):
        pass
    return INK, PEN_WIDTH, SMOOTHING


def save_pen(path, color, width, smoothing):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".part"
    with open(tmp, "w") as f:
        json.dump({"color": list(color), "width": width, "smoothing": smoothing}, f)
    os.replace(tmp, path)


def normalize(strokes):
    """Strokes as a size-free signature: top-left at 0,0, width 1."""
    x0 = min(s.bounds()[0] for s in strokes)
    y0 = min(s.bounds()[1] for s in strokes)
    width = max(s.bounds()[2] for s in strokes) - x0 or 1.0
    return {"width": width,
            "strokes": [[[(x - x0) / width, (y - y0) / width, p] for x, y, p in s.points]
                        for s in strokes]}


def place(signature, x, y, width=SIGNATURE_WIDTH, pen=PEN_WIDTH, color=INK):
    """Strokes of a normalized signature with its left/vertical centre at x,
    y, drawn with the pen's current width and colour."""
    height = max((pt[1] for s in signature["strokes"] for pt in s), default=0) * width
    # Pen width scales with the signature so a small one does not look bold.
    scaled = pen * width / signature.get("width", width)
    return [Stroke([(x + sx * width, y - height / 2 + sy * width, p) for sx, sy, p in s],
                   width=max(MIN_WIDTH, min(scaled, pen * 1.5)), color=color)
            for s in signature["strokes"]]


def touches(mark, a, b, radius):
    """Whether an eraser of this radius going from a to b (page points)
    touches mark."""
    if isinstance(mark, Text):
        # Rough box around the text: glyphs average about half an em.
        x0, x1 = mark.x - radius, mark.x + 0.55 * mark.size * len(mark.text) + radius
        y0, y1 = mark.y - mark.size - radius, mark.y + 0.25 * mark.size + radius
        return any(x0 <= x <= x1 and y0 <= y <= y1 for x, y in (a, b))
    pts = [tuple(pt[:2]) for pt in mark.points]
    reach = radius + mark.width / 2
    if len(pts) == 1:
        return _to_segment(pts[0], a, b) <= reach
    return any(_between_segments(p, q, a, b) <= reach for p, q in zip(pts, pts[1:]))


def _to_segment(pt, a, b):
    dx, dy = b[0] - a[0], b[1] - a[1]
    length = dx * dx + dy * dy
    t = 0.0 if not length else max(0.0, min(1.0, ((pt[0] - a[0]) * dx + (pt[1] - a[1]) * dy) / length))
    return math.dist(pt, (a[0] + t * dx, a[1] + t * dy))


def _side(a, b, c):
    return (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])


def _between_segments(p, q, a, b):
    if _side(a, b, p) * _side(a, b, q) < 0 and _side(p, q, a) * _side(p, q, b) < 0:
        return 0.0  # they cross
    return min(_to_segment(p, a, b), _to_segment(q, a, b), _to_segment(a, p, q), _to_segment(b, p, q))


def load_signature(path):
    try:
        with open(path) as f:
            data = json.load(f)
        return data if data.get("strokes") else None
    except (OSError, ValueError):
        return None


def save_signature(path, strokes):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".part"
    with open(tmp, "w") as f:
        json.dump(normalize(strokes), f)
    os.replace(tmp, path)
