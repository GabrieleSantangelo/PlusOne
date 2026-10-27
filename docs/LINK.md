# The link: connection apart from the features

Status: phase 1 done (the core); phase 2 in progress: plus-one-link and
plusone_link work, the features not on them yet (branch `link`).

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
| Wi-Fi Direct, or the PC's own access point (a second adapter) | Wi-Fi Direct's peers; the app's local-only network request | one hop through the air | this PC's AX211 (firmware 89) crashes with Wi-Fi Direct on its own Wi-Fi's channel |
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
- **mDNS**, as now, where the network lets multicast through;
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
| input | unreliable; the newest state counts, never queued | UDP | frames, before any media; one waiting at most |
| realtime (sound, video) | unreliable; late is lost | UDP | frames; dropped while the stream's queue is past its budget |
| bulk (a PDF, a file, an update) | a reliable byte stream, last in line | a TCP connection of its own, joined to the session | frames, in what the others leave |

- Datagrams: AES-256-GCM, the nonce the direction and a counter, the
  channel inside the sealed part. Replays: a window of 1024 for realtime;
  input only newer than the newest (an older state must not undo a newer).
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

## Measured

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
