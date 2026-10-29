# The link: connection apart from the features

Status (branch `link`): the core, plus-one-link, plusone_link and the
core in the app work, measured with the tablet; the desktop, the
speakers, the camera and the microphone go over it when plus-one-link
runs, by cable too (USB), moving between the cable and the Wi-Fi without
ending. Not yet: signing and updates (they need bulk channels), Bluetooth.

The connection to the tablet becomes a layer of its own, in Rust, the same
code on both sides: on the PC a program of its own (`plus-one-link`), in
the app a library under it (JNI). It finds the paired tablet over every
way there is, keeps one session with it over the best path that works,
moves to another when that one goes, and gives the features (desktop,
speakers, camera, microphone, signing, and the next ones) channels through
one API. A feature never knows the path; a path never knows the features.

Not for speed: Python with every channel at once measured as fast as Rust
(docs on the `rust` branch, RUST.md). For one implementation of the
protocol on both sides instead of two, and a clean line between the
connection and the rest.

```
 features     Desktop · Speakers · Camera · Microphone · Signing · ...
 ──────────── client libraries: Python (plusone_link), Kotlin; any language
              that speaks the socket's JSON and takes file descriptors ────
 API          the PC: a Unix socket of plus-one-link, a channel = a file
              descriptor; the app: the same through JNI
 ─────────────────────────────────────────────────────────────────────────
 channels     control · input · realtime · bulk
 session      one per paired device: TLS 1.3, pinned certificates, keys
              from the TLS exporter, liveness, features in the hello
 paths        USB · Wi-Fi Direct / the PC's access point · the same
              network · Bluetooth · (a relay)
 discovery    the cable · mDNS · Bluetooth LE · the last addresses · peers
```

## Paths, best first

| Path | Found by | Expect | Notes |
|---|---|---|---|
| USB (Android Open Accessory, as Android Auto by cable) | the cable plugged in (udev; the app's accessory intent) | ~1 ms, tens of MB/s | charges the tablet; Android asks once "open Plus One for this accessory" |
| Wi-Fi Direct, or the PC's own access point (a second adapter) | Wi-Fi Direct's peers; the app's local-only network request | one hop through the air | this PC's AX211 crashed with Wi-Fi Direct on its own Wi-Fi's channel with firmware 89.123cf747 (Ubuntu 24.04's), not with 89.b20dcd1f; one radio for both costs half the speed or more |
| The same network (home Wi-Fi, a hotspot, Ethernet) | mDNS, Bluetooth LE, the last addresses | two hops (through the access point) | blocked where clients are isolated (eduroam, guest networks) |
| Bluetooth (L2CAP or RFCOMM) | Bluetooth LE, bonded devices | 1-2 Mbit/s, tens of ms | the pen, signing, sound; the desktop only small and slow |
| A relay over the internet (later) | an account on a server | the internet's | needs a server: not before the rest |

The session tries what it has candidates for at once, the better ones
first by a few hundred ms (as Happy Eyeballs), uses the best that answers,
and keeps at most one other ready (power: a Bluetooth link kept up for
nothing costs both batteries). A better path showing up (the cable plugged
in) takes over; the active one going (the Wi-Fi gone) hands over to the
next. Each path's round trip is measured all along (its pings).

## Discovery, with or without multicast

Every source feeds one list of candidates per paired device:
- **the cable**: the PC sees a USB device it can switch to accessory mode;
- **mDNS**, where the network lets multicast through. The app's name on
  the network is no one's ("Plus One" and a few letters new each start),
  and its TXT record holds a tag for each paired PC instead of its id:
  SHA-256(the app's fingerprint ‖ the PC's ‖ the quarter of an hour)[:8]
  (`tags.py`, `Tags.kt`, `mdns.rs`, one test vector for the three). Only
  that PC recognises it, nobody can follow the tablet from one quarter to
  the next, nor learn which PCs it is paired with; its version comes over
  the link, authenticated. While the user holds pairing open on the
  tablet, its id and name are there too, for a new PC to list it;
