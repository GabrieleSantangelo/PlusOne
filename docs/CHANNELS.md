# Proposal: one link, many channels (0.8)

Status: the separation is built, with the wire protocol unchanged; the
steps below are not.

Connectivity is now a package of its own on each side, under every
service, which never opens a socket itself:

| Primitive | PC (`plusone/link/`) | App (`link/`) |
|---|---|---|
| Discovery | `discovery.py`: every source as one; `nearby.py` (mDNS) | `Announcer.kt`: `MdnsAnnouncer` |
| Transports | `transport.py`: `Tcp`, targets `(host, port)` or `(kind, ...)` | `Listener.kt`: `TcpListener` |
| Trust | `trust.py`: identity, paired tablets, pairing code | `Identity.kt`, `Paired.kt`, pairing in `LinkServer.kt` |
| Links | `link.py`: `connect`, `request`, `hold` | `LinkServer.kt`; `Link.kt`: `Link`, `Requests`, `Handler` |
| Datagrams | `datagrams.py`: `Datagrams` | `Datagrams.kt` (with a replay window), `Sealed.kt` |
| Messages | `wire.py` | `Wire.kt` |

A new way of finding a tablet is one more discovery source on the PC and
one more announcer in the app; a new way of reaching it, a transport and a
listener; a new service, a `Handler` in the app. The steps below change the
protocol on top of these.

## Why

Each feature carries its own transport today:

| Feature | Control | Data |
|---|---|---|
| Signing | the TLS link (length, JSON header, payload) | the same, one connection per PDF |
| Camera, microphone | the TLS link (SDP offer and answer) | WebRTC (DTLS-SRTP, libwebrtc on the tablet) |
| Desktop in the app | the TLS link (a key, two ports) | UDP sealed with AES-GCM: VIDEO, KEYFRAME, PING, PONG, STATS, INPUT |
| Speakers | the TLS link (a key, two ports) | UDP sealed with AES-GCM: PCM, STATS |
| Second Screen | Wi-Fi Direct, RTSP | RTP / MPEG-TS, UIBC |

So every new feature re-does discovery, the reconnection logic, its
keepalive and liveness rules, its key handover, its stats and its
permissions. The review that preceded this found the same bugs in several
copies, such as a dead link that went unnoticed for 15 minutes. A new
module (files, clipboard, a phone as a scanner) would add one more copy.

## Layers

```
 modules     Desktop · Signing · Camera · Microphone · Speakers · Files · ...
 ─────────── each declares the channels it needs, and its permission ───────────
 media       audio channel (format, jitter, drift, filters)
             video channel (codec, keyframes, stats, effects)
             input channel (touch, pen, keys)
 ─────────────────────────────────────────────────────────────────────────────
 channels    reliable (messages, byte streams)  ·  datagram (sealed UDP)
 ─────────────────────────────────────────────────────────────────────────────
 link        one per paired device: TLS 1.3 control, pinned certificates,
             hello with features, keepalive, liveness, reconnection
 ─────────────────────────────────────────────────────────────────────────────
 paths       home Wi-Fi · Wi-Fi Direct · USB (later) — chosen by the link
```

### Link

There is one link per paired device: `plusone/link/` on the PC and
`link/` in the app (`LinkServer.kt`; the services are its `Handler`s). It
keeps what both already do well:
- pairing, with the commitment, the six-digit code and pinned
  certificates;
- TLS 1.3 with client authentication.

It adds the following:
- **Features in the hello**, for example `{"display": 1, "speakers": 1,
  "files": 1}`, instead of one global `PROTOCOL`. Each module then checks
  its own version, and an older app means that module is missing, not
  "update everything".
- **Liveness in one place**: pings, a `TCP_USER_TIMEOUT`, and a single
  notion of "the device is gone" that every channel's session follows.
- **Paths**: the same link over the home Wi-Fi or a Wi-Fi Direct group,
  whichever is up. See the measurements below.

### Channels

- **Reliable**: messages or a byte stream with a channel id. Control
  traffic is multiplexed on the link's TLS connection. Bulk data, such as a
  PDF or a file, gets a TLS connection of its own, as signing already
  does, so that a big transfer never holds back a ping or an input event
  (TCP head-of-line blocking). QUIC would solve this inside one connection,
  but it costs a dependency on both sides that we do not need yet.
- **Datagram**: one UDP socket per link, packets sealed with AES-GCM.
  - Nonce: the direction plus a counter, as now. The kind byte becomes a
    channel id.
  - The key is fresh for each link session and handed over the TLS link,
    as now. (Python's `ssl` has no TLS exporter, which would otherwise
    derive it.)
  - Replay protection is a sliding window instead of "higher than the
    last", which dropped a reordered packet as a replay (for media, a
    loss). The app has it (`Datagrams.kt`, 1024 packets). The PC keeps
    "higher than the last": the app sends it input as states, and an older
    one must not undo a newer.
  - QoS per channel: DSCP (voice for audio, video for video), and the
    Wi-Fi low-latency lock only while a realtime channel is open and
    shown.

