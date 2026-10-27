# Plus One in Rust (the rust branch)

The PC's side, ported to Rust a part at a time, to see what it gains. The
wire protocol does not change: the tablet's app (Kotlin) works with either.

## Why

Python's threads share one lock (the GIL). Plus One sends the desktop,
plays the speakers and takes the fingers and the pen at once, each from a
thread of its own, and every packet goes through Python. Measured on this
PC (i9-13900H), sealing and sending 1472-byte packets as Plus One does
(AES-256-GCM, UDP to a receiver here):

| Threads | Python | Python, processes, GSO | C++ / Rust, `sendto` | C++ / Rust, GSO |
|---|---|---|---|---|
| 1 | 124 kpkt/s | 160 | 651 / 657 | 1865 / 2024 |
| 4 | **42** | 520 | 2140 / 2229 | 5995 / 6065 |
| 8 | **37** | 802 | 3655 / 3672 | 9552 / 9946 |

More Python threads go slower (8 to 53 µs of CPU per packet): they wait
for the lock. One frame sealed and sent at 60 fps, median (99th
percentile):

| Frame | Python, 1 thread | Python, 4 threads | Rust, GSO, 1 thread |
|---|---|---|---|
| 60 KB (a P-frame) | 0.54 ms (1.3) | 1.1 ms (2.5) | 0.16 ms (0.3) |
| 400 KB (a keyframe) | 2.3 ms (4.3) | 6.3 ms (14.7) | 0.28 ms (0.5) |
| 1 MB (a 4K keyframe) | 5.6 ms (8.4) | 14 ms (20) | 0.50 ms (0.8) |

Rust and C++ are alike (the same OpenSSL, the same system calls); Rust is
the safer of the two for code that takes packets from the network.

## Build

`tools/rust-docker.sh fetch` downloads the crates `rust/Cargo.lock` pins
(only downloads: nothing runs); `tools/rust-docker.sh build --release`,
`test --release` build and test with no network at all, in Ubuntu 24.04
with its own Rust (1.85) and -dev libraries (Dockerfile.rust). The binary,
`rust/target/release/plus-one-rs`, runs on the host.

## Ported so far

| Part | Python | Rust | State |
|---|---|---|---|
| Discovery, trust, links, datagrams | `plusone/link/` | `rust/src/link/` | done; the same identity and pairings |
| The desktop: monitor, stream, rate, fingers, pen | `appdisplay.py`, `display.py`, `media.py`, `pen.py` | `rust/src/display/` | done; `plus-one-rs display` |
| Speakers | `pcm.py`, `devices.py` | `rust/src/pcm.rs`, `rust/src/devices/` | done; `plus-one-rs speakers` |
| Camera, microphone | `devices.py`, `rtc.py` | | |
| Signing | `signing.py`, `sign_jobs.py`, `sign.py`, `pdf.py`, `ink.py`... | | |
| The app: service, D-Bus, notifications, updates | `app.py`, `updates.py`, `engine.py` | | |
| The window, translations | `gui.py`, `i18n.py` | | |
| Packages: .deb (amd64), Flatpak, CI | `packaging/` | | |

## Measured: the desktop

`plus-one-rs fake-app` is a tablet of pretend on this PC: it answers as
the app does, takes the desktop's packets, checks and puts each frame back
together and reports each second, so the PC's rate control runs as with
the tablet. Same machine, same clock: it measures each frame from its
capture to its last packet in, with no Wi-Fi in between. Each side runs
against it with a test pattern (`--test`), 20 s, 1920x1200 at 60 fps,
VA's low-power encoder:

| Bitrate | Python: p50 / p90 / p99 | Rust | Python: CPU per frame | Rust |
|---|---|---|---|---|
| 20 Mbit/s | 9.65 / 10.16 / 12.06 ms | 9.11 / 9.74 / 11.93 | 11.6 ms | 10.7 |
| 60 Mbit/s | 10.57 / 11.77 / 13.76 | 9.88 / 10.97 / 15.83 | 12.6 | 11.8 |
| 100 Mbit/s | 10.96 / 11.61 / 13.57 | 10.32 / 11.05 / 13.29 | 13.1 | 12.2 |

Rust is ahead at each load, by 0.5-0.7 ms a frame and about 7 % of the
CPU: the test pattern itself (a clock drawn on 1920x1200 sixty times a
second, by the CPU) takes most of it, alike on both sides. Larger sizes
measure the pattern, not the sending: it does not keep up at 4K (25 fps).

On the tablet, over a phone's hotspot: the same delay from capture to
decoded on both (26 ms with the pattern, 29.5 ms with the real desktop):
capture, encoder, Wi-Fi and the tablet's decoder decide it. The real
desktop, which the GPU captures: under 2 % of a core for the whole
process, 5.3 ms from capture to sent (Python: 5.7-6.5).

## Measured: the desktop and the speakers at once

`tools/bench/av.sh` sends a test pattern and a test tone to the fake
tablet at once, with the threads the app has for them (the desktop's
stream and its listener, the speakers' stream and theirs, a link held for
each). The fake tablet times each 5 ms packet of sound against when it
was due, while the frames go out, 20 s each:

| | Python, 20 Mbit/s | Rust | Python, 100 Mbit/s | Rust |
|---|---|---|---|---|
| Sound late, p99 | 0.38 ms | 0.27 | 0.38 | 0.24 |
| Sound late, p99.9 / most | 1.09 / 1.39 | 0.94 / 1.36 | 1.09 / 1.75 | 0.72 / 1.43 |
| Sound: gaps over 20 ms | 0 | 0 | 0 | 0 |
| Frames: capture to in, p50 / p99 | 9.36 / 11.68 ms | 10.06 / 12.57 | 10.88 / 14.35 | 11.35 / 17.11 |
| CPU in 20 s | 14.1 s | 14.5 | 16.2 | 16.7 |

The sound is a little more even in Rust, and in Python within 1-2 ms of
when it was due anyway: the tablet waits 40 ms before it plays, nobody
hears the difference. The frames are alike (runs differ by ±0.5 ms; in
the run before, Rust was the faster). The wait for Python's lock does not
show at these loads: the heavy work (capture, VA's encoder, OpenSSL) is C
also when Python calls it, and what Python itself does is little.

So far, then: Rust sends packets far faster, but the packets are not
where the time goes. What it buys at Plus One's loads is not speed but a
single language with types, and no lock between the parts.
