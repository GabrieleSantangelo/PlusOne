import os
import socket
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


def free_port(kind=socket.SOCK_STREAM):
    with socket.socket(socket.AF_INET, kind) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait_for(predicate, timeout=5.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(0.02)
    return False


import pytest  # noqa: E402


@pytest.fixture(autouse=True)
def own_signatures(tmp_path, monkeypatch):
    """No test reads or writes the user's saved signatures."""
    from plusone import signatures
    monkeypatch.setattr(signatures, "PATH", str(tmp_path / "signatures.json"))
    monkeypatch.setattr(signatures, "OLD", str(tmp_path / "signature.json"))


@pytest.fixture(autouse=True)
def own_turns(monkeypatch):
    """A test's PDFs take turns of their own: one that fakes sign_jobs.sign
    leaves no turn unused for the next."""
    from plusone import sign_jobs
    monkeypatch.setattr(sign_jobs, "_turns", sign_jobs._Turns())


@pytest.fixture(autouse=True)
def own_pairings(tmp_path, monkeypatch):
    """No test reads or writes this PC's identity or paired tablets."""
    from plusone.link import trust
    monkeypatch.setattr(trust, "CONFIG", str(tmp_path / "companion"))


@pytest.fixture(autouse=True)
def own_link(tmp_path, monkeypatch):
    """No test reaches the user's plus-one-link (or a tablet through it):
    its socket somewhere nothing listens, unless the test starts one."""
    monkeypatch.setenv("PLUSONE_LINK_SOCKET", str(tmp_path / "no-link"))