- **Bluetooth LE**: the app advertises a service with an id that changes
  every 15 minutes, HMAC(the pairing's key, the time slot), its IPv4
  address and port and which paths it has. Only a PC paired with it can
  tell whose it is; nobody can follow the tablet from one slot to the
  next. The PC scans only while it looks for a tablet (BlueZ, passive);
- **the last addresses** each side was reached at, tried directly, both
  ways (each side listens);
- **Wi-Fi Direct's peers**.

On eduroam multicast is blocked, and clients usually cannot reach each
other at all: Bluetooth LE still finds the tablet there, and the session
goes over the cable, Wi-Fi Direct or Bluetooth instead.

## Session

- **One per paired device.** Identities as now: the PC's key and
  certificate in `~/.config/plus-one`; the app's P-256 key in Android's
  keystore, never out of it (the core asks Kotlin to sign, once per
  handshake). Pairing as now (the commitment, six digits, pinned
  certificates): pairings made before carry over.
- **The first path: TLS 1.3** (rustls), both sides' certificates checked
  against the paired ones. Its exporter gives the session's secrets: the
  datagram keys, one per direction, and the token other paths join with.
  No key is ever sent in a message.
- **Every handshake whole**: no session resumed, no tickets, no early
  data, so a device forgotten is refused at its next handshake and a
  stolen resumption secret stands for no one. Only ECDSA P-256 with
  SHA-256 signs, against the whole pinned certificate. The key exchange
  is the hybrid post-quantum X25519MLKEM768 (rustls on aws-lc-rs), X25519
  alone with an app or a PC from before it (TLS 1.3 signs the choice).
- **Who may start a handshake** (`gate.rs`, TCP and QUIC together): 8 at
  once, 2 from one address; new ones 4 at once then 1 a second from an
  address, 16 then 8 a second from all. Anyone on the network can try,
  and each try costs the tablet a keystore signature.
- **A QUIC path** (`quic.rs`) is dialled with the same certificates.
  Its connection IDs carry a 4-byte HMAC under the endpoint's key: a
  packet for no connection we know whose ID fails it is dropped, and
  nothing goes back (no stateless reset to strangers). Over 2 handshakes
  at once, an address answers a Retry first. One stream, the session's. Keys change within the first
  1,000 packets, then before the AEAD's limit (2²³ packets for AES-GCM),
  and the connection ends at its integrity limit (quinn).
- **More paths join** with a token (an HMAC of the session's secret and a
  fresh number): no second handshake, no signing.
- **The hello** says the features each side has, each with its version
  (`{"display": 2, "speakers": 1, ...}`): an older app means a feature
  missing, not "update everything".
- **Liveness in one place**: a ping each second on the active path; three
  missed and the path is gone; no path left and the device is gone, for
  every channel at once.

## Channels

Either side opens one, naming the feature and the channel
(`display/video`); the other side's handler for that feature takes it or
refuses it (not served, not allowed). Four classes:

| Class | Delivery | On an IP path | On a stream (USB, Bluetooth) |
|---|---|---|---|
| control | reliable, ordered messages | the session's TLS stream | frames, first |
| input | unreliable; the newest state counts, never queued; up to 1380 bytes | UDP or QUIC datagrams | frames, before any media; one waiting at most |
| realtime (sound, video) | unreliable; late is lost; up to 1380 bytes | UDP or QUIC datagrams | frames; dropped while the stream's queue is past its budget |
| bulk (a PDF, a file, an update) | a reliable byte stream, last in line | a TCP connection of its own, joined to the session | frames, in what the others leave |

- Datagrams: AES-256-GCM, the nonce the direction and a counter, the
  channel inside the sealed part. Replays: a window of 1024 for realtime;
  input only newer than the newest (an older state must not undo a newer).
- Reliable frames are numbered as they leave, not as they are queued, so
  the queue may put some ahead: the session's own messages (opening a
  channel and its answer, credits) go first; the channels go in turn, so
  a message on one waits behind no other's transfer (on a 100 KB/s path,
  opening a channel and a round trip on it beside a transfer took 3.3 s,
  now 1 s: what is left is the transfer's whole messages already on their
  way); a channel's close goes after its own messages, "bye" after all.
  At most 64 channels at once, each side's.
- A path change keeps the channels: control resends what the other side
  had not acknowledged; realtime and input go on (a video channel's
  feature hears "path changed" and asks for a keyframe); bulk starts over.
- Each channel's bytes in and out, losses and the path under it, for the
  features' stats and the UI.

## The API

### The PC: plus-one-link

