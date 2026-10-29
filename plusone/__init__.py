"""Plus One: a Samsung Galaxy Tab, with Plus One's app on it, as a second
screen, a signing tablet, a webcam, a microphone and speakers for GNOME."""
import json
import os
import tempfile
import time

__version__ = "0.10.0"

_handlers = []


def log(*parts):
    line = " ".join(str(p) for p in parts)
    print(time.strftime("%H:%M:%S"), line, flush=True)
    for handler in _handlers:
        handler(line)


def once(fn, *args):
    """Wrap fn for GLib.idle_add / timeout_add so it runs a single time."""
    def run():
        fn(*args)
        return False
    return run


def write_private(path, data, indent=None):
    """Writes data (JSON-able) to path as a whole, replacing it at once, for
    this user only (the directory too): signatures are handwriting."""
    directory = os.path.dirname(path)
    os.makedirs(directory, mode=0o700, exist_ok=True)
    os.chmod(directory, 0o700)
    # A name of its own (0600): two threads saving the same file shared one.
    fd, tmp = tempfile.mkstemp(prefix=os.path.basename(path) + ".", suffix=".part", dir=directory)
    try:
        with os.fdopen(fd, "w") as f:
            json.dump(data, f, indent=indent)
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def add_log_handler(handler):
    """handler(line) is called from whichever thread logged; marshal as needed."""
    _handlers.append(handler)

