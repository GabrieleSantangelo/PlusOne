"""Discovery over Bluetooth LE: the tablets whose user holds pairing open,
as earphones to be paired show up among the devices nearby. The tablet
advertises who and where it is for the two minutes its window is open
(ble.rs); plus-one-link (BlueZ) hears it, where mDNS does not go: another
network, a guest Wi-Fi, a PC on a cable and a tablet on Wi-Fi.

Only what is looking for a tablet to pair asks (refresh_pairing(), as the
pairing page is open): nothing runs at any other time (power), and a scan
is one at a time, a few seconds. What it hears is as good as what mDNS
says: where to try, and who it claims to be; the code both screens show is
what pairs.
"""
import threading
import time

from .. import log

try:
    import plusone_link
except ImportError:  # the link's client is not installed
    plusone_link = None

SCAN = 3.0   # seconds a scan listens
FRESH = 10   # seconds a tablet heard stays listed after it was last heard


class BleNearby:
    """A discovery source (discovery.py): apps() are the tablets heard."""

    def __init__(self, on_change=None, link=None, clock=time.monotonic):
        """link(): plus-one-link's client, a context manager (plusone_link.Link)."""
        self.on_change = on_change or (lambda: None)
        self._link = link or (plusone_link.Link if plusone_link else None)
        self._clock = clock
        self._found = {}  # app id -> (address, port, name, heard at)
        self._lock = threading.Lock()
        self._scanning = False
        self._complained = False

    def start(self):
        pass  # nothing runs until a tablet to pair is looked for

    def stop(self):
        pass

    def refresh(self):
        pass  # asked where the paired tablets are: they do not advertise like this

    def present(self):
        return False  # not a way to tell that no tablet is there

    def names(self):
        return []

    def app_version(self, _app_id):
        return None

    def apps(self):
        """[(app id, address, port, name)] of the tablets heard lately."""
        with self._lock:
            now = self._clock()
            return [(app_id, address, port, name or f"Tablet {app_id[:4]}")
                    for app_id, (address, port, name, heard) in self._found.items() if now - heard < FRESH]

    def addresses(self, app_id):
        with self._lock:
            found = self._found.get(app_id)
            return [(found[0], found[1])] if found and self._clock() - found[3] < FRESH else []

    def refresh_pairing(self):
        """The user is pairing a tablet: listen for those holding pairing open
        (one scan at a time; its answer comes on a thread of its own)."""
        if self._link is None:
            return
        with self._lock:
            if self._scanning:
                return
            self._scanning = True
        threading.Thread(target=self._scan, name="ble-nearby", daemon=True).start()

    def _scan(self):
        try:
            with self._link() as link:
                tablets = link.pairing_scan(SCAN)
        except (OSError, ValueError) as e:
            self._complain(e)
            tablets = []
        except Exception as e:  # noqa: BLE001 - plusone_link.LinkError, or what a client says
            self._complain(e)
            tablets = []
        finally:
            with self._lock:
                self._scanning = False
        now = self._clock()
        changed = False
        with self._lock:
            for tablet in tablets:
                try:
                    entry = (str(tablet["address"]), int(tablet["port"]), str(tablet.get("name", "")), now)
                    app_id = str(tablet["id"])
                except (KeyError, TypeError, ValueError):
                    continue
                if len(app_id) != 16:
                    continue
                before = self._found.get(app_id)
                self._found[app_id] = entry
                changed = changed or before is None or before[:3] != entry[:3] or now - before[3] >= FRESH
            for app_id in [a for a, e in self._found.items() if now - e[3] >= FRESH]:
                del self._found[app_id]
                changed = True
        if changed:
            self.on_change()

    def _complain(self, error):
        if not self._complained:  # once: a PC with no link or no Bluetooth is no news each second
            self._complained = True
            log("ble nearby:", error)
