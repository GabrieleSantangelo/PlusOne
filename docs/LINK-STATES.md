# The link's state machines

How a PC and a tablet find each other, connect, stay connected, move
between paths and let go — every case, both sides. docs/LINK.md has the
design; this is its behaviour, precise enough to implement and to test
state by state. What is built today is marked **[built]**; the rest is
the target.

Three machines, nested:

```
 Device (one per paired tablet, on the PC; one per paired PC, on the app)
   └─ Session (at most one per device: keys, channels, reliable messages)
        └─ Path (zero or one active, at most one arriving; each a transport)
```

and beside them, per transport, the **discovery** sources that feed the
Device machine with candidates.

## Words

- **Candidate**: a way to reach a device found by discovery: `(transport,
  address, since, cost)`, e.g. `(usb, accessory 3/27)`, `(lan,
  192.168.178.49:47101)`, `(wifi-direct, peer 56:dd:…)`, `(bluetooth,
  L2CAP psm 0x81 at …)`, `(relay, …)`.
- **Rank** (best first): `usb` 5 · `ap` (the PC's own access point) 4 ·
  `wifi-direct` 4 · `lan` 3 · `bluetooth` 2 · `relay` 1. Ties: the lower
  round trip.
- **Dialer**: the PC, always, except where only the tablet can start a
  transport (none today; Bluetooth may be either). The dialer drives
  reconnection; the other side waits and keeps the session.
- **Want**: whether something uses the device now — a channel open, a
  feature asking (`open`), the user's "connect", or the cable (a cable
  costs nothing on battery: the tablet charges).

## Timers

| Name | Value | What |
|---|---|---|
| `HANDSHAKE` | 5 s a read, 8 s whole | TLS and hellos on a new connection |
| `PATH_SILENCE` | 5 s | nothing heard on a path (stream frames and datagrams alike): the path is lost **[built]** |
| `PATH_SUSPECT` | 1.5 s | nothing heard: look for a better/other path already, keep this one |
| `UDP_PROOF` | 3 s | a pong this recent: datagrams go both ways **[built]** |
| `RESUME_WAIT` | 15 s; 60 s with a channel open | a session with no path this long is over (60 s: time to move both devices to another network by hand) **[built]** |
| `IDLE` | 60 s | a session with no channel, not wanted, not by cable: closed **[built]** |
| `DISCOVERY_ROUND` | 3 s | one round of the cascade |
| `BACKOFF` | 1, 2, 4, … 30 s | between failed rounds while wanted |
| `UPGRADE_LOOK` | 30 s | while on a path below the best rank seen, look again for better |
| `FLAP_HOLD` | 60 s | a path lost twice in this long is not taken again before it (unless it is the only one) |

## 1. Discovery (per device, while wanted)

Methods run as a **cascade with overlap**: each starts at its offset,
the earlier ones keep running; every candidate found goes to the Device
machine at once (it does not wait for the round to end).

| Offset | Method | Finds | Needs | Notes |
|---|---|---|---|---|
| 0 | the cable | `usb` | an Android device on the bus; its serial remembered, or the user's "use the cable" | PC: sysfs/udev; the app: the accessory intent **[built]** |
| 0 | last addresses | `lan`, `ap` | the address it was reached at (per network) | tried directly, 1.5 s **[built]** |
| 0 | mDNS | `lan` | multicast | one query, three repeats (0, 0.25, 1 s) **[built]** |
| 0.3 s | Bluetooth LE | the tablet's rotating id, its IP:port, its paths | Bluetooth on | passive scan on the PC; the app advertises while it has a session or wants one |
| 0.5 s | Wi-Fi Direct peers | `wifi-direct` | P2P on both, not refused by the firmware's channel rules | only if no `lan` candidate within 1 s (it disturbs the Wi-Fi) |
| 1 s | the PC's access point | `ap` | a second adapter (or one allowing AP+STA on different channels) | the app joins a local-only network (Android asks once) |
| 2 s | relay | `relay` | an account; the internet | last resort |

