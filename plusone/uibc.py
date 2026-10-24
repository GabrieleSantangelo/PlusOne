"""WFD UIBC (User Input Back Channel) server: tablet touches -> desktop input.

The sink connects to the TCP port we announce in M4 and sends generic input
events (WFD spec 4.11): a 4-byte header (version/timestamp flag, input
category, total length), an optional 2-byte timestamp, then one or more
generic input elements: ID (1 byte), length (2 bytes), body.
"""
import socket
import struct
import threading

from . import log
from .pen import feed as pen_feed

TOUCH_DOWN, TOUCH_UP, TOUCH_MOVE = 0, 1, 2


class UibcServer:
    def __init__(self, injector, port=50000, pen_factory=None):
        """injector: object with touch_down/touch_motion/touch_up(slot, x, y).

        pen_factory(descriptor) returns an object with report(data)/close()
        for the S Pen, which arrives as an HIDC device.
        """
        self.injector = injector
        self.pen_factory = pen_factory
        self.pen = None
        self.reports = 0
        self.pen_states = set()  # pen state bytes logged so far
        self.port = port
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("0.0.0.0", port))
        self.sock.listen(1)
        self.down = set()
        self.conns = set()  # open sink connections, closed with the server
        threading.Thread(target=self._serve, daemon=True).start()

    def _serve(self):
        while True:
            try:
                conn, addr = self.sock.accept()
            except OSError:
                return
            log("UIBC: sink connected from", addr)
            self.conns.add(conn)
            threading.Thread(target=self._handle, args=(conn, addr), daemon=True).start()

    def _recv(self, conn, n):
        data = b""
        while len(data) < n:
            chunk = conn.recv(n - len(data))
            if not chunk:
                raise EOFError
            data += chunk
        return data

    def _handle(self, conn, addr):
        try:
            self._loop(conn, addr)
        except Exception as e:  # noqa: BLE001 - report and keep the app alive
            log("UIBC: handler crashed:", repr(e))
        finally:
            self.conns.discard(conn)
            conn.close()

    def _loop(self, conn, addr):
        try:
            while True:
                head = self._recv(conn, 4)
                has_ts = head[0] & 0x10
                category = head[1] & 0x0F
                length = struct.unpack(">H", head[2:4])[0]
                body = self._recv(conn, length - 4)
                if has_ts:
                    body = body[2:]
                if category == 0:
                    self._generic(body)
                elif category == 1:
                    self._hidc(body)
                else:
                    log(f"UIBC: ignoring category {category} ({len(body)} bytes)")
        except (EOFError, OSError) as e:
            log("UIBC: sink disconnected", addr, e)
            for slot in list(self.down):
                self.injector.touch_up(slot)
            self.down.clear()
            self._close_pen()

    def _generic(self, body):
        while len(body) >= 3:
            ie_id = body[0]
            ie_len = struct.unpack(">H", body[1:3])[0]
            desc, body = body[3:3 + ie_len], body[3 + ie_len:]
            if ie_id in (TOUCH_DOWN, TOUCH_UP, TOUCH_MOVE) and desc:
                count = desc[0]
                for i in range(count):
                    # Coordinates are in the negotiated mode, i.e. the monitor's.
                    pid, x, y = struct.unpack(">BHH", desc[1 + 5 * i:6 + 5 * i])
                    if ie_id == TOUCH_DOWN:
                        self.down.add(pid)
                        self.injector.touch_down(pid, x, y)
                    elif ie_id == TOUCH_MOVE and pid in self.down:
                        self.injector.touch_motion(pid, x, y)
                    elif ie_id == TOUCH_UP:
                        self.down.discard(pid)
                        self.injector.touch_up(pid)
            else:
                log(f"UIBC: generic event {ie_id} {desc.hex()}")

    def _hidc(self, body):
        # HIDC (WFD 4.11.4): input path, HID type, usage (0 report, 1 descriptor),
        # 2-byte length, then the raw HID data.
        if len(body) < 5:
            return
        hid_type, usage = body[1], body[2]
        length = struct.unpack(">H", body[3:5])[0]
        data = body[5:5 + length]
        if usage == 1:
            log(f"UIBC: HID descriptor for type {hid_type}: {data.hex()}")
            self.pen_states = set()
            self._close_pen()
            if self.pen_factory:
                try:
                    self.pen = self.pen_factory(data)
                except OSError as e:
                    log("pen unavailable:", e)
        elif self.pen:
            self.reports += 1
            if self.reports in (1, 100) or self.reports % 2000 == 0:
                log(f"UIBC: pen report #{self.reports} {data.hex()}")
            # Byte 1 holds tip, side button, eraser and in-range bits on the
            # Samsung pen: log each combination once, to see what a button sends.
            if len(data) > 1 and data[1] not in self.pen_states:
                self.pen_states.add(data[1])
                log(f"UIBC: pen state {data[1]:#04x} in {data.hex()}")
            pen_feed.publish(data)
            self.pen.report(data)
        else:
            log("UIBC: pen report before any descriptor, dropped")

    def _close_pen(self):
        if self.pen:
            self.pen.close()
            self.pen = None

    def close(self):
        self._close_pen()
        # shutdown() wakes the threads blocked in accept() and recv(); close()
        # alone leaves the port listening until the process exits, and a
        # connection from the previous session would linger until TCP gives
        # up on it (about 30 s after the Wi-Fi Direct group goes away).
        for sock in (self.sock, *list(self.conns)):
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        self.sock.close()
