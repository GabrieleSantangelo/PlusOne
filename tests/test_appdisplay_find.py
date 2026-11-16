"""The app display finds the tablet's app where it is now, not only where it
was: the network may have just changed, its Wi-Fi may sleep."""
from plusone import appdisplay, link
from plusone.appdisplay import AppDisplay


def display(monkeypatch, targets, find, answers):
    tried = []

    def connect(targets, app_id):
        tried.append(list(targets))
        if targets[0] in answers:
            return "link at %s" % (targets[0][0],)
        raise OSError(113, "No route to host")
    monkeypatch.setattr(link, "connect", connect)
    monkeypatch.setattr(appdisplay, "LOOK_EVERY", 0.01)
    return AppDisplay(targets, "0123456789abcdef", (None, None, 60), 20000, "x264", find=find), tried


def test_where_it_was_answers(monkeypatch):
    shown, tried = display(monkeypatch, [("192.168.1.112", 47236)], lambda: [], {("192.168.1.112", 47236)})
    assert shown._connect() == "link at 192.168.1.112" and len(tried) == 1


def test_not_where_it_was_it_is_looked_for(monkeypatch):
    found = iter([[], [("10.151.253.132", 47236)], [("192.168.1.112", 47236)]])
    shown, tried = display(monkeypatch, [("10.151.253.132", 47236)], lambda: next(found),
                           {("192.168.1.112", 47236)})
    assert shown._connect() == "link at 192.168.1.112"
    assert tried == [[("10.151.253.132", 47236)], [("192.168.1.112", 47236)]]  # the old one not again


def test_nowhere_is_not_found(monkeypatch):
    monkeypatch.setattr(appdisplay, "LOOK", 0.05)
    shown, tried = display(monkeypatch, [], lambda: [], set())
    try:
        shown._connect()
    except link.NotFound:
        pass
    else:
        raise AssertionError("found nowhere")
    assert tried == []


def test_stopped_while_looking_gives_up(monkeypatch):
    shown, _tried = display(monkeypatch, [], lambda: [], set())
    shown._stopped.set()
    try:
        shown._connect()
    except link.NotFound:
        pass
