"""The Wi-Fi's power saving, which delays the tablet: Ubuntu keeps it on
(NetworkManager's wifi.powersave = 3), and the radio sleeps between the
access point's beacons, the Wi-Fi Direct groups' too: 15-30 ms more on a
packet, measured (docs/LINK.md). Plus One turns it off for good when the
user says so, with their password (polkit: the helper,
data/wifi-latency), and on again from the preferences."""
import os
import re
import subprocess
import threading

from gi.repository import GLib

from . import log

CONF = "/etc/NetworkManager/conf.d/plus-one-wifi-latency.conf"
INSTALLED = "/usr/lib/plus-one/wifi-latency"
HERE = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "data", "wifi-latency",
                    "plus-one-wifi-latency")


def low_latency():
    """Whether Plus One turned the power saving off (and it stays so)."""
    return os.path.exists(CONF)


def saving():
    """Whether a Wi-Fi interface up saves power now (iw; False if none, or no iw)."""
    try:
        devices = subprocess.run(["iw", "dev"], capture_output=True, text=True, timeout=5).stdout
        for name in re.findall(r"^\s*Interface (\S+)", devices, re.M):
            state = subprocess.run(["iw", "dev", name, "get", "power_save"], capture_output=True, text=True,
                                   timeout=5).stdout
            if "Power save: on" in state:
                return True
    except (OSError, subprocess.SubprocessError):
        pass
    return False


def set_low_latency(on, done):
    """The power saving off (on: True) or on again, asking the user's password
    (GNOME's dialog, through pkexec); done(ok) on the main loop."""
    helper = INSTALLED if os.path.exists(INSTALLED) else HERE

    def run():
        try:
            ok = subprocess.run(["pkexec", helper, "on" if on else "off"], timeout=300).returncode == 0
        except (OSError, subprocess.SubprocessError) as e:
            log("wifi:", repr(e))
            ok = False
        log(f"wifi: low latency {'on' if on else 'off'}" if ok else "wifi: not changed (no password, or no helper)")
        GLib.idle_add(lambda: done(ok) and False)

    threading.Thread(target=run, name="plusone-wifi", daemon=True).start()
