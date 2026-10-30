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
and keeps the others it has ready (power: a Bluetooth link kept up for
nothing costs both batteries). With both sides' hellos saying
`"paths": 1`, a session keeps up to three paths, one of each kind: the
carrier, and reserves pinged once a second, nothing else. A better path
showing up (the cable plugged in) is a reserve first, the carrier once it
was heard throughout 2 s, the one before a reserve (make before break);
the carrier going (the Wi-Fi gone, or silent 1.5 s) hands over to the
best reserve at once, no detaching; a kind that failed or went silent
carrying does not carry again for 60 s while another is left. Each side
picks its carrier alone and reads every path. Each path's round trip is
measured all along (its pings).

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
- **Liveness in one place**: a ping each second on each path; 1.5 s with
  nothing heard and the path is suspect (the carrier's place goes to a
  reserve heard lately), 5 s and it is lost; no path left and the session
  waits 15 s for another (60 s with a channel open), then is over, for
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
  from plusone_link import Link, REALTIME, VIDEO
  link = Link()                                   # plus-one-link's socket
  tablet = link.devices()[0]
  video = link.open(tablet, "display", "video", REALTIME)
  video.send(packet); data = video.recv(timeout=0.1); video.fileno()
  frames = link.open(tablet, "display", "frames", VIDEO)  # groups (q3)
  frames.send_in_group(number, keyframe, last, piece)     # a frame's piece
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
| QUIC's packets as video (`dscp.rs`) | DSCP AF41 on all of an endpoint's packets from the first realtime or input either way (sent, or received and authenticated) until 10 s without any, QUIC's ECN bits kept; DSCP 0 otherwise | chosen: as the UDP path's datagrams (RFC 8325: WMM's video queue); quinn tells the socket nothing of what a packet carries, so the bulk goes as video too meanwhile (within the controller's rate); quinn-udp's own sends were always DSCP 0 (its ECN control message overrides the socket's IP_TOS). Off only after 10 s: a packet newly marked may pass unmarked ones still in the best effort queue, which quinn takes for losses (3 out of order) and its controller for congestion, so the switch on must be rare (not at each pen stroke); 10 s is iwlmvm's window too (over 100 VI/VO frames in 10 s) |
| TCP bulk pace (`pace.rs`) | 8 ms of queue, 32 KiB/s at least | measured on a hotspot; the floor lowered on an emulated 3 Mbit/s Wi-Fi |
| Budget told (`cc.rs` `PUBLISH_CHANGE`, `PUBLISH_EVERY`) | past 10 % of the last told; going up at most every 100 ms, a cut at once; `{"event": "budget"}` to the apps, null on a path with no controller of its own | the design's (architecture §3.11); a condition variable, not a poll: nothing wakes while it holds |
| Bluetooth's budget (`session.rs` `Drain`, `BT_PRIOR`, `DRAIN_SPAN`) | 0.9 Mbit/s until its stream is measured full; then what it drained over the last second of 200 ms windows one after the other, each full from start to end with more waiting all along (the socket's room filling is no rate), a window ended only with the socket full or nothing waiting; never more than that, told as above | chosen: RFCOMM carries 1-2 Mbit/s; under it the speakers go in Opus (they sent PCM's 1.7 Mbit/s with no budget); derived: a window alone is off by up to half (a bridge's socket holds 8 to 16 KB when full, 25 KB go in 200 ms at 1 Mbit/s); seen: windows ending in a round that had just emptied TLS's buffer locked onto that phase, nothing measured in 2 s of a full stream |
| A cable's budget (`session.rs` `USB_PRIOR`, `PROBE_AFTER`; `usb.rs` `AOA_SHARE`) | a fifth of the speed its USB link was made at (sysfs; 96 Mbit/s at USB 2's high speed; 80 Mbit/s where it is not known: the app's side) until its stream is measured full, as Bluetooth's; measured under it (the other side reading slowly), 1.5 times higher each 2 s it is not measured again (nor full throughout meanwhile), up to it: a cable has no congestion, its drain is its controller; realtime on a stream with a budget waits up to `REALTIME_WAIT` (150 ms) of what it takes: what its socket took in a window full throughout, or in one it filled at a burst only, its budget if more (1.5 MB on a cable, 256 KiB at least elsewhere) | measured (2026-10-27): USB 2's high speed by accessory 17.5 MB/s one way, 18.7 each way at once (0.29 of 480 Mbit/s); chosen: a fifth, over the desktop's 20 Mbit/s; seen (live, 2026-10-30): with no budget the desktop followed its own reports, a third off at each frame lost, 3-11 Mbit/s on the cable; and the cable's rate taken from what its socket took while a keyframe filled its little room (what went on it, not what it carries) dropped each keyframe's tail: every keyframe the tablet asked for on the cable came after such drops on the PC |
| Screen's bitrate on QUIC (`appdisplay.py`) | 0.85 of the controller's rate, as the budget is told (it read it once a second); down once it no longer fits in 95 % of the budget (less the others'), at once (`HOLD` 0; `PLUSONE_L36_HOLD_MS` holds it, on Intel's encoders only, where a change is a keyframe), up by 15 % at least and 2 s after its last change; a stream starts at 0.85 of the budget, not at the most | chosen; the start: from the most, the first second on a budget under it left out frames (half of them from 20 Mbit/s on 10), then a keyframe to go down; a frame lost asks for a keyframe, it no longer cuts the bitrate (a third each, 20 to 4 Mbit/s in 4 s on a hotspot taking 36); measured: each change of the bitrate is a keyframe (Intel's VA encoders, vah264lpenc and vaapih264enc, start a new sequence), at each cut and at each step up of 5 % before; the hold: live, 61 cuts of 30 % in 10 minutes on a good Wi-Fi, most taken back within the second, 51 bitrate changes, so a second at first; then on the grid's chain (adsl, lossy, spikes, 30 runs each, against the desktop before l36), with keyframes made once they fit: hold 0, frames shown 1.9 points fewer, keyframes 10 fewer in 30 s, the delay's p99 3 ms lower; 500 ms, 3.3 fewer, 13, 6 ms higher; 1 s, 4.8 fewer |
| Frames within the budget (`appdisplay.py` `Allowance`) | a bucket filling at 95 % of the budget less the others', 5 frames deep; a frame is encoded only while it is not in debt | chosen: over the budget (until a change's keyframe goes, a hold, a budget under the least bitrate, keyframes asked faster than the room left pays for them) whole frames are left out, none late; never more than the budget |
| Keyframes forced (`appdisplay.py` `force_keyframe`, `_bitrate`) | not while the last one, whatever made it, still waits to go out (more waits than went after it, 0.5 s at most): the app's ask is left (it asks again), a change of bitrate on Intel's encoders made with the first frame in after it; an ask within 1.5 round trips and the keyframe's own time (the last one's at the budget, if longer than four frames at the bitrate) of the last keyframe sent, or asked for, is left too; an ask within 250 ms (the app's own gap between asks), the keyframe's time out and two round trips of the last one going, the link having dropped realtime since it went (its count, as `devices()`' `dropped`, in the answer to `queued` the desktop asks as the keyframe goes), weighed even when the gap above leaves it unanswered, once: that keyframe lost, the next made at half the bitrate (on Intel's the change is that keyframe; up again 2 s on, as far as its keyframe fits) and alone, the frames after it left out until it is out | chosen; live: 67 of 73 asks within 3 s of a cut, a bitrate change's keyframe and the app's answered one into the same queue; live (2026-10-30, a video starting in a browser): 0 fps for 8 s, an ask every 270 ms, each answered with a keyframe the link cut, none whole; an ask alone is no proof (a frame lost on the air after a keyframe that came whole asks too); on the grid's chain where the link holds half what the desktop takes it to (pathbench `room=2`), keyframes twice the reckoning (10 runs of adsl, hotspot, lossy, spikes): frames shown 75.7 %, 1 s of 30 frozen, 13 keyframes backed off, where before l36 18.2 % (19 s) and l36 as it was 16.3 % (19 s); weighed only past the gap, 66.7 % |
| A frame left out before it is encoded (`appdisplay.py` `QUEUE_MS`) | past 60 ms of the budget waiting (on QUIC, in quinn too), and past what leaves an average frame room under what the link holds (below) | chosen: under the link's 100 ms (`LATE`), past which it drops a frame's packets and the frames after are broken; quinn's were not counted before, and on a hotspot every cut broke frames; the 64 KB at least let in frames the link cut on budgets under 5 Mbit/s |
| Whole frames to the link (`appdisplay.py` `_out`, `_keyframe_in`) | the link holds realtime up to 100 ms of its rate (no more than quinn's 512 KB), taken as 90 % of that of the budget told; an encoded frame that would not fit whole is not sent, nor those after it until a keyframe; a keyframe (asked for, the stream broken, a bitrate change on Intel's) is made only with a frame in when it fits behind what waits, reckoned at the most of the last three the encoder made within 20 s at half its bitrate or more (in average frames at their bitrate, four at least), the frames left out meanwhile if the tablet waits for one; sent only if all of it still fits behind what waits as it goes, never alone past what the link holds but at 800 kbit/s; a bitrate whose keyframe could not fit with nothing waiting goes down first, to the one whose keyframe does (the change its keyframe on Intel's), and a change up only so far | chosen: the link drops late packet by packet, a keyframe's tail, and every frame after it broken; a frame the tablet never had any of, it does not see missing (DisplaySession.kt counts frames that came in part), so those after it are never sent; the reckoning: Intel's encoder made keyframes two or three times four frames as a video started (live), each made where four fitted and cut by the link; on the grid's chain with such keyframes and the bitrate ramping up (pathbench `key=2`, `key=3`, `ramp=6`; 10 runs of adsl, hotspot, lossy, spikes), frames shown 85 % and 77 %, 1 and 2.5 s of 30 frozen, every keyframe but one or two whole, none dropped late, where before l36 28 % and 18 % (16 and 22.5 s frozen) and l36 as it was 39 % and 14 % (7 and 23.5 s); the bitrate 10.7 and 7.4 Mbit/s against l36's 13.6 and 15.9 (much of it cut); with keyframes as reckoned, as before; the last ones only within 20 s and at half the bitrate or more: one at 800 kbit/s weighing 20 frames held the climb after congestion at about 5 Mbit/s on a 20 Mbit/s budget until three newer came (on the grid, keyframes heavier by the square root of 12 Mbit/s over the bitrate, `low=12000`, nothing moved: its budgets never fall that far) |

`PLUSONE_L36=0` runs the desktop as before the hold, the allowance, the
keyframes held back and the whole frames of the table above
(`appdisplay.py` `L36`): the bitrate down at once whenever it no longer
fits, no allowance, every frame encoded sent, keyframes made as asked; a
live A/B without rebuilding, read as each stream starts. Its knobs, each
kept within a safe range: `PLUSONE_L36_HOLD_MS` (0-3000), how long the
budget must stay under the bitrate before it goes down; `PLUSONE_L36_PAY`
(0-30, 0 off), down sooner once the frames left out for it are worth that
many frames at the bitrate; `PLUSONE_L36_DEPTH` (1-5), the allowance's
depth in frames (5 is about all the link holds realtime for at the
budget); `PLUSONE_L36_FITS` (80-100), the percent of the budget less the
others' the video may send, and the bitrate must stay under (100: right
up to the budget told, never past it). Each stream says which in the log
("app display: l36 on, hold 0 ms, pay off, allowance 5 frames at 95 % of
the budget").

`PLUSONE_LINK_CC_TRACE=1` puts what the controller does in the log four
times a second (rate, what is acknowledged, trend and threshold, state,
queue, its cuts by why); `PLUSONE_LINK_CC_RECORD=FILE` writes every event
it gets (each acknowledgement's times, losses), a real network's to replay
in a test.

What a live test follows, always in the logs, a line a second at most:
the link's "realtime dropped over 1.0 s: N late, N for room, in N runs"
(a run: a channel's datagrams dropped one after the other, a frame's tail
or frames); the desktop's every change of bitrate with its cause ("A -> B
kbit/s, as the network takes (the budget …)", from the budget's events
too), and each second with frames lost, not encoded or a keyframe sent
(how many of each).

For a live A/B another controller may govern a side's QUIC paths in its
place (`cc/select.rs`): `PLUSONE_LINK_CC` is `gcc` (the default, the one
above as it is), `gcc-l23` (with the l23 rules: a radio's silence and
its trickle after read as one burst), `copa` (Copa, δ 0.1, the spike's),
`c4` (Huitema's C4, ported from picoquic's c4.c), or quinn's own `bbr`
and `cubic` as references; `PLUSONE_LINK_CC_PARAMS`
(`key=value,...`) tunes it: GCC's `threshold_min`, `threshold_max`,
`threshold_gain`, `k_up`, `k_down`, `overuse_ms`, `overuse_send_time`
(over-use counted in send time, as WebRTC does), `beta`,
`queue_limit_ms`, `loss_cut`, and gcc-l23's `silence_ms`, `held_ms`,
`burst_most_ms`; Copa's `delta`; C4's `delay_max`, `margin`,
`jitter_max`, `loss_base`, `beta`, `push`, `probe`, `recover`,
`min_rate`, `min_win`, `wcap`. Both are read once, as the node starts,
and the log says the controller with all its tunables ("congestion
control: gcc-l23 threshold_min=6 ..."); a name not known is gcc, a key
not known or a value out of its range keeps its default, each said in
the log. Each of them governs every packet of the connection (datagrams
too) under quinn's pacer, tells the budget from its own window or rate
(the window a round trip; BBR's pacing rate if less; C4's nominal rate,
less while it drains), and writes the same
trace (its decreases as cuts: by loss when one came in the second before,
else by trend) and record.

`PLUSONE_LINK_VIDEO_STREAMS=0` (either side's process; on by default)
turns off what q3 added on QUIC: our hellos say neither "groups" nor
"control", so a Video channel's groups go as datagrams and the session's
own messages on its stream alone, both ways (each needs both hellos), and
the transport takes no unidirectional streams; logged once ("video
streams off"). The desktop's frames go in groups: Plus One opens its
media channel as Video (`plusone_link.VIDEO`; Realtime from a
plus-one-link that refuses it) and writes each packet's group head, the
frame's number its group; off QUIC the link takes the heads off and the
tablet gets what it got before. Plus One reads the same switch: with
`PLUSONE_LINK_VIDEO_STREAMS=0` it opens the media channel as Realtime,
its packets exactly as before Video (no head, the payload whole).

`PLUSONE_LINK_DSCP=0` (the process's; on by default) leaves QUIC's
packets unmarked, as before `dscp.rs`: the endpoint on quinn's own socket,
every packet DSCP 0 (the raw UDP path's datagrams stay AF41, as they
always were); logged once ("QUIC unmarked"). It holds for the side whose
process has it: the tablet's app marks its own either way.

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

### Paths taken down under a load

`tools/switchbench.sh SCENARIO` (stage 9's yardstick): the PC and the
tablet in two containers on three Docker networks, lan (40 Mbit/s, 4 ms),
p2p (100 Mbit/s, 1 ms) and bt (1 Mbit/s, 15 ms, carried by socat between
a UNIX socket on each side: a stream path with no IP, as an RFCOMM socket
handed over is, its room about RFCOMM's); `link/core/examples/switchbench`
on each side. The PC's side sends a desktop's video (8 Mbit/s, or 0.85 of
the path's budget: about 430 kbit/s on bt), its sound (5 ms packets, Opus
under 8 Mbit/s of budget) and control messages, the tablet's the pen's
input; the PC's side makes the paths as plus-one-link does (the first that
answers, in order, at once when the session has none and each second
after). Twenty times a path is taken down (every packet lost both ways:
`drop`; the PC's interface down: `link`; dropped, and both ends' TCP
connections destroyed: `kill`, a cable pulled), held down 10 s and brought
back; each side records its events and every frame, packet and message,
and `tools/switchbench-summary.py` gives each cycle's numbers and their
median and worst: how soon the path was suspect, failed, left; the video's
gap (no frame shown: whole and decodable, a keyframe or the frame after
one), frames lost and broken, keyframes; the sound's longest gap (lost,
or more than 150 ms late); input and control messages lost, the latest;
the threads and descriptors of each side at the start and the end. The
scenarios: `wifi` (the path the session is on taken down, the other Wi-Fi
up: lan, then p2p, in turn), `bt` (lan taken down, only bt to fall back
on; lan tried again from bt each 2 s), `flap` (lan down 2 s, up 2 s, ten
times in a row, twice).

At 8bae6a7 (2026-10-30; TCP, as plus-one-link by default), median (worst)
of 20 cycles:

| Scenario | Failed | On another path | Video gap | Frames lost | Sound's gap | Control's latest |
|---|---|---|---|---|---|---|
| wifi, drop | 5.0 s | 5.8 s (6.6) | 5.8 s (6.6) | 348 (394) | 5.8 s (6.6) | 5.8 s (6.6) |
| wifi, link | 5.0 s | 5.0 s (5.1) | 5.1 s (5.1) | 303 (304) | 5.1 s (5.1) | 5.0 s (5.1) |
| wifi, kill | 9 ms | 0.8 s (1.5) | 0.8 s (1.5) | 46 (91) | 0.8 s (1.5) | 0.8 s (1.5) |
| bt, drop | 5.0 s | 6.6 s (6.7) | 6.9 s (7.0) | 396 (400) | 6.6 s (6.7) | 6.7 s (6.7) |
| bt, kill | 9 ms | 1.6 s (1.6) | 1.7 s (1.8) | 96 (98) | 1.6 s (1.6) | 1.6 s (1.6) |
| flap, drop | never | never | 2.1 s (3.1) | 123 (124) | 2.1 s (2.1) | 3.3 s (3.3) |

Every cycle that left its path was detached on both sides (one path at a
time: nothing to go on with until another is made, and none is made before
the session has lost its own), after the 5 s of silence unless the stream
ended (kill); an interface down is not seen. The cycles from lan waited
1.5 s more, the dial of the lan that was down; from p2p, killed, the
session was on lan 34 ms later, a gap of 71 ms. None of the control
messages was lost, the pen's input lost was all of the gap's, a keyframe
or two each cycle; threads and descriptors the same after 20 cycles as at
the start, on both sides. Back on lan from bt, once lan is up: 78 ms
(145), a frame lost and one broken, 20 ms of sound. Flapping, one cycle in
ten took 1 s more to come back (its frames and sound lost meanwhile), and
in one the pen's input that went over TCP while UDP was not proven again
came 1-1.8 s late, handed on after newer input that came by UDP (286
messages older than one before them). How the bench is made:
`tools/switchbench.sh`'s head; each cycle's numbers: `cycles.jsonl` among
its logs (`SWITCHBENCH_OUT`).

With reserves (stage 9's P3, 2026-10-30, at 95bd0ed; bt's at f15d805, the
same handover): the session keeps a path of each kind it is given, the
best the carrier, the others pinged once a second; the bench's dialer
keeps one of each kind of the scenario up while the session has one
(`SWITCHBENCH_RESERVE=1`: plus-one-link does not dial reserves itself
yet). Median (worst) of 20 cycles, 8bae6a7 → reserves:

| Scenario | On another path | Video gap | Frames lost | Sound's gap | Control's latest | Detached |
|---|---|---|---|---|---|---|
| wifi, kill | 0.8 s (1.5) → 9 ms (10) | 0.8 s (1.5) → 20 ms (54) | 46 (91) → 0 (1) | 0.8 s (1.5) → 5 ms (15) | 0.8 s (1.5) → 5 ms (10) | every cycle → never |
| wifi, drop | 5.8 s (6.6) → 1.5 s (1.5) | 5.8 s (6.6) → 1.6 s (1.8) | 348 (394) → 92 (93) | 5.8 s (6.6) → 1.5 s (1.6) | 5.8 s (6.6) → 1.5 s (1.5) | every cycle → never |
| bt, kill | 1.6 s (1.6) → 9 ms (9) | 1.7 s (1.8) → 28 ms (91) | 96 (98) → 0 (1) | 1.6 s (1.6) → 5 ms (15) | 1.6 s (1.6) → 22 ms (30) | every cycle → never |
| flap, drop | never → in 1 cycle of 20 | 2.1 s (3.1) → 17 ms (1.6 s) | 123 (124) → 0 (93) | 2.1 s (2.1) → 0 (1.6 s) | 3.3 s (3.3) → 5 ms (1.5 s) | never → never |

A path killed (its streams ended on both sides), the session is on its
reserve 9 ms later, the time the system takes to tell; the video's gap
54 ms at the 95th percentile on Wi-Fi, 82 ms on Bluetooth, no
"Reconnecting…", no control message lost or late past 30 ms. The frame
lost in some cycles is the one on its way on the network killed: nothing
carried a copy of it (duplication is P5's). A path gone silent (drop) is
left once suspect, 1.5 s (the system's events and a faster suspect are
P4's). Flap here is p2p, the carrier, flapping (`SWITCHBENCH_FLAP=p2p`):
its first outage moves the session to lan, and `FLAP_HOLD` keeps it there
through the 19 others (17 ms, a frame's time: nothing lost); with lan
flapping as the reserve (the default), nothing at all (17 ms, 0 lost).
From bt the session is back on lan once its hold is over, 50 s after lan
came back (60 s after it failed). Threads and descriptors the same at the
start, at most and at the end of every run, on both sides. Moving from bt
back to lan, about 10 ms of the pen's input still on bt's stream (3
messages) is handed on after newer input that came over lan's UDP: input
on a stream has no counter (P5's sealed frames on streams).

The cable (`tools/switchbench.sh cable`, 10 cycles, 2026-10-30): a usb
stream path (400 Mbit/s, the session's end of it with a bridge's little
room) pulled (its streams ended at both ends at once) and plugged back
4 s later, lan up, reserves kept. Each time the session is on the cable
again 2.47 s after it is back (2.49 at worst: HYSTERESIS and its next
ping; with `FLAP_HOLD` on the cable it stayed a reserve 60 s after each
pull, as live); pulled, on lan 9 ms after, the video's gap 38 ms (55 at
worst), half a frame lost (the one on its way), the sound's 7.5 ms. On
the cable once back: no frame lost, broken or late, no sound missing;
the pen's input both ways (2 messages lost at the pull, those on the
killed stream; one handed on after newer each time, as from bt).

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
