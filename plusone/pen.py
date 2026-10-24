"""S Pen from the tablet: a real Linux tablet device when possible.

Second Screen sends the pen as a UIBC HIDC "Gesture" device: first its HID
report descriptor (digitizer pen, report ID 0x14: tip/barrel/invert/eraser/
in-range bits, X/Y 0..32767, pressure 0..4096, X/Y tilt -90..90), then
10-byte input reports. If /dev/uhid is writable we replay that descriptor
and those reports into a virtual HID device, so the kernel creates a proper
tablet (pressure and tilt reach Krita, GIMP, Xournal++...). Otherwise the pen
drives the pointer through Mutter's remote desktop session.

The reports are also published, all of them, to readers in this process
(`feed`): the signing window draws with those.
"""
import collections
import math
import os
import struct
import threading
import time

from gi.repository import Gio

from . import log

UHID_PATH = "/dev/uhid"
UHID_DESTROY, UHID_CREATE2, UHID_INPUT2 = 1, 11, 12
BUS_VIRTUAL = 0x06
VENDOR, PRODUCT = 0x04E8, 0x5350  # Samsung vendor id, "SP"
DEVICE_NAME = "Plus One S Pen"
TABLET_SCHEMA = "org.gnome.desktop.peripherals.tablet"
BTN_LEFT, BTN_RIGHT = 0x110, 0x111
TIP, BARREL, IN_RANGE = 0x01, 0x02, 0x10
AXIS_MAX = 32767
PRESSURE_MAX = 4096


def parse_report(data):
    """(bits, x, y, pressure) of a pen report, all but bits 0..1; None if
    it is not one."""
    if len(data) < 8 or data[0] != 0x14:
        return None
    bits, x, y, pressure = struct.unpack("<BHHH", data[1:8])
    return bits, x / AXIS_MAX, y / AXIS_MAX, min(1.0, pressure / PRESSURE_MAX)


class Despike:
    """Drops the stray samples Second Screen sends.

    Now and then, in the middle of a stroke, the tablet sends a sample from a
    few reports ahead and then carries on in order (that position comes again
    later): drawn as is, a 4-15 pt needle sticks out of the line. Holding each
    report back by one (3.5 ms) shows it, as the pen jumping away and
    straight back, and that report is dropped.

    Most such samples hop less than that, but their pressure jumps too, away
    and back (by 0.06-0.15 where the pen's own changes stay under 0.02 a
    report): drawn, the line swells for a moment, every few millimetres."""

    JUMP = 0.0015          # of the screen width: about 3 pixels
    PRESSURE_JUMP = 0.04   # of full pressure

    def __init__(self):
        self.held = None    # (report, parsed) waiting for the next report
        self.last = None    # (x, y, pressure) of the last report passed on, tip down
        self.dropped = 0

    def feed(self, data):
        """Reports to pass on, in order."""
        sample = parse_report(data)
        drawing = sample is not None and bool(sample[0] & TIP)
        out = []
        if self.held:
            report, (bits, *here) = self.held
            self.held = None
            if drawing and bits == sample[0] and self.last and self._stray(self.last, here, sample[1:]):
                self.dropped += 1
            else:
                out.append(report)
                self.last = here
        if drawing:
            self.held = (data, sample)
        else:  # hovering or lifted: nothing to hold back
            self.last = None
            out.append(data)
        return out

    @classmethod
    def _stray(cls, before, here, after):
        """Whether `here` (x, y, pressure) leaves the stroke and comes back."""
        a, b = math.dist(before[:2], here[:2]), math.dist(here[:2], after[:2])
        if a > cls.JUMP and b > cls.JUMP and math.dist(before[:2], after[:2]) < 0.3 * min(a, b):
            return True
        up, down = here[2] - before[2], here[2] - after[2]
        jump = min(abs(up), abs(down))
        return up * down > 0 and jump > cls.PRESSURE_JUMP and abs(before[2] - after[2]) < 0.5 * jump


