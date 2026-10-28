"""The Wi-Fi's power saving (wifi.py), with iw and pkexec stand-ins on the
PATH: nothing of this PC's is read or changed."""
import os
import time
import types

from gi.repository import GLib

from plusone import wifi


def fake(directory, name, script):
    path = directory / name
    path.write_text("#!/bin/sh\n" + script)
    path.chmod(0o755)


def test_power_saving_is_read_off_each_wifi_interface(tmp_path, monkeypatch):
    fake(tmp_path, "iw", 'case "$*" in\n'
         '"dev") printf "phy#0\\n\\tInterface wlan1\\nphy#1\\n\\tInterface p2p-wlan1-0\\n" ;;\n'
         '"dev wlan1 get power_save") echo "Power save: off" ;;\n'
         '*) echo "Power save: $SAVING" ;;\nesac\n')
    monkeypatch.setenv("PATH", f"{tmp_path}:{os.environ['PATH']}")
    monkeypatch.setenv("SAVING", "on")
    assert wifi.saving()  # the Wi-Fi Direct group's saves
    monkeypatch.setenv("SAVING", "off")
    assert not wifi.saving()


def test_the_helper_runs_through_pkexec_and_says_how_it_went(tmp_path, monkeypatch):
    asked = tmp_path / "asked"
    fake(tmp_path, "pkexec", f'echo "$*" > {asked}\nexit $ANSWER\n')
    monkeypatch.setenv("PATH", f"{tmp_path}:{os.environ['PATH']}")
    for answer, expected in (("0", True), ("126", False)):  # 126: the dialog dismissed
        monkeypatch.setenv("ANSWER", answer)
        done = []
        wifi.set_low_latency(True, done.append)
        deadline = time.monotonic() + 5
        while not done and time.monotonic() < deadline:
            GLib.MainContext.default().iteration(False)
            time.sleep(0.01)
        assert done == [expected]
        assert asked.read_text().split()[-1] == "on" and asked.read_text().split()[0].endswith("wifi-latency")


def test_told_once_at_the_first_start(monkeypatch):
    from plusone import app
    told = []
    fake = types.SimpleNamespace(settings=types.SimpleNamespace(wifi_latency_asked=False), save_settings=lambda: None,
                                 notify=lambda key, title, body, button=None, **args: told.append((title, button)))
    monkeypatch.setattr(wifi, "low_latency", lambda: False)
    monkeypatch.setattr(wifi, "saving", lambda: True)
    for _ in range(2):
        app.PlusOneApp._offer_low_latency(fake)
    deadline = time.monotonic() + 5
    while not told and time.monotonic() < deadline:
        GLib.MainContext.default().iteration(False)
        time.sleep(0.01)
    assert told == [("The Wi-Fi's power saving delays the tablet", ("Turn it off", "app.wifi-latency"))]
    assert fake.settings.wifi_latency_asked