A user service (systemd, started by its socket: nothing runs until an app
asks). Its socket: `$XDG_RUNTIME_DIR/plus-one/link`, `SOCK_SEQPACKET`,
only for the same user (`SO_PEERCRED`). Each message is a JSON object;
a reply that opens a channel carries its file descriptor (`SCM_RIGHTS`).

| Request | Answer |
|---|---|
| `{"op": "devices"}` | `{"devices": [{"id", "name", "paired", "features", "paths": [{"kind", "state", "rtt_ms"}]}]}` |
| `{"op": "open", "device", "feature", "channel", "class"}` | `{"ok": true}` and the channel's descriptor, or `{"error"}` |
| `{"op": "serve", "feature"}` | channels the device opens for this feature, each as `{"incoming", "device", "channel", "class"}` with its descriptor |
| `{"op": "watch"}` | events from then on: a device up or gone, a path changed, a channel closed |
| `{"op": "pair", ...}`, `{"op": "forget", "device"}` | pairing, as the app's page does it now |

A channel's descriptor: `SOCK_SEQPACKET` for control, input and realtime
(one read, one message; one write, one message), `SOCK_STREAM` for bulk.
Whatever reads and writes a file descriptor uses it: Python, GStreamer,
C. Closing it closes the channel.

### Client libraries

The socket is the contract; the libraries only spare each feature its
details:

- **Python** (`plusone_link`, no dependencies):
  ```python
  from plusone_link import Link, REALTIME
  link = Link()                                   # plus-one-link's socket
  tablet = link.devices()[0]
  video = link.open(tablet, "display", "video", REALTIME)
  video.send(packet); data = video.recv(timeout=0.1); video.fileno()
  link.serve("speakers", on_channel)              # channels the app opens
  link.watch(on_event)                            # devices, paths
  ```
- **Kotlin**, in the app: the same calls on the core through JNI; a
  channel is a `ParcelFileDescriptor`, read and written with `Os.read` and
  `Os.write` on direct buffers.
- Others when a feature needs one: the socket's JSON and file descriptors
  are all there is to it.

### The app

The core runs in the app's process. What only Java can do stays in
Kotlin, each a small adapter handing the core a file descriptor or a byte
pipe: the USB accessory, Bluetooth sockets, Wi-Fi Direct and local-only
networks (`Network.bindSocket`), mDNS and Bluetooth LE advertising, and
signing with the keystore's key.

## Next to the link of now

- The old link (`plusone/link/`, `LinkServer.kt`) stays while features
  move over, each in its turn; the new one listens on a port of its own
  and announces `_plusone-link._tcp`.
- The app keeps answering the old `update` request for good: any older PC
  can still bring it up to date.
- Desktop and speakers first: their packets carry over as they are (the
  datagram kinds become channels). Then signing; then camera and
  microphone, which leave WebRTC for their own realtime channels (WebRTC
  needs an IP path, the cable and Bluetooth are not one).

## Flow control, and where its numbers come from