Discovery stops when the Device is **Connected on the best rank it can
have** (the cable present → usb; else the best seen this round), or when
nothing wants the device. While Connected below the best rank, a light
look (cable, mDNS, BLE) every `UPGRADE_LOOK`.

The tablet's side of discovery is announcing: mDNS (while its service
runs) **[built]**, BLE advertising (while wanted or a session is up),
listening on a stable port (47101, else any) **[built]**.

## 2. Device machine (PC side; the app's mirrors it, without dialing)

```
            pair ──────────────► Unpaired ◄──────── forget (any state)
                                    │ paired
                                    ▼
   ┌──────────────────────────► Idle ◄────────────────────────────┐
   │ (no want, no cable)          │ want / cable                   │
   │                              ▼                                │
   │                         Discovering ──── round fails ──► Backoff
   │                              │ candidate                  │  timer / new
   │                              ▼                            │  network / want
   │                          Connecting ◄────────────────────┘
   │        all fail / timeout │      │ session made (new or resumed)
   │   ◄── (want gone) ────────┘      ▼
   │                             Connected ── better candidate ──► Upgrading
   │                              │  ▲   ▲                            │
   │      path lost (session      │  │   └─── moved / not better ─────┘
   │      waits RESUME_WAIT) ─────▼  │
   │                          Reconnecting ── session over ──► Discovering
   │                              │                               (if wanted)
   └──── IDLE elapsed / closed ───┘
```

| State | Entered on | Does | Leaves on → to |
|---|---|---|---|
| **Unpaired** | start, or forget | nothing; refuses its connections (TLS pinning) | the user pairs → Idle |
| **Idle** | paired; session closed while not wanted | nothing at all: no radio, no timer (power) | want, or the cable appears → Discovering |
| **Discovering** | want/cable; Backoff's timer; a network change | the cascade (§1) | a candidate → Connecting; round over, none → Backoff; want gone → Idle |
| **Connecting** | a candidate | dial the best candidates, **one at a time per transport, best rank first, the next after 300 ms** (happy eyeballs); `HANDSHAKE` each | a session (new or resumed) → Connected; all failed → Backoff; a better candidate while dialing → dial it too |
| **Connected** | a session on a path | the session runs (§3); remember the address per network | path lost → Reconnecting; better candidate → Upgrading; idle `IDLE` → Idle (close); forget → Unpaired (close) |
| **Upgrading** | a candidate ranked above the path now | dial it as a path for the session (resume) | taken (`moved`) → Connected; refused/failed → Connected (and `FLAP_HOLD` it) |
| **Reconnecting** | the session's path lost | the session waits; discovery in cascade at once, no backoff for `RESUME_WAIT` | a path → Connected (`moved`); `RESUME_WAIT` over → Discovering if wanted (session over: features see their channels end), else Idle |
| **Backoff** | a round with nothing | wait `BACKOFF` (doubling to 30 s) | timer, a network change, the cable, a new want → Discovering; want gone → Idle |

Guards everywhere: the peer's certificate is the paired one (else the
connection is closed, the Device untouched); the app's protocol is one we
speak (else `Incompatible`: the user is told to update; updates still go
over the old `update` request).

## 3. Session machine (both sides) **[mostly built]**

```
  New ──hellos──► Active(path) ──path lost──► Detached ──path──► Active(path')
                    │    ▲                       │
                    │    └── better path ────────┤ (from Active too: Moving)
                    │                            │ RESUME_WAIT
                 bye / close / forget            ▼
                    └──────────────────────────► Over
```

