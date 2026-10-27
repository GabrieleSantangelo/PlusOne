"""plusone/link/p2p.py: which radio a Wi-Fi Direct group goes on."""
from plusone.link import p2p


class FakeNM:
    """NetworkManager's devices: path -> {(interface, property): value}."""

    def __init__(self, devices):
        self.devices = devices

    def call(self, _bus, path, iface, method, args=None, reply=None):
        assert method == "GetDevices"
        return (list(self.devices),)

    def get(self, _bus, path, iface, name):
        return self.devices.get(path, {}).get(name)


def nm(monkeypatch, devices, iw=None):
    fake = FakeNM(devices)
    monkeypatch.setattr(p2p, "_call", fake.call)
    monkeypatch.setattr(p2p, "_get", fake.get)
    monkeypatch.setattr(p2p, "_iw_addresses", lambda: iw or {})
    monkeypatch.setattr(p2p, "_phy", lambda interface: {"wlan0": "phy2", "wlan1": "phy3"}.get(interface))
    monkeypatch.delenv("PLUSONE_P2P_SAME_RADIO", raising=False)


ON_A_NETWORK = {"DeviceType": p2p.TYPE_WIFI, "Interface": "wlan0", "State": p2p.DEVICE_ACTIVATED}
FREE = {"DeviceType": p2p.TYPE_WIFI, "Interface": "wlan1", "State": 30}


def test_the_radio_on_a_network_is_not_taken(monkeypatch):
    # One adapter, on the hotspot: a group on it crashed the AX211's firmware.
    nm(monkeypatch, {"/d/1": ON_A_NETWORK, "/d/2": {"DeviceType": p2p.TYPE_WIFI_P2P, "Interface": "p2p-dev-wlan0"}},
       iw={"phy2": "b0:dc:ef:a6:04:83"})
    assert p2p.p2p_device(None) is None
    assert p2p.p2p_device(None, same_radio=True) == ("/d/2", "b0:dc:ef:a6:04:83")


def test_a_second_adapter_is(monkeypatch):
    nm(monkeypatch, {"/d/1": ON_A_NETWORK, "/d/2": {"DeviceType": p2p.TYPE_WIFI_P2P, "Interface": "p2p-dev-wlan0"},
                     "/d/3": FREE, "/d/4": {"DeviceType": p2p.TYPE_WIFI_P2P, "Interface": "p2p-dev-wlan1",
                                            "HwAddress": "AA:BB:CC:00:11:22"}})
    assert p2p.p2p_device(None) == ("/d/4", "aa:bb:cc:00:11:22")


def test_its_address_from_iw_when_networkmanager_has_none(monkeypatch):
    nm(monkeypatch, {"/d/3": FREE, "/d/4": {"DeviceType": p2p.TYPE_WIFI_P2P, "Interface": "p2p-dev-wlan1"}},
       iw={"phy3": "02:11:22:33:44:55"})
    assert p2p.p2p_device(None) == ("/d/4", "02:11:22:33:44:55")


def test_iw_parsed():
    text = """phy#2
\tUnnamed/non-netdev interface
\t\twdev 0x200000002
\t\taddr b0:dc:ef:a6:04:83
\t\ttype P2P-device
\tInterface wlan0
\t\taddr b0:dc:ef:a6:04:82
\t\ttype managed
"""
    import types
    original = p2p.Gio.Subprocess.new
    try:
        p2p.Gio.Subprocess.new = lambda *a: types.SimpleNamespace(communicate_utf8=lambda *b: (True, text, ""))
        assert p2p._iw_addresses() == {"phy2": "b0:dc:ef:a6:04:83"}
    finally:
        p2p.Gio.Subprocess.new = original
