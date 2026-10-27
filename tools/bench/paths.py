"""The PC's side of the datagrams, now and with the link, on loopback: the
same flows from Python at once (video 1200-byte packets, 20 Mbit/s in 60
bursts a second; sound, a packet each 5 ms; input, 285 a second), each
packet's time from Python's write to the other end's read, and the CPU the
PC's side spent (Python, and plus-one-link for the link).
  python3 tools/bench/paths.py old|link SECONDS [BIN]
old: plusone.link.datagrams seals in Python, straight to UDP; the other end
a Python process of its own. link: plusone_link writes into a channel,
plus-one-link (BIN: link/target/release) seals and sends; the other end
link/core's examples/tablet."""
import hashlib
import json
import multiprocessing
import os
import resource
import socket
import ssl
import struct
import subprocess
import sys
import tempfile
import threading
import time

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, REPO)
FLOWS = [("video", "realtime", 30, 1200, 60), ("sound", "realtime", 1, 980, 200), ("input", "input", 1, 40, 285)]


def stamp(size, index):
    body = bytearray(size)
    struct.pack_into("!q", body, 0, time.clock_gettime_ns(time.CLOCK_MONOTONIC))
    body[8] = index
    return bytes(body)


def run_flows(seconds, send_for):
    """Each flow on a thread of its own, as the features do: {name: sent}."""
    until = time.monotonic() + seconds
    sent = {}

    def flow(index, name, burst, size, rate):
        send, count, due = send_for(name), 0, time.monotonic()
        while time.monotonic() < until:
            for _ in range(burst):
                send(stamp(size, index))
                count += 1
            due += 1 / rate
            time.sleep(max(0, due - time.monotonic()))
        sent[name] = count
    threads = [threading.Thread(target=flow, args=(i, name, burst, size, rate))
               for i, (name, _kind, burst, size, rate) in enumerate(FLOWS)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    return sent


def describe(name, late_ns, sent):
    late_ns.sort()
    at = lambda q: late_ns[min(int(len(late_ns) * q), len(late_ns) - 1)] / 1e6 if late_ns else 0
    print(f"{name:>6}: {len(late_ns)} of {sent}; p50 {at(0.5):.3f} ms, p99 {at(0.99):.3f}, "
          f"p99.9 {at(0.999):.3f}, max {(late_ns[-1] if late_ns else 0) / 1e6:.3f}")


def cpu_of(pid):
    """Seconds of CPU a process spent (user + system)."""
    with open(f"/proc/{pid}/stat") as f:
        fields = f.read().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def own_cpu():
    usage = resource.getrusage(resource.RUSAGE_SELF)
    return usage.ru_utime + usage.ru_stime


# -- now: plusone.link.datagrams -----------------------------------------------------------

def old_receiver(key, ports, results):
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 << 20)
    sock.bind(("127.0.0.1", 0))
    ports.put(sock.getsockname()[1])
    aead, late = AESGCM(key), {name: [] for name, *_ in FLOWS}
    sock.settimeout(1.0)
    while True:
        try:
            packet = sock.recv(2048)
        except socket.timeout:
            break
        body = aead.decrypt(packet[:12], packet[12:], None)
        late[FLOWS[body[8]][0]].append(time.clock_gettime_ns(time.CLOCK_MONOTONIC) - struct.unpack_from("!q", body)[0])
    results.put(late)


def old(seconds):
    from plusone.link import datagrams
    channel = datagrams.Datagrams()
    ports, results = multiprocessing.Queue(), multiprocessing.Queue()
    receiver = multiprocessing.Process(target=old_receiver, args=(channel.key, ports, results))
    receiver.start()
    channel.app = ("127.0.0.1", ports.get())
    cpu = own_cpu()
    sent = run_flows(seconds, lambda name: channel.send)
    cpu = own_cpu() - cpu
    late = results.get()
    receiver.join()
    print(f"{seconds} s, now (Python seals, UDP): the PC's CPU {cpu:.2f} s")
    for name, *_ in FLOWS:
        describe(name, late[name], sent[name])


# -- the link: plusone_link, plus-one-link -------------------------------------------------

def identity(directory):
    os.makedirs(directory, mode=0o700, exist_ok=True)
    cert, key = os.path.join(directory, "cert.pem"), os.path.join(directory, "key.pem")
    subprocess.run(["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
                    "-nodes", "-keyout", key, "-out", cert, "-days", "2", "-subj", "/CN=bench"],
                   check=True, capture_output=True, timeout=30)
    with open(cert) as f:
        return cert, key, hashlib.sha256(ssl.PEM_cert_to_DER_cert(f.read())).hexdigest()


def link(seconds, binaries):
    from plusone_link import Link
    with tempfile.TemporaryDirectory() as work:
        _c, _k, pc_fp = identity(os.path.join(work, "pc"))
        tablet_cert, tablet_key, tablet_fp = identity(os.path.join(work, "tablet"))
        with open(os.path.join(work, "pc", "paired.json"), "w") as f:
            json.dump({tablet_fp: {"name": "bench"}}, f)
        tablet = subprocess.Popen([os.path.join(binaries, "examples", "tablet"), "--cert", tablet_cert,
                                   "--key", tablet_key, "--trust", pc_fp],
                                  stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        port = int(tablet.stdout.readline().split()[1])
        path = os.path.join(work, "run", "link")
        daemon = subprocess.Popen([os.path.join(binaries, "plus-one-link"), "--socket", path,
                                   "--config", os.path.join(work, "pc"), "--state", os.path.join(work, "state.json")],
                                  stderr=subprocess.DEVNULL)
        while not os.path.exists(path):
            time.sleep(0.02)
        connection = Link(path)
        device = connection.connect(f"127.0.0.1:{port}")
        while connection.devices()[0]["rtt_ms"] is None:  # UDP proven
            time.sleep(0.02)
        channels = {name: connection.open(device, "sink", name, kind) for name, kind, *_ in FLOWS}
        cpu, daemon_cpu = own_cpu(), cpu_of(daemon.pid)
        sent = run_flows(seconds, lambda name: channels[name].send)
        cpu, daemon_cpu = own_cpu() - cpu, cpu_of(daemon.pid) - daemon_cpu
        time.sleep(0.3)
        for channel in channels.values():
            channel.close()
        lines = [tablet.stdout.readline() for _ in FLOWS]
        dropped = connection.devices()[0]["dropped"]
        for process in (daemon, tablet):
            process.kill()
            process.wait()
    print(f"{seconds} s, the link (Python writes, plus-one-link seals): the PC's CPU {cpu + daemon_cpu:.2f} s "
          f"(Python {cpu:.2f}, plus-one-link {daemon_cpu:.2f}); dropped {dropped}")
    for name, *_ in FLOWS:
        line = next(line for line in lines if line.startswith(f"sink {name}:"))
        count, rest = line.split(": ", 1)[1].split(" came; ")
        print(f"{name:>6}: {count} of {sent[name]}; {rest.strip()}")


if __name__ == "__main__":
    mode, seconds = sys.argv[1], int(sys.argv[2])
    if mode == "old":
        old(seconds)
    else:
        link(seconds, sys.argv[3] if len(sys.argv) > 3 else os.path.join(REPO, "link", "target", "release"))
