import gi

gi.require_version("NM", "1.0")
from gi.repository import GLib  # noqa: E402

from plusone.p2p import WFD_IES, is_wfd_sink  # noqa: E402


class Peer:
    def __init__(self, ies):
        self.ies = ies

    def get_wfd_ies(self):
        return GLib.Bytes.new(self.ies) if self.ies is not None else None


def test_samsung_second_screen_is_a_sink():
    # WFD IE advertised by a Galaxy Tab running Second Screen.
    assert is_wfd_sink(Peer(bytes.fromhex("0000060111c0060028")))


def test_sources_and_plain_peers_are_not_sinks():
    assert not is_wfd_sink(Peer(WFD_IES))      # our own source IE
    assert not is_wfd_sink(Peer(None))
    assert not is_wfd_sink(Peer(b""))


class Device:
    def __init__(self, state):
        self.state = state

    def get_state(self):
        return self.state


def link_in(state, connected=False):
    from plusone.p2p import P2PLink
    link = P2PLink.__new__(P2PLink)
    link.stopped, link.connected = False, connected
    link.device = Device(state)
    link.gave_up = False
    link.on_give_up = lambda: setattr(link, "gave_up", True)
    return link


def test_gives_up_when_idle():
    from gi.repository import NM
    link = link_in(NM.DeviceState.DISCONNECTED)
    assert link._give_up() is False and link.gave_up


def test_never_gives_up_during_an_attempt():
    from gi.repository import NM
    for state in (NM.DeviceState.CONFIG, NM.DeviceState.ACTIVATED):
        link = link_in(state)
        assert link._give_up() is True and not link.gave_up  # checks again later


def test_no_give_up_once_connected():
    from gi.repository import NM
    link = link_in(NM.DeviceState.ACTIVATED, connected=True)
    assert link._give_up() is False and not link.gave_up


SINK_IES = bytes.fromhex("0000060111c0060028")


class FakePeer(Peer):
    def __init__(self, path):
        super().__init__(SINK_IES)
        self.path = path

    def get_name(self):
        return "Tab S10 FE+"

    def get_hw_address(self):
        return "56:DD:4F:07:78:78"


class P2PDevice:
    def __init__(self, path, peers=()):
        self.path, self.peers, self.finds = path, list(peers), []

    def get_path(self):
        return None  # the udev path, as for every virtual device

    def get_peers(self):
        return self.peers

    def start_find(self, options, *_args):
        self.finds.append(options.unpack() if options is not None else None)


def searching_link(monkeypatch, device, window=120):
    from plusone import p2p
    monkeypatch.setattr(p2p, "_path", lambda obj: obj.path)
    link = p2p.P2PLink.__new__(p2p.P2PLink)
    link.stopped = link.connected = link._polling = link._reset = False
    link.device, link.peer, link.attempt, link.match = device, None, 0, None
    link.give_up_after = window
    link.found, link.hints = [], 0
    link.on_found = link.found.append
    link.on_hint = lambda: setattr(link, "hints", link.hints + 1)
    return link


def test_other_devices_going_away_do_not_matter(monkeypatch):
    # A docker or VPN interface going away has no udev path either.
    link = searching_link(monkeypatch, P2PDevice("/Devices/171"))
    link._device_removed(None, P2PDevice("/Devices/175"))
    assert link.device is not None


def test_the_find_lasts_as_long_as_the_wait(monkeypatch):
    device = P2PDevice("/Devices/171")
    searching_link(monkeypatch, device, window=120)._search()
    assert device.finds == [{"timeout": 120}]  # NetworkManager stops after 30 s otherwise


def test_a_peer_gone_meanwhile_is_looked_for_again(monkeypatch):
    device = P2PDevice("/Devices/171")
    link = searching_link(monkeypatch, device)
    link.peer = FakePeer("/WifiP2PPeer/15")  # the tablet left; NM dropped it
    assert link._activate() is False
    assert link.peer is None and link.attempt == 0 and len(device.finds) == 1
    device.peers = [FakePeer("/WifiP2PPeer/16")]  # back under a new path
    assert link._poll_peers() is False
    assert link.peer is device.peers[0] and link.found == ["Tab S10 FE+"]


def test_a_failed_activation_looks_again(monkeypatch):
    device = P2PDevice("/Devices/171")
    link = searching_link(monkeypatch, device)
    link.peer = FakePeer("/WifiP2PPeer/15")

    class Client:
        def add_and_activate_connection2_finish(self, res):
            raise GLib.Error("The P2P peer /org/freedesktop/NetworkManager/WifiP2PPeer/15 is unknown")
    link._activated(Client(), None)
    assert link.peer is None and len(device.finds) == 1


def test_after_a_driver_reset_the_user_is_asked_to_reopen_second_screen(monkeypatch):
    device = P2PDevice("/Devices/180", [FakePeer("/WifiP2PPeer/2")])
    link = searching_link(monkeypatch, device)
    link._reset = True
    link._poll_peers()
    assert link.found and link.hints == 1
    link.peer = None
    link._poll_peers()
    assert link.hints == 1  # once