| State | Does | Leaves on → to |
|---|---|---|
| **New** | TLS 1.3, both certificates pinned; hellos: names, features, UDP port, the session id we have with this device (`resume`) | both hellos name the same id → that session's **Moving**/**Active** (a path for it); else a new session → **Active**, and the one before (if any) **Over** **[built]** |
| **Active(p)** | channels flow; reliable frames numbered, kept until acknowledged (pings carry the ack) **[built]**; pings each second, on the stream and on UDP **[built]** | `PATH_SILENCE` or the stream's end → **Detached** **[built]**; a better path adopted → **Moving** **[built]**; `bye` sent or heard → **Over** **[built]** |
| **Moving(p→q)** | q prepared; what p had not delivered sent again on q first, duplicates dropped by number **[built]**; stale datagrams dropped; the features told `moved` (the desktop's decoder asks a keyframe by itself) | → **Active(q)**; p closed |
| **Detached** | no path; reliable frames queue; realtime and input are dropped (counted) **[built]**; the dialer reconnects (Device: Reconnecting) **[built]** | a path → **Active**; `RESUME_WAIT` → **Over** **[built]** |
| **Over** | every channel ends (its feature reads 0), the other side told if it can be (`bye`) **[built]** | — |

Invariants:
- One session per device, one active path per session.
- Reliable frames reach the feature once each, in order, across any
  number of path changes.
- A path is ranked when adopted; a worse path never replaces a working
  better one **[built]**.
- Realtime never waits behind reliable data on a UDP path; on a stream
  path it is dropped past `STREAM_BUDGET` rather than queued **[built]**.

## 4. Path machine (per transport, under the session)

```
  Dialing ──► Handshaking ──► Probing ──► Up ──► Suspect ──► Lost
                                │         ▲  │      │  ▲
                                │         └──┼──────┘  │ heard again
                                ▼            ▼         │
                          Up(stream-only)  Lost ◄──────┘
```

| State | Does | Leaves on → to |
|---|---|---|
| **Dialing** | TCP connect / AOA switch and READY/GO **[built]** / L2CAP connect / Wi-Fi Direct group | connected → Handshaking; failed or timeout → (Device decides: next candidate) |
| **Handshaking** | TLS and hellos (`HANDSHAKE`) | done → Probing |
| **Probing** | UDP pings every 200 ms while not proven **[built]**; datagrams go on the stream meanwhile **[built]** | a pong → Up (datagrams on UDP); `UDP_PROOF` without → Up(stream-only) (a network dropping UDP still works) **[built]** |
| **Up** | pings each second **[built]** | nothing heard `PATH_SUSPECT` → Suspect; UDP pongs stop `UDP_PROOF` → Up(stream-only) **[built]** |
| **Suspect** | still used; the Device looks for another path at once (cable, last address on another network, mDNS) | heard again → Up; `PATH_SILENCE` → Lost; a better/working path adopted → (the session moves) |
| **Lost** | closed; the transport's resources freed (the USB interface released, the group removed) **[built for usb]** | — |

## 5. The app's side

The app never dials (it cannot know the PC's network position better than
the PC), except the cable, where the accessory arrives from Android:

| State | Does | Leaves on → to |
|---|---|---|
| **Stopped** | service not running | the app opened, a boot with "always ready", the cable → Listening |
| **Listening** | listens on 47101 (else any), announces mDNS (and BLE while wanted); holds no wake lock | a connection → (Session New); the cable's accessory → Cable; idle 10 min with "always ready" off and no channel → Stopped **[built]** |
| **Cable** | opens the accessory, READY+number every 2 s until GO+number, then TLS **[built]**; tries again while the cable is plugged in **[built]** | session made/resumed → Listening (the session runs); the cable out → Listening |

On the app a Detached session also waits `RESUME_WAIT` and takes the
PC's resumed path; with the screen off it holds a partial wake lock only
while a channel is open (the desktop, the speakers), so its pings go on.

## 6. Cases

| # | Case | What happens |
|---|---|---|
| 1 | The cable pulled during a session | the USB path's reads fail at once → Detached → the PC dials the last LAN address and mDNS at once → resumed on `lan` in ~1-3 s; reliable messages resent; video asks a keyframe **[built; measured in the switch test]** |
| 2 | The cable plugged in during a Wi-Fi session | the accessory appears → AOA switch (if remembered) → READY/GO → TLS → `resume` → better rank → Moving → on `usb` **[built]** |
| 3 | Android's "open Plus One?" not answered | the app has no accessory: the PC waits for READY with the cable present (no timeout, no data sent) **[built]**; the Wi-Fi session goes on meanwhile |
| 4 | Wi-Fi lost, no cable | `PATH_SUSPECT` → look; `PATH_SILENCE` → Detached → reconnect attempts every second for `RESUME_WAIT`, then Backoff; features end after 15 s, come back with the next session |
| 5 | Wi-Fi roams (same network, new AP/IP) | the UDP peer's address follows authenticated packets **[built]**; the TCP stream dies → Detached → the PC dials the new address (mDNS) → resumed |
| 6 | The PC changes network (eduroam, hotspot) | NetworkManager's change → Device: Discovering at once (no backoff); last address of *that* network first |
| 7 | Multicast blocked (eduroam) | mDNS finds nothing; last address, BLE (its IP and port), the cable, Wi-Fi Direct follow in the cascade |
| 8 | Clients isolated (eduroam, guest Wi-Fi) | `lan` candidates fail to connect within 1.5 s → the cascade's next: the cable, Wi-Fi Direct/the PC's AP, Bluetooth |
| 9 | UDP blocked, TCP works | Probing never proves UDP → Up(stream-only): everything on the stream **[built]** |
| 10 | A half-dead Wi-Fi (bursts of loss) | Suspect ↔ Up; a lost path only after `PATH_SILENCE`; `FLAP_HOLD` stops ping-ponging between two flaky paths |
| 11 | The tablet's app restarted | its session forgotten: the PC's resume names an id the app does not have → a new session; the PC's old one Over (features see the end and reopen) **[built]** |
| 12 | The PC's plus-one-link restarted | same, the other way: the app's Detached session ends after `RESUME_WAIT`, or at once when the new hello names no session **[built]** |
| 13 | The tablet asleep (doze, screen off) | no wake lock while idle: pings stop → Lost → Detached → Over after 15 s: right, nothing is in use; while a channel is open the app holds a partial wake lock (to do) |
| 14 | The PC suspended | its sessions' paths die; on resume (logind's PrepareForSleep false) → Discovering at once |
| 15 | The user forgets the device on either side | Unpaired: its sessions Over at once (`check_paired`) **[built]**, its connections refused by pinning |
| 16 | Another device answers at the address | the certificate is not the expected one → closed, next candidate **[built: "answered there"]** |
| 17 | Two paths succeed at once (race) | dialing is one at a time per device (`finding` lock) **[built]**; an extra path adopted only if better, else closed **[built]** |
| 18 | Version mismatch | the hello's features say what the other has; a missing feature is refused per channel ("not served"), not the session **[built]**; an incompatible link protocol → the user told to update |
| 19 | Many unauthenticated connections (an attacker on the Wi-Fi) | 8 handshakes at once at most, 8 s each **[built]** |
| 20 | The Wi-Fi Direct firmware bug (AX211: same channel as the PC's Wi-Fi) | the candidate is skipped when the PC's own Wi-Fi is on the group's channel; a crash marks the transport unusable for the boot |
| 21 | The tablet charging by cable, nothing in use | the cable session stays (cheap: no radio woken) **[built]**; Wi-Fi sessions close after `IDLE` **[built]** |
| 22 | The phone hotspot as a repeater (100+ ms spikes) | Suspect is not Lost; the desktop's rate control lowers the bitrate; Upgrading looks for the cable or Wi-Fi Direct |

## Testing it

Each transition has a test with a simulated transport (a socket pair
for a stream path, a lossy UDP relay for impairment): the cable pulled
(`a_session_goes_on_over_another_path`) and a better path taking over
(`a_better_path_takes_over`) are in link/core/tests **[built]**; the
others follow the table, one test per row.