`Datagrams` on both sides (`datagrams.py`, `Datagrams.kt`) are most of the
datagram side already; the app's has the sliding window.

### Media

- **Audio channel** (both directions):
  - the format is negotiated: s16 or s24, 48 kHz, the channels, the frames
    per packet;
  - the receiver handles prefill, drift trimming and loss concealment
    (`PcmPlayback.kt` today);
  - the sender gates silence;
  - **filters** form a chain the user can change per channel. On the PC
    these are GStreamer elements; on the tablet, Android's effects.
  - Examples:
    - noise reduction: DeepFilterNet, or the tablet's libwebrtc;
    - **echo cancellation** when the tablet's speakers and microphone are
      used together (a call), using PipeWire's `module-echo-cancel` (its
      `aec-webrtc` is on this PC). Today that combination echoes.
- **Video channel**:
  - H.264 now, HEVC later;
  - keyframes asked for and rate-limited in one place;
  - the stats that both directions already compute;
  - **effects** as a chain before the encoder on the PC: mirror, crop and
    zoom, colour, sharpen;
  - for the camera, after the decoder and before v4l2loopback;
  - a blurred background needs a segmentation model, run either on the
    tablet before encoding or on the PC.
- **Input channel**: the state-based touch and pen packets of today, plus
  keys.

Camera and microphone may stay on WebRTC behind the same channel API at
first. WebRTC's congestion control and retransmissions are worth keeping
for video over a weak Wi-Fi. Its jitter buffer is what hurt the music, and
that is already gone for the speakers.

### Modules

Each module is a small class on both sides with:
- the channels it opens;
- its permission;
- its UI page.

Permissions become **per PC and per module on the tablet**: "this PC may
use the camera". Today the switches are global. A paired peer gets only
what its user allowed, which also closes the review's point about any
paired peer reaching the camera once the global switch is on.

## Example: the tablet's storage on the PC

1. The app runs a small WebDAV handler over a **reliable data channel**.
   It serves only the folders the user picked (Storage Access Framework),
   never the whole storage.
2. On the PC, Plus One listens on a random port on 127.0.0.1, protected by
   a random password. It tunnels each connection over the link and mounts
   it with gvfs (`dav://`, `gvfsd-dav` is on this PC).
3. Files then shows *Tablet* in its sidebar, and every program reads and
   writes through GVfs. There is no FUSE code of our own and no MTP cable.

The alternative, our own file protocol with a FUSE filesystem, costs a new
dependency (pyfuse3) and code that gvfs already has.

## Measured: Wi-Fi and Wi-Fi Direct at the same time

Both devices support it (2026-10-26):

- **PC**, Intel AX211 (`iw list`):
  - `#{managed} <= 1, #{P2P-client, P2P-GO} <= 1, total <= 3, #channels
    <= 2`;
  - so one Wi-Fi network plus one Wi-Fi Direct group, at the same time,
    on up to two different channels (time-shared).
- **Tablet**, Android 16 (`dumpsys wifi`, `dumpsys wifip2p`):
  - connected to the home network on 5745 MHz while it was the group owner
    of Second Screen's groups, also on 5745 MHz;
  - as group owner it puts the group on the channel of its Wi-Fi
    (single-channel concurrency, the cheap case).

On one channel, both work at full speed, sharing the airtime. On two
channels, the radio hops between them, which costs latency and throughput.
In FINDINGS.md, Wi-Fi Direct was "flaky" when the PC was on a 2.4 GHz
network while the group ran on 5745 MHz, and the AX211 firmware crashed
once on the first join after the PC changed channel. A link that can use
either path should therefore prefer the home Wi-Fi when both devices are
on it, and use Wi-Fi Direct when they are not (away from home, or a guest
network that isolates clients).

## Steps

1. `link.py` / `Link.kt`: the hello with features, liveness, paths. Signing
   moves onto it unchanged (its messages become a reliable channel).
2. The datagram mux: speakers and desktop move onto it (small changes,
   their packets stay the same).
3. The audio and video channel classes with filter and effect chains; echo
   cancellation for speakers plus microphone.
4. Per-PC, per-module permissions in the app.
5. The first new module on the new layers: Files (WebDAV over a data
   channel). A phone as a scanner could follow ("devices with roles").

Each step ships on its own and keeps everything working. Steps 1 and 2
change the protocol, so both sides update together (the hello's features
say what is missing to an older app).