Every flow is congestion controlled. On a QUIC path (`quic.rs`) one
controller (`cc.rs`) holds the rate for everything on it; realtime goes
first in each packet, and the bulk gets what the realtime leaves. On a TCP
path the kernel's controller holds the stream, and `pace.rs` slows the
bulk down while realtime goes (its datagrams' own control: to come).

The aim: from about 15 Mbit/s up (a home ADSL or Wi-Fi, a phone's hotspot,
Wi-Fi Direct) everything goes perfectly; under 5 Mbit/s it degrades
gracefully (less quality, no stall). Each number below is one of three
kinds: from a published algorithm, measured while running, or chosen here
(and checked on `tools/netbench-grid.sh`'s networks, kept only if it holds
on all of them).

| Number | Value | Kind |
|---|---|---|
| Packet groups (`cc.rs` `GROUP`) | 5 ms | published: GCC (draft-ietf-rmcat-gcc-02) |
| Trendline window, smoothing, gain | 20 groups, 0.9, 4 | published: WebRTC's `TrendlineEstimator` |
| Overuse threshold | 12.5 ms at first, 6-600 ms, k_up 0.0087, k_down 0.039 | published: GCC (adapts to the path's jitter while running) |
| Overuse lasting | 20 ms | chosen (WebRTC's 10 ms cut on Wi-Fi's bursts) |
| Cut | 0.85 of the rate acknowledged | published: GCC |
| Increase | 8 % a second; near the last cut a packet a round trip (and 100 ms), at least 5 % a second | published: GCC; the 5 % floor chosen |
| Never above | 1.5 × acknowledged + 10 KB/s, going up; never down for it | published: WebRTC (`AimdRateControl`); applied going down too, a still screen took the rate, and the screen with it, to the least in seconds |
| Cuts apart | a round trip, 100-200 ms | published: WebRTC's `TimeToReduceFurther` (10-200 ms); the 100 ms floor chosen (what is acknowledged shows a cut only later: it is measured over 500 ms) |
| A cut on a stale measurement | at most 30 % | chosen |
| Rate acknowledged, over | 500 ms | published: WebRTC's acknowledged bitrate estimator starts on 500 ms; measured: over 200 ms a keyframe filled it, the rate allowed 3.3 times what went through on a hotspot's replay (2.3 over 500 ms); on the emulated grid a fixed 4 Mbit/s video beside a download 65 → 99 % of frames whole, nothing worse over 10 runs of the steps |
| Loss | past 10 %: cut by half the loss | published: GCC's loss-based controller |
| Queue standing | past 20 ms (least round trip of 500 ms over the least of 10 s): cut; only with packets in 6 of its 10 buckets of 50 ms | the design's emergency cut (architecture §3.11); the windows chosen; idle, a Wi-Fi asleep lengthened the keepalives' round trips: 17 cuts to the least |
| Window | rate × (round trip + 5 ms), 4 packets at least | chosen; QUIC's least is 2 (RFC 9002) |
| Start | 10 Mbit/s, doubling a round trip until the first overuse | chosen |
| MTU on a local network | 1452 bytes, from the first packet, never less | derived: 1500 less IPv6 and UDP; never less measured: quinn's black hole detection read a full queue's losses as a smaller path, 1200 bytes for a minute, and every full realtime datagram was dropped (video 0 % whole on an emulated ADSL) |
| Realtime late (`quic.rs` `LATE`) | 100 ms | chosen; to be each channel's own deadline |
| Bulk handed ahead of the rate | 30 ms (32 KiB at least) | chosen; as `TCP_NOTSENT_LOWAT` on TCP |
| Bulk's share | 1.25 × rate − 1.2 × realtime's | chosen |
| Acknowledgements asked | every 2 packets, within 2 ms | chosen (QUIC's default is 25 ms) |
| TCP bulk pace (`pace.rs`) | 8 ms of queue, 32 KiB/s at least | measured on a hotspot; the floor lowered on an emulated 3 Mbit/s Wi-Fi |
| Budget told (`cc.rs` `PUBLISH_CHANGE`, `PUBLISH_EVERY`) | past 10 % of the last told; going up at most every 100 ms, a cut at once; `{"event": "budget"}` to the apps, null on a path with no controller of its own | the design's (architecture §3.11); a condition variable, not a poll: nothing wakes while it holds |
| Bluetooth's budget (`session.rs` `Drain`, `BT_PRIOR`, `DRAIN_SPAN`) | 0.9 Mbit/s until its stream is measured full; then what it drained over the last second of 200 ms windows one after the other, each full from start to end with more waiting all along (the socket's room filling is no rate), a window ended only with the socket full or nothing waiting; never more than that, told as above | chosen: RFCOMM carries 1-2 Mbit/s; under it the speakers go in Opus (they sent PCM's 1.7 Mbit/s with no budget); derived: a window alone is off by up to half (a bridge's socket holds 8 to 16 KB when full, 25 KB go in 200 ms at 1 Mbit/s); seen: windows ending in a round that had just emptied TLS's buffer locked onto that phase, nothing measured in 2 s of a full stream |
| Screen's bitrate on QUIC (`appdisplay.py`) | 0.85 of the controller's rate, as the budget is told (it read it once a second); down only once it no longer fits in 95 % of the budget (less the others'), up by 15 % at least and 2 s after its last change | chosen; a frame lost asks for a keyframe, it no longer cuts the bitrate (a third each, 20 to 4 Mbit/s in 4 s on a hotspot taking 36); measured: each change of the bitrate is a keyframe (Intel's VA encoders, vah264lpenc and vaapih264enc, start a new sequence), at each cut and at each step up of 5 % before |
| A frame left out before it is encoded (`appdisplay.py` `QUEUE_MS`) | past 60 ms of the budget waiting (on QUIC, in quinn too) | chosen: under the link's 100 ms (`LATE`), past which it drops a frame's packets and the frames after are broken; quinn's were not counted before, and on a hotspot every cut broke frames |

`PLUSONE_LINK_CC_TRACE=1` puts what the controller does in the log four
times a second (rate, what is acknowledged, trend and threshold, state,
queue, its cuts by why); `PLUSONE_LINK_CC_RECORD=FILE` writes every event
it gets (each acknowledgement's times, losses), a real network's to replay
in a test.

## Measured

### The emulated grid, the reference

`tools/netbench-grid.sh 3 6 100` at c7e3127 (2026-10-28), every number in
`docs/netbench/2026-10-28-c7e3127.txt`: 11 networks, 3 runs each, no run
failed. QUIC, against the same commit before the controller's fixes of
that evening (87dcb72 → c7e3127):

| Network | Video, adaptive | Whole frames (video; under upload) | Realtime under bulk, p99 | Channel opens under upload, p99 |
|---|---|---|---|---|
| adsl 15 Mbit/s | 12.3 → 12.5 Mbit/s | 97.5 → 99.4 %; 97.8 → 98.6 % | 128 → 122 ms | 369 → 424 ms |
| long (80 ms) | 11.9 → 17.1 | 95.3 → 99.2; 90.6 → 97.2 | 399 → 366 | 821 → 906 |
| hotspot 50 | 20.5 → 20.8 | 98.1 → 99.4; 97.8 → 97.8 | 76 → 89 | 250 → 202 |
| shallow (15 ms queue) | 11.6 → 13.0 | 94.5 → 89.5; 86.4 → 78.1 | 71 → 89 | 397 → 429 |
| steps (rate a third, 5 s) | 5.9 → 8.1 | 99.4 → 94.2; 94.2 → 91.4 | 139 → 264 | 716 → 714 |
| cross (a TCP download) | 2.6 → 7.5 | 99.4 → 99.7; 86.1 → 99.4 | 91 → 214 | 409 → 923 |
| p2p 300 | 20.8 → 20.8 | 99.7 → 99.7; 75.9 → 77.0 | 29 → 30 | 69 → 67 |

Better where the controller spiralled down (long, cross, adsl); worse
where the queue is small or the rate changes (shallow, steps) and on
realtime beside a download: the stage 6 work (the controller on the
packets' arrival times) starts from here.

On loopback, the flows of the tablet in use at once from Python (video,
1200-byte packets at 20 Mbit/s in 60 bursts a second; sound, a packet each
5 ms; input, 285 a second), each packet from Python's write to the other
end's read (`tools/bench/paths.py old|link 20`, twice each, 2026-10-27):

| | Now: Python seals, straight to UDP | The link: Python writes, plus-one-link seals |
|---|---|---|
| Video p50 / p99 / max | 0.03 / 0.20-0.25 / ~2.0 ms | 0.14 / 0.31-0.34 / 0.4-1.2 ms |
| Sound p50 / p99.9 | 0.10-0.14 / 0.27-0.51 ms | 0.11 / 0.36-0.37 ms |
| Input p50 / p99.9 | 0.09-0.10 / 0.29-0.30 ms | 0.11-0.13 / 0.37-0.41 ms |
| The PC's CPU in 20 s | 0.98-1.03 s | 0.85-0.92 s (half of it plus-one-link's) |
| Lost | 0 | 0 |

The link costs about 0.1 ms a packet (a thread more to wake on each side)
and less CPU, with lower peaks: nothing anyone sees next to a frame's
16.7 ms and the Wi-Fi's milliseconds. The core alone, Rust at both ends
(`link/core/examples/bench.rs`): 0.1 ms p50, 0.3 ms p99.

With the tablet (2026-10-27): the app's core (link/android in
libplusonelink.so, LinkCore.kt) listens and announces
`_plusone-link._tcp`; plus-one-link reached it with the PC's own
identity and pairing, a session in 158 ms (the TLS handshake signed in
Android's keystore). Round trips on the app's "echo" over the phone's
hotspot (each way twice through the air), against ICMP's 7.1 ms average:

| | Came back | p50 | p90 | p99 |
|---|---|---|---|---|
| Control, 100 bytes, one at a time | 300 of 300 | 10.8 ms | 14.1 | 29.5 |
| Realtime, 200 bytes, 200 a second | 1000 of 1000 | 8.9 ms | 10.6 | 15.3 |
| Realtime, 17 Mbit/s each way | 17994 of 18000 | 12.8 ms | 17.0 | 22.9 |

The desktop over it (2026-10-27): plusone/appdisplay.py opens "display"'s
three channels (control, media, input) when plus-one-link runs and the
app serves it, else the link of before; the packets are the same on both
(LinkPackets, Packets.kt). A test pattern, 1920x1200 at 20 Mbit/s, 60 fps,
about 24 s each, from the PC's capture to the frame decoded on the tablet:

| | Median | 90% | Lost | Max |
|---|---|---|---|---|
| The link | 25.7 / 25.9 ms | 29.3 / 28.8 ms | 0 / 6 | 43 / 161 ms |
| The link of before | 26.3 / 27.5 ms | 30.7 / 30.9 ms | 0 / 0 | 41 / 54 ms |

(The 6 lost and 161 ms: one second of the second run, the hotspot's, it
seems; to be watched.)

Speakers, camera and microphone over it (2026-10-27): devices.Feed asks
plus-one-link first (plusone/link/channels.py: ChannelLink, the link of
before's surface on a control channel); the speakers' sound goes on a
"pcm" channel, the camera and the microphone keep WebRTC for their media.
In the app, LinkFeatures serves each feature with its handler of before
(Link is abstract: SocketLink or ChannelLink). Every channel at once on
the tablet, the desktop through the link: 60 fps, 24-27 ms median, 30-31
ms at 90%, as over the link of before at 14:45 (60 fps, 26-28, 30-31).
The speakers alone, 40 s twice each way: the tablet held 40-80 ms over
either, a few packets missed on both, no gap heard.

By cable (2026-10-27): plus-one-link asks the tablet plugged in to be
Plus One's accessory (Android Open Accessory, through usbfs, no libusb);
Android asks once whether Plus One opens it (UsbActivity); the app then
says it is there with a number of its own, again every 2 s, the PC
answers with the same number, and TLS starts: bytes of an attempt before
are never taken for this one's. Both sides try again as long as the cable
is plugged in; a cable session is never closed for being idle. Round
trips on "echo", the PC on battery:

| | Cable | The hotspot's Wi-Fi (before) |
|---|---|---|
| Control, one at a time: p50 / p99 / max | 3.9 / 5.2 / 5.3 ms | 10.8 / 29.5 / 34.7 ms |
| Realtime, 200 a second | 3.3 / 4.5 / 4.9 ms | 8.9 / 15.3 / 22.3 ms |
| Realtime, 17 Mbit/s each way | 8.0 / 11.5 / 16.8 ms, none lost | 12.8 / 22.9 / 44.2 ms, 6 of 18000 lost |

The desktop (test pattern, 20 Mbit/s, 60 fps), both on battery, against
the home Wi-Fi (2.4 GHz) over the link of before: by cable 27.4-27.9 ms
median from capture to decoded, 34-36 ms at 90%, none lost, the bitrate
kept; the home Wi-Fi 41-52 ms, 81-106 ms, frames lost, the bitrate down
to 2-4 Mbit/s. On battery the PC's encoding is slower (13 ms from capture
to sent, 9 on mains).

Paths changing during a session (2026-10-27; the state machines:
docs/LINK-STATES.md): with the tablet by cable, realtime every 20 ms and
control every 200 ms on "echo", the cable pulled: the session went on over
the Wi-Fi 1.6 s later (realtime's and control's gap), and every one of the
1500 control messages came back, in order; plugged in again, the session
went back to the cable and the Wi-Fi path was let go.

## Phases

1. The core (`link/`): session, channels, keys, liveness over TCP and UDP;
   tests on loopback; the fake tablet on it; measured against now
   (`tools/bench`).
2. `plus-one-link` and its socket; `plusone_link`; the desktop and the
   speakers on it (Python), measured.
3. The core in the app (JNI; the NDK and the Rust target in the Android
   build's container); the desktop and the speakers on it; measured with
   the tablet.
4. USB.
5. Discovery without multicast: the last addresses, Bluetooth LE.
6. Bluetooth.
7. Paths changing during a session; Wi-Fi Direct and the PC's access
   point.
8. Signing, camera and microphone on the link.

Each phase ends measured, with nothing that worked before broken.