class Feed:
    """Every pen report from the tablet (about 285 a second), for readers in
    this process, as (time, bits, x, y, pressure): they get each one as it
    arrives, a few milliseconds before the desktop delivers it, and whatever
    the desktop merges."""

    def __init__(self):
        self.readers = []

    def listen(self):
        reader = collections.deque(maxlen=4096)
        self.readers = self.readers + [reader]
        return reader

    def unlisten(self, reader):
        self.readers = [r for r in self.readers if r is not reader]

    def publish(self, data):
        readers = self.readers  # called from the UIBC thread
        if readers:
            sample = parse_report(data)
            if sample:
                for reader in readers:
                    reader.append((time.monotonic(),) + sample)


feed = Feed()


def uhid_available():
    return os.access(UHID_PATH, os.R_OK | os.W_OK)


class UhidPen:
    @staticmethod
    def create_event(descriptor):
        """struct uhid_event { u32 type; struct uhid_create2_req } (packed)."""
        return struct.pack("<I128s64s64sHHIIII", UHID_CREATE2, DEVICE_NAME.encode(),
                           b"plus-one", b"", len(descriptor), BUS_VIRTUAL,
                           VENDOR, PRODUCT, 1, 0) + descriptor

    @staticmethod
    def input_event(data):
        """struct uhid_event { u32 type; struct uhid_input2_req { u16 size; data } }."""
        return struct.pack("<IH", UHID_INPUT2, len(data)) + data

    def __init__(self, descriptor):
        self.fd = os.open(UHID_PATH, os.O_RDWR | os.O_CLOEXEC)
        os.write(self.fd, self.create_event(descriptor))
        # The kernel queues start/open notifications; drain them so the queue
        # never fills up.
        threading.Thread(target=self._drain, daemon=True).start()
        log("pen: virtual HID tablet created (pressure and tilt)")

    def _drain(self):
        try:
            while os.read(self.fd, 4380):
                pass
        except OSError:
            pass

    def report(self, data):
        os.write(self.fd, self.input_event(data))

    def close(self):
        try:
            os.write(self.fd, struct.pack("<I", UHID_DESTROY))
        finally:
            os.close(self.fd)


class PointerPen:
    """Fallback: hover moves the pointer, tip = left button, barrel = right."""

    def __init__(self, injector, size):
        self.injector = injector
        self.size = size
        self.buttons = 0
        log("pen: no access to /dev/uhid, the pen drives the pointer (no pressure)")

    def report(self, data):
        if len(data) < 6 or data[0] != 0x14:
            return
        bits = data[1]
        x, y = struct.unpack("<HH", data[2:6])
        if bits & IN_RANGE:
            self.injector.pointer_move(x * self.size[0] / AXIS_MAX, y * self.size[1] / AXIS_MAX)
        for mask, button in ((TIP, BTN_LEFT), (BARREL, BTN_RIGHT)):
            pressed = bool(bits & mask) and bool(bits & IN_RANGE)
            if pressed != bool(self.buttons & mask):
                self.injector.pointer_button(button, pressed)
                self.buttons ^= mask

    def close(self):
        for mask, button in ((TIP, BTN_LEFT), (BARREL, BTN_RIGHT)):
            if self.buttons & mask:
                self.injector.pointer_button(button, False)
        self.buttons = 0


def map_to_monitor(edid):
    """Tell Mutter to map our tablet onto the monitor with this (vendor, product, serial)."""
    if not Gio.SettingsSchemaSource.get_default().lookup(TABLET_SCHEMA, True):
        log("pen: no tablet settings schema, cannot map the pen to the monitor")
        return
    path = f"/org/gnome/desktop/peripherals/tablets/{VENDOR:04x}:{PRODUCT:04x}/"
    settings = Gio.Settings.new_with_path(TABLET_SCHEMA, path)
    settings.set_strv("output", list(edid))
    settings.set_boolean("keep-aspect", False)
    Gio.Settings.sync()
    log("pen: mapped to monitor", edid)
